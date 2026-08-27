//! The file pane: the virtualised list view over a [`Folder`].
//!
//! The `gpui-component` `DataTable` owns layout, column geometry, scrolling
//! and keyboard row movement. Ferret layers multi-selection on top: the
//! selection lives in [`Folder`] as paths, and rows paint it themselves via
//! an overlay child (so hover cannot wash it out). Pointer and keyboard
//! events arrive through `TableEvent`, with live modifiers read from
//! `window.modifiers()`.
//!
//! Focus follows the real focus chain (`on_focus_in`/`on_focus_out` on the
//! wrapper), which is what switches selection painting between the focused
//! (`--app-selected`) and idle (`--app-selected-idle`) fills — Gate 2.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferret_settings::{SortKey, SortOrder, ViewMode};
use ferret_vfs::{Vfs as _, VfsPath};
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, Sizable as _,
    button::ButtonVariants as _,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};

use crate::actions;
use crate::folder::{Folder, FolderEvent};
use crate::format::{format_date, format_size};
use crate::state::FerretState;
use crate::theme::BrowserPalette;

/// Things the pane needs its ancestors to do — navigation and opening are
/// browser-level concerns because they move history and tabs.
#[derive(Clone, Debug)]
pub enum PaneEvent {
    /// Double-click or Open action on a directory: navigate.
    EnterDirectory(VfsPath),
    /// Double-click or Open action on a file: hand to the OS.
    OpenFile(VfsPath),
    /// Sort or view preferences changed; the browser persists them.
    ViewChanged,
    /// Rows were dropped on a folder (§8). The browser owns the filesystem
    /// work because it owns the error reporting the operation needs.
    DropPaths {
        sources: Vec<VfsPath>,
        dest: VfsPath,
        /// Alt was held: copy even within one volume.
        force_copy: bool,
    },
}

/// What a row drag carries.
///
/// The whole selection, not the row under the cursor: dragging one of five
/// selected files and getting only that one is the behaviour every file
/// manager avoids.
#[derive(Clone, Debug)]
pub struct DraggedPaths {
    pub paths: Vec<VfsPath>,
}

/// The little card that follows the cursor during a drag.
struct DragPreview {
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px(px(8.))
            .py(px(4.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_md()
            .text_size(px(12.))
            .text_color(cx.theme().foreground)
            .child(self.label.clone())
    }
}

/// A drop the pane has not turned into an event yet.
///
/// The delegate's drop listener has no route to the browser, so it leaves the
/// request here and the pane's next frame emits it — the same hand-off the
/// slow-click rename uses.
type DropQueue = Arc<std::sync::Mutex<Option<PendingDrop>>>;

/// A disclosure triangle click waiting to be applied.
///
/// Same hand-off as drops: the delegate renders the row but only the pane can
/// mutate the folder.
type ToggleQueue = Arc<std::sync::Mutex<Option<usize>>>;

struct PendingDrop {
    sources: Vec<VfsPath>,
    dest: VfsPath,
    force_copy: bool,
}

/// Record a drop, rejecting the ones that cannot mean anything.
fn queue_drop(queue: &DropQueue, dragged: &DraggedPaths, dest: VfsPath, window: &Window) {
    let sources = droppable(&dragged.paths, &dest);
    if sources.is_empty() {
        return;
    }
    *queue.lock().unwrap() = Some(PendingDrop {
        sources,
        dest,
        force_copy: window.modifiers().alt,
    });
}

/// Which of `paths` may actually be dropped into `dest`.
///
/// Two rules, both of which would otherwise destroy data or do nothing: a
/// directory cannot be moved into itself or into anything it contains, and an
/// item already in `dest` has nowhere to go.
pub fn droppable(paths: &[VfsPath], dest: &VfsPath) -> Vec<VfsPath> {
    paths
        .iter()
        .filter(|src| *src != dest && !dest.starts_with(src))
        .filter(|src| src.parent().as_ref() != Some(dest))
        .cloned()
        .collect()
}

/// The same paths as a native file drag, for when the pointer leaves the
/// window (§8: "drop targets: … other applications").
///
/// Whether each path is a directory is passed in rather than looked up: the
/// resolver runs as the drag crosses the window edge, and a `stat` there
/// would be a syscall in the middle of a gesture.
fn external_payload(dragged: &DraggedPaths, folder: &Folder) -> Option<gpui::ExternalDragPayload> {
    let entries: Vec<(std::path::PathBuf, bool)> = dragged
        .paths
        .iter()
        .filter_map(|p| {
            let local = p.as_local()?.to_path_buf();
            Some((local, folder.is_dir_like(p).unwrap_or(false)))
        })
        .collect();
    if entries.is_empty() {
        // A remote path has nothing the platform could receive.
        return None;
    }
    Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
        entries,
    )))
}

/// Build the drag payload and its preview for a row.
fn drag_payload(paths: Vec<VfsPath>) -> (DraggedPaths, SharedString) {
    let label = match paths.as_slice() {
        [only] => SharedString::from(only.file_name().unwrap_or("item").to_string()),
        other => SharedString::from(format!("{} items", other.len())),
    };
    (DraggedPaths { paths }, label)
}

pub struct FilePane {
    pub folder: gpui::Entity<Folder>,
    table: gpui::Entity<TableState<ListDelegate>>,
    focus: FocusHandle,
    /// Which rendering the folder uses (§5). Icons and columns share the
    /// same Folder — selection, filter and navigation are view-agnostic.
    view_mode: ViewMode,
    icon_scroll: gpui::UniformListScrollHandle,
    /// Column view: one listing per strict ancestor of the browsed directory,
    /// root-most first. The browsed directory is `folder` and renders as the
    /// last listing column, so selection and mutation stay view-agnostic.
    ancestors: Vec<AncestorColumn>,
    /// Scroll position of the browsed directory's own column.
    column_scroll: gpui::UniformListScrollHandle,
    /// Horizontal scroll of the chain itself.
    column_chain_scroll: gpui::ScrollHandle,
    /// Set when the chain is rebuilt: the next frame scrolls it right so the
    /// browsed directory is what you land on, not the root of the chain.
    /// Cleared afterwards so it never fights a manual sideways scroll.
    anchor_chain_right: bool,
    /// Rubber-band selection in icon view, window coordinates.
    marquee: Option<Marquee>,
    /// Shared with the delegate and the tile renderer for focus-aware fills.
    pane_focused: Arc<AtomicBool>,
    /// Columns the user has turned off, shared with the delegate. Persisted
    /// per folder through `FolderView::hidden_columns`.
    hidden_columns: Arc<std::sync::Mutex<Vec<FileColumn>>>,
    /// Latest known pane width, shared with the delegate so the Name column
    /// can absorb the spare width the way §4 demands.
    pane_width: Arc<AtomicUsize>,
    typeahead: String,
    typeahead_at: std::time::Instant,
    /// The active inline rename, rendered as a small overlay editor.
    renaming: Option<Renaming>,
    /// Drops recorded by row/tile listeners, drained on the next frame.
    drops: DropQueue,
    /// Disclosure clicks, likewise.
    toggles: ToggleQueue,
    /// Measured column widths, shared with the delegate.
    widths: ColumnWidths,
    /// Columns whose edge was double-clicked, awaiting measurement.
    sizing_requests: SizeToContentQueue,
    /// Bumps on anything that should cancel a pending slow-click rename.
    rename_arm: u64,
    /// Set by the slow-click timer; the next frame turns it into a rename,
    /// which is where a `&mut Window` is available to focus the editor.
    pending_rename: Option<VfsPath>,
    /// The empty-space context menu, mounted at the right-click position.
    space_menu: Option<SpaceMenu>,
    /// Pane origin in window coordinates, for positioning the menu.
    pane_origin: Arc<std::sync::Mutex<gpui::Point<gpui::Pixels>>>,
    _subscriptions: Vec<Subscription>,
}

struct Marquee {
    start: gpui::Point<gpui::Pixels>,
    current: gpui::Point<gpui::Pixels>,
}

/// One ancestor listing in column view.
///
/// These are display-and-navigation only: they carry no selection of their
/// own, just the child that lies on the path to the browsed directory, drawn
/// selected so the chain reads as a path.
struct AncestorColumn {
    folder: gpui::Entity<Folder>,
    on_path: VfsPath,
    scroll: gpui::UniformListScrollHandle,
    /// Lives here so clearing the chain also detaches its listing tasks.
    _subscription: Subscription,
}

struct SpaceMenu {
    position: gpui::Point<gpui::Pixels>,
    menu: gpui::Entity<PopupMenu>,
    _dismiss: Subscription,
}

struct Renaming {
    path: VfsPath,
    input: gpui::Entity<InputState>,
}

impl EventEmitter<PaneEvent> for FilePane {}

impl Focusable for FilePane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilePane {
    pub fn new(
        folder: gpui::Entity<Folder>,
        view_mode: ViewMode,
        hidden_columns: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane_focused = Arc::new(AtomicBool::new(false));
        let pane_width = Arc::new(AtomicUsize::new(0));
        let drops: DropQueue = Arc::new(std::sync::Mutex::new(None));
        let toggles: ToggleQueue = Arc::new(std::sync::Mutex::new(None));
        let widths: ColumnWidths = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let sizing_requests: SizeToContentQueue = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hidden_columns = Arc::new(std::sync::Mutex::new(
            hidden_columns
                .iter()
                .filter_map(|key| FileColumn::from_key(key))
                .collect::<Vec<_>>(),
        ));
        let table = cx.new(|cx| {
            let delegate = ListDelegate {
                folder: folder.clone(),
                pane_focused: pane_focused.clone(),
                pane_width: pane_width.clone(),
                hidden_columns: hidden_columns.clone(),
                drops: drops.clone(),
                toggles: toggles.clone(),
                widths: widths.clone(),
                sizing_requests: sizing_requests.clone(),
            };
            TableState::new(delegate, window, cx)
                .col_selectable(false)
                .col_movable(false)
                .row_selectable(true)
        });

        // Keyboard-first: the pane takes focus when it appears, so arrows and
        // type-ahead work before the first click.
        let table_focus = table.read(cx).focus_handle(cx).clone();
        window.focus(&table_focus, cx);

        let mut subscriptions = vec![
            cx.subscribe_in(&table, window, Self::on_table_event),
            cx.subscribe_in(&folder, window, |this, _, event, _window, cx| match event {
                FolderEvent::RowsChanged | FolderEvent::LoadFinished => {
                    if let Some(rename) = &this.renaming
                        && !this.folder.read(cx).has_entry(&rename.path)
                    {
                        this.renaming = None;
                    }
                    this.table.update(cx, |_, cx| cx.notify());
                    cx.notify();
                }
                FolderEvent::SelectionChanged => {
                    this.table.update(cx, |_, cx| cx.notify());
                    cx.notify();
                }
                // Every route to a sort funnels through here, including the
                // column header's, which the delegate owns and cannot reach
                // the browser from.
                FolderEvent::SortChanged => cx.emit(PaneEvent::ViewChanged),
                FolderEvent::Gone => {
                    this.table.update(cx, |_, cx| cx.notify());
                    cx.notify();
                }
            }),
        ];

        let focus = cx.focus_handle();

        // Gate 2: the pane knows when it has focus, which is what switches
        // selection painting between --app-selected and --app-selected-idle.
        let focused_flag = pane_focused.clone();
        subscriptions.push(cx.on_focus_in(&focus, window, move |_, _, cx| {
            focused_flag.store(true, Ordering::Relaxed);
            cx.notify();
        }));
        let idle_flag = pane_focused.clone();
        subscriptions.push(cx.on_blur(&focus, window, move |_, _, cx| {
            idle_flag.store(false, Ordering::Relaxed);
            cx.notify();
        }));

        let mut this = Self {
            folder,
            table,
            focus,
            view_mode,
            pane_focused,
            hidden_columns,
            icon_scroll: gpui::UniformListScrollHandle::new(),
            ancestors: Vec::new(),
            column_scroll: gpui::UniformListScrollHandle::new(),
            column_chain_scroll: gpui::ScrollHandle::new(),
            anchor_chain_right: false,
            marquee: None,
            pane_width,
            pane_origin: Arc::new(std::sync::Mutex::new(gpui::point(px(0.), px(0.)))),
            space_menu: None,
            typeahead: String::new(),
            typeahead_at: std::time::Instant::now(),
            renaming: None,
            drops,
            toggles,
            widths,
            sizing_requests,
            rename_arm: 0,
            pending_rename: None,
            _subscriptions: subscriptions,
        };
        if this.view_mode == ViewMode::Columns {
            this.build_ancestor_columns(cx);
        }
        this
    }

    // ------------------------------------------------------------------
    // Commands the browser dispatches here

    pub fn selected_paths(&self, cx: &App) -> Vec<VfsPath> {
        self.folder.read(cx).selected_paths()
    }

    /// Emit a pane event from the outside (browser-driven Open, filter
    /// Enter, …).
    pub fn emit_event(&self, event: PaneEvent, cx: &mut Context<Self>) {
        cx.emit(event);
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Hidden column keys for the settings file, in a stable order.
    pub fn hidden_column_keys(&self) -> Vec<String> {
        let hidden = self.hidden_columns.lock().unwrap();
        FileColumn::HIDEABLE
            .into_iter()
            .filter(|c| hidden.contains(c))
            .map(|c| c.key().to_string())
            .collect()
    }

    /// Show or hide one column from the header's config menu.
    fn toggle_column(&mut self, column: FileColumn, cx: &mut Context<Self>) {
        {
            let mut hidden = self.hidden_columns.lock().unwrap();
            if let Some(at) = hidden.iter().position(|c| *c == column) {
                hidden.remove(at);
            } else {
                hidden.push(column);
            }
        }
        // `refresh` is the "columns changed" hook: the table caches its column
        // groups, so a plain notify repaints stale geometry — the removed
        // column's slot stays behind and the config slot draws twice.
        self.table.update(cx, |table, cx| {
            table.refresh(cx);
            cx.notify();
        });
        // Rides the same persistence path as sort and view mode.
        cx.emit(PaneEvent::ViewChanged);
        cx.notify();
    }

    pub fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode == mode {
            return;
        }
        self.view_mode = mode;
        self.marquee = None;
        // Ancestor listings only exist to be looked at, so they are walked
        // when column view is entered and dropped when it is left rather than
        // kept warm behind a view the user is not in.
        match mode {
            ViewMode::Columns => self.build_ancestor_columns(cx),
            _ => self.ancestors.clear(),
        }
        cx.notify();
    }

    /// The Open action: what double-click would do to the cursor row.
    pub fn open_current(&self, cx: &App) -> Option<PaneEvent> {
        let path = self.folder.read(cx).cursor_path()?;
        let is_dir = self.folder.read(cx).is_dir_like(&path)?;
        Some(if is_dir {
            PaneEvent::EnterDirectory(path)
        } else {
            PaneEvent::OpenFile(path)
        })
    }

    /// Open every selected item: each file goes to its default application,
    /// and the first directory is entered — one pane can only browse one
    /// directory, so entering more than one would silently discard the rest.
    fn open_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.folder.read(cx).selected_paths();
        if paths.is_empty() {
            if let Some(event) = self.open_current(cx) {
                cx.emit(event);
            }
            return;
        }

        let mut enter = None;
        for path in paths {
            match self.folder.read(cx).is_dir_like(&path) {
                Some(true) => enter.get_or_insert(path),
                _ => {
                    cx.emit(PaneEvent::OpenFile(path));
                    continue;
                }
            };
        }
        if let Some(dir) = enter {
            cx.emit(PaneEvent::EnterDirectory(dir));
        }
    }

    pub fn begin_rename(&mut self, path: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = path.file_name() else {
            return;
        };
        if !self.folder.read(cx).has_entry(&path) || self.renaming.is_some() {
            return;
        }

        let stem_len = stem_length(name);
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value(name);
            // Spec §6: pre-select the stem, not the extension.
            state.set_selected_range(0..stem_len, cx);
            state
        });

        self._subscriptions.push(cx.subscribe_in(
            &input,
            window,
            move |this, input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(input.clone(), window, cx),
                InputEvent::Blur => this.commit_rename(input.clone(), window, cx),
                _ => {}
            },
        ));

        self.renaming = Some(Renaming { path, input });
        cx.notify();
    }

    fn commit_rename(
        &mut self,
        input: gpui::Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let new_name = input.read(cx).value().trim().to_string();
        cx.notify();

        let old_name = renaming.path.file_name().unwrap_or("").to_string();
        if new_name.is_empty() || new_name == old_name {
            return; // Nothing changed: treat as cancel.
        }
        if new_name.contains('/') || new_name.contains('\\') {
            eprintln!("rename: name may not contain separators: {new_name:?}");
            return;
        }

        let parent = renaming
            .path
            .parent()
            .unwrap_or_else(|| renaming.path.clone());
        let dest = parent.join(&new_name);
        let fs = cx.global::<FerretState>().fs;
        let src = renaming.path.clone();
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            if let Err(err) = fs.rename(&src, &dest).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    use gpui_component::{WindowExt as _, notification::Notification};
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
        // The directory watcher repaints the listing once the rename lands.
    }

    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // Event plumbing

    fn on_table_event(
        &mut self,
        _: &gpui::Entity<TableState<ListDelegate>>,
        event: &TableEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = window.modifiers();
        match event {
            TableEvent::SelectRow(row) => {
                let row = *row;
                // §8: clicking the name of a row that was already the lone
                // selection, then pausing, starts a rename. Decided before
                // the click is applied, because applying it makes every row
                // look like it was already selected.
                let was_only_selection = {
                    let folder = self.folder.read(cx);
                    folder.is_row_selected(row) && folder.selection_count() == 1
                };
                let plain = !modifiers.shift && !modifiers.platform && !modifiers.control;
                if was_only_selection && plain {
                    self.arm_slow_click_rename(row, cx);
                } else {
                    self.disarm_slow_click_rename();
                }

                if modifiers.shift {
                    self.folder.update(cx, |f, _| f.extend_select(row));
                } else if modifiers.platform || modifiers.control {
                    self.folder.update(cx, |f, _| f.toggle_select(row));
                } else {
                    self.folder.update(cx, |f, cx| {
                        f.select_only(row);
                        cx.emit(FolderEvent::SelectionChanged);
                    });
                }
                cx.notify();
            }
            TableEvent::DoubleClickedRow(row) => {
                let entry = self.folder.read(cx).entry_at(*row).cloned();
                if let Some(entry) = entry {
                    let ev = if entry.is_dir_like() {
                        PaneEvent::EnterDirectory(entry.path.clone())
                    } else {
                        PaneEvent::OpenFile(entry.path.clone())
                    };
                    cx.emit(ev);
                }
            }
            TableEvent::RightClickedRow(Some(row)) => {
                let row = *row;
                self.folder.update(cx, |f, cx| {
                    f.prepare_context_menu(row);
                    cx.emit(FolderEvent::SelectionChanged);
                });
                cx.notify();
            }
            TableEvent::ClearSelection => {
                self.disarm_slow_click_rename();
                self.folder.update(cx, |f, cx| {
                    f.clear_selection();
                    cx.emit(FolderEvent::SelectionChanged);
                });
                cx.notify();
            }
            _ => {}
        }
    }

    /// Start the countdown to an inline rename, and cancel any previous one.
    ///
    /// The delay has to clear the double-click interval, or opening a file
    /// with two ordinary clicks would leave a rename editor behind it.
    fn arm_slow_click_rename(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(path) = self.folder.read(cx).entry_at(row).map(|e| e.path.clone()) else {
            return;
        };
        self.rename_arm += 1;
        let armed = self.rename_arm;

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SLOW_CLICK_RENAME_DELAY)
                .await;
            let _ = this.update(cx, |this, cx| {
                // Anything at all since the click — a second click, a key, a
                // new selection — has already bumped the generation.
                if this.rename_arm != armed || this.renaming.is_some() {
                    return;
                }
                let still_alone = {
                    let folder = this.folder.read(cx);
                    folder.selection_count() == 1 && folder.has_entry(&path)
                };
                if still_alone {
                    this.pending_rename = Some(path.clone());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn disarm_slow_click_rename(&mut self) {
        self.rename_arm += 1;
        self.pending_rename = None;
    }

    /// Type-ahead: letters jump to the first matching name (§8).
    fn handle_typeahead(&mut self, e: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = e.keystroke.key.as_str();
        let printable = key.len() == 1
            && !e.keystroke.modifiers.control
            && !e.keystroke.modifiers.platform
            && !e.keystroke.modifiers.alt;
        if !printable {
            return;
        }

        let now = std::time::Instant::now();
        if now.duration_since(self.typeahead_at) > std::time::Duration::from_millis(900) {
            self.typeahead.clear();
        }
        self.typeahead_at = now;
        self.typeahead.push_str(key);

        let start = self
            .folder
            .read(cx)
            .cursor_row()
            .map(|r| r + 1)
            .unwrap_or(0);
        let prefix = self.typeahead.clone();
        let matched = self
            .folder
            .update(cx, |f, _| f.select_by_prefix(&prefix, start));

        if matched {
            self.sync_table_cursor(cx);
            self.folder
                .update(cx, |_, cx| cx.emit(FolderEvent::SelectionChanged));
        }
        cx.notify();
    }

    fn sync_table_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.folder.read(cx).cursor_row() {
            self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
        }
    }
}

impl FilePane {
    /// Icon-view geometry (§4): 92 px tiles, 48 px glyph, 6 px gaps, 12 px
    /// padding.
    const TILE_W: f32 = 92.;
    const TILE_GLYPH: f32 = 48.;
    const TILE_H: f32 = 96.;
    const GAP: f32 = 6.;
    const PAD: f32 = 12.;

    fn tiles_per_row(&self) -> usize {
        let width = self.pane_width.load(Ordering::Relaxed) as f32;
        (((width - Self::PAD * 2. + Self::GAP) / (Self::TILE_W + Self::GAP)).floor() as usize)
            .max(1)
    }

    fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = self.folder.update(cx, |f, _| f.move_cursor(delta));
        if let Some(row) = next {
            self.sync_table_cursor(cx);
            self.folder
                .update(cx, |_, cx| cx.emit(FolderEvent::SelectionChanged));
            self.scroll_cursor_into_view(row, cx);
        }
        cx.notify();
    }

    /// Shift + arrow: grow or shrink the range from the anchor (§8).
    fn extend_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = self.folder.update(cx, |f, _| f.extend_cursor(delta));
        if let Some(row) = next {
            self.sync_table_cursor(cx);
            self.folder
                .update(cx, |_, cx| cx.emit(FolderEvent::SelectionChanged));
            self.scroll_cursor_into_view(row, cx);
        }
        cx.notify();
    }

    /// Left/right mean something different in each view: expand or collapse
    /// in list view (§8), one tile in icon view, one column in column view.
    fn move_cursor_horizontal(&mut self, delta: isize, cx: &mut Context<Self>) {
        match self.view_mode {
            ViewMode::List => self.expand_or_collapse(delta > 0, cx),
            ViewMode::Icons => self.move_cursor(delta, cx),
            ViewMode::Columns => self.move_column(delta, cx),
        }
    }

    /// → opens the cursor's folder; ← closes it, or steps out to the parent
    /// row when it is already closed — the movement a tree is expected to do.
    fn expand_or_collapse(&mut self, expand: bool, cx: &mut Context<Self>) {
        let Some(row) = self.folder.read(cx).cursor_row() else {
            return;
        };
        let changed = self.folder.update(cx, |f, cx| {
            if expand {
                f.expand(row, cx)
            } else {
                f.collapse(row, cx)
            }
        });
        if changed {
            cx.notify();
            return;
        }

        // Nothing to collapse: move to the row this one hangs under.
        if !expand {
            let parent_row = {
                let folder = self.folder.read(cx);
                folder
                    .cursor_path()
                    .and_then(|p| p.parent())
                    .and_then(|p| folder.row_of(&p))
            };
            if let Some(parent_row) = parent_row {
                self.folder.update(cx, |f, cx| {
                    f.select_only(parent_row);
                    cx.emit(FolderEvent::SelectionChanged);
                });
                self.sync_table_cursor(cx);
                cx.notify();
            }
        }
    }

    /// Right descends into the cursor row when it is a directory; left goes
    /// back to the parent — the column chain's own axis.
    fn move_column(&mut self, delta: isize, cx: &mut Context<Self>) {
        if delta < 0 {
            if let Some(parent) = self.folder.read(cx).dir.parent() {
                cx.emit(PaneEvent::EnterDirectory(parent));
            }
            return;
        }
        let folder = self.folder.read(cx);
        let Some(path) = folder.cursor_path() else {
            return;
        };
        if folder.is_dir_like(&path) == Some(true) {
            cx.emit(PaneEvent::EnterDirectory(path));
        }
    }

    fn scroll_cursor_into_view(&mut self, row: usize, _cx: &mut Context<Self>) {
        match self.view_mode {
            ViewMode::Icons => {
                let per_row = self.tiles_per_row();
                self.icon_scroll
                    .scroll_to_item(row / per_row, gpui::ScrollStrategy::Nearest);
            }
            ViewMode::Columns => self
                .column_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Nearest),
            ViewMode::List => {}
        }
    }

    fn apply_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        let (current_key, current_order) = self
            .folder
            .read_with(cx, |f, _| (f.sort_key(), f.sort_order()));
        // Picking the sorted column's menu entry flips direction, matching
        // the header behaviour.
        let order = if current_key == key {
            current_order
        } else {
            SortOrder::Ascending
        };
        self.folder.update(cx, |f, cx| {
            f.set_sort(key, order);
            cx.emit(FolderEvent::SortChanged);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        self.table.update(cx, |_, cx| cx.notify());
    }

    fn apply_order(&mut self, order: SortOrder, cx: &mut Context<Self>) {
        self.folder.update(cx, |f, cx| {
            f.set_sort(f.sort_key(), order);
            cx.emit(FolderEvent::SortChanged);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        self.table.update(cx, |_, cx| cx.notify());
    }

    /// Right-click on empty space opens the pane menu: create, paste, sort,
    /// toggles. Rows and tiles claim the event first, in the bubble phase, so
    /// this only has to notice that they did.
    fn open_space_menu(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // List view routes row hits through the table's own menu.
        if self.view_mode == ViewMode::List && self.table.read(cx).right_clicked_row().is_some() {
            return;
        }
        // Icon tiles and column rows open their menu on the way out to here;
        // a menu already placed at this event's position is theirs.
        let origin = *self.pane_origin.lock().unwrap();
        if self
            .space_menu
            .as_ref()
            .is_some_and(|menu| menu.position == event.position - origin)
        {
            return;
        }
        self.open_menu_at(None, event, window, cx);
    }

    /// The pane menu, optionally aimed at one entry (icon-view tiles).
    fn open_menu_at(
        &mut self,
        target: Option<VfsPath>,
        event: &gpui::MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focus = gpui::Focusable::focus_handle(self.table.read(cx), cx);
        let (key, order, show_hidden, selected) = self.folder.read_with(cx, |f, _| {
            (
                f.sort_key(),
                f.sort_order(),
                f.show_hidden(),
                f.selection_count(),
            )
        });

        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            use gpui_component::menu::PopupMenuItem;
            let menu = menu.action_context(focus);
            let menu = if target.is_some() {
                entry_menu_items(menu, selected).separator()
            } else {
                menu
            };
            menu.item(PopupMenuItem::new("New Folder").action(Box::new(actions::NewFolder)))
                .item(PopupMenuItem::new("New File").action(Box::new(actions::NewFile)))
                .separator()
                .item(PopupMenuItem::new("Paste").action(Box::new(actions::Paste)))
                .separator()
                .submenu("Sort By", window, cx, move |sort_menu, _, _| {
                    sort_menu
                        .menu_with_check(
                            "Name",
                            key == SortKey::Name,
                            std::boxed::Box::new(actions::SortByName),
                        )
                        .menu_with_check(
                            "Date Modified",
                            key == SortKey::Modified,
                            std::boxed::Box::new(actions::SortByModified),
                        )
                        .menu_with_check(
                            "Size",
                            key == SortKey::Size,
                            std::boxed::Box::new(actions::SortBySize),
                        )
                        .separator()
                        .menu_with_check(
                            "Ascending",
                            order == SortOrder::Ascending,
                            std::boxed::Box::new(actions::SortAscending),
                        )
                        .menu_with_check(
                            "Descending",
                            order == SortOrder::Descending,
                            std::boxed::Box::new(actions::SortDescending),
                        )
                })
                .separator()
                .menu_with_check(
                    "Show Hidden Files",
                    show_hidden,
                    std::boxed::Box::new(actions::ToggleHidden),
                )
                .item(PopupMenuItem::new("Refresh").action(std::boxed::Box::new(actions::Refresh)))
        });

        let dismiss = cx.subscribe(
            &menu,
            |this: &mut Self,
             _: gpui::Entity<PopupMenu>,
             _: &DismissEvent,
             cx: &mut Context<Self>| {
                if this.space_menu.take().is_some() {
                    cx.notify();
                }
            },
        );

        let origin = *self.pane_origin.lock().unwrap();
        self.space_menu = Some(SpaceMenu {
            position: event.position - origin,
            menu,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    fn space_menu_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(space_menu) = &self.space_menu else {
            return div().into_any_element();
        };
        // A full-pane click catcher closes the menu on any click; the menu
        // itself sits above it at the cursor.
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(space_menu.position.x)
                    .top(space_menu.position.y)
                    .child(space_menu.menu.clone()),
            )
            .into_any_element()
    }
}

impl Render for FilePane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The slow-click timer has no window to focus an editor with, so it
        // leaves the request here and the next frame starts the rename.
        if let Some(path) = self.pending_rename.take() {
            self.begin_rename(path, window, cx);
        }
        // Drops arrive from row listeners that cannot reach the browser; this
        // is where they become an event it can act on.
        if let Some(row) = self.toggles.lock().unwrap().take() {
            self.folder.update(cx, |f, cx| f.toggle_expanded(row, cx));
        }
        let sizing: Vec<FileColumn> = std::mem::take(&mut self.sizing_requests.lock().unwrap());
        for column in sizing {
            self.size_column_to_content(column, window, cx);
        }
        if let Some(drop) = self.drops.lock().unwrap().take() {
            cx.emit(PaneEvent::DropPaths {
                sources: drop.sources,
                dest: drop.dest,
                force_copy: drop.force_copy,
            });
        }

        let rename_overlay = self.render_rename_overlay(cx);

        let width_probe = self.pane_width.clone();
        let width_probe_owner = cx.weak_entity();
        // `size_full` is load-bearing: a GPUI div is `display: block`, so
        // without it this wrapper shrinks to its content, the table gets no
        // definite height, and a virtualised list with no height paints no
        // rows at all — silently, and only at runtime.
        let table = div()
            .size_full()
            .on_children_prepainted(move |children: Vec<gpui::Bounds<gpui::Pixels>>, _, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let w = f32::from(bounds.size.width) as usize;
                if width_probe.swap(w, Ordering::Relaxed) != w
                    && let Some(pane) = width_probe_owner.upgrade()
                {
                    pane.update(cx, |pane, cx| pane.table.update(cx, |_, cx| cx.notify()));
                }
            })
            .child(
                DataTable::new(&self.table)
                    .bordered(false)
                    .with_size(px(ROW_HEIGHT)),
            );

        // Menu placement and marquee hit-testing work in pane-local space,
        // but mouse events arrive in window space; this is the offset between
        // them, and it is only knowable after layout.
        let origin = self.pane_origin.clone();

        // Empty space in the pane is the browsed directory itself, so items
        // dragged in from another window or tab land here.
        let pane_dir = self.folder.read(cx).dir.clone();
        let pane_drops = self.drops.clone();

        div()
            .id("file-pane")
            .relative()
            .flex_1()
            .min_h_0()
            .key_context(actions::context::FILE_PANE)
            .track_focus(&self.focus)
            .on_drop(move |dragged: &DraggedPaths, window, _| {
                queue_drop(&pane_drops, dragged, pane_dir.clone(), window);
            })
            .on_prepaint(move |bounds, _, _| {
                *origin.lock().unwrap() = bounds.origin;
            })
            .on_key_down(cx.listener(Self::handle_typeahead))
            .on_action(cx.listener(|this, _: &actions::Open, _, cx| {
                this.open_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &actions::Rename, window, cx| {
                let target = this.folder.read(cx).cursor_path();
                if let Some(path) = target {
                    this.begin_rename(path, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &actions::CancelEdit, _, cx| {
                this.cancel_rename(cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SelectAll, _, cx| {
                this.folder.update(cx, |f, cx| {
                    f.select_all();
                    cx.emit(FolderEvent::SelectionChanged);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &actions::SortByName, _, cx| {
                this.apply_sort(SortKey::Name, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortByModified, _, cx| {
                this.apply_sort(SortKey::Modified, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortBySize, _, cx| {
                this.apply_sort(SortKey::Size, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortAscending, _, cx| {
                this.apply_order(SortOrder::Ascending, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortDescending, _, cx| {
                this.apply_order(SortOrder::Descending, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorUp, _, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorDown, _, cx| {
                this.move_cursor(1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleColumnModified, _, cx| {
                    this.toggle_column(FileColumn::Modified, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleColumnSize, _, cx| {
                this.toggle_column(FileColumn::Size, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ExtendUp, _, cx| {
                this.extend_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ExtendDown, _, cx| {
                this.extend_cursor(1, cx);
            }))
            // Cmd↓ / Alt↓ descends into the cursor row when it is a folder,
            // the mirror of Cmd↑ going to the parent (§8).
            .on_action(cx.listener(|this, _: &actions::EnterDirectory, _, cx| {
                let folder = this.folder.read(cx);
                if let Some(path) = folder.cursor_path()
                    && folder.is_dir_like(&path) == Some(true)
                {
                    cx.emit(PaneEvent::EnterDirectory(path));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::CursorLeft, _, cx| {
                this.move_cursor_horizontal(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorRight, _, cx| {
                this.move_cursor_horizontal(1, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_space_menu(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    // A left click between right-clicks clears the table's
                    // stale row target, so an empty-area right-click can't
                    // replay an old row menu.
                    this.table
                        .update(cx, |t, cx| t.set_right_clicked_row(None, cx));
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                    if this.view_mode == ViewMode::List {
                        this.start_list_marquee(event, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.view_mode == ViewMode::List {
                    this.update_marquee(event, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.marquee.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .when(self.view_mode == ViewMode::List, |el| {
                el.child(table)
                    .children(self.render_loading_bar(cx))
                    .child(self.marquee_overlay())
            })
            .when(self.view_mode == ViewMode::Icons, |el| {
                el.child(self.render_icon_view(cx))
            })
            .when(self.view_mode == ViewMode::Columns, |el| {
                el.child(self.render_column_view(cx))
            })
            .when_some(rename_overlay, |el, overlay| el.child(overlay))
            .when_some(
                self.space_menu_overlay(cx).into_any_element().into(),
                |el, overlay| el.child(overlay),
            )
            .when(self.view_mode == ViewMode::Icons, |el| {
                let overlay = self.marquee_overlay();
                el.child(overlay)
            })
    }
}

/// Column header height (§4), and where the loading hint sits.
const COLUMN_HEADER_HEIGHT: f32 = 26.;

/// How long a click on an already-selected name waits before becoming a
/// rename (§8: "click then pause, or slow second click").
///
/// Must clear the platform double-click interval — 500 ms on both targets —
/// or opening a file with two ordinary clicks would leave a rename editor
/// open behind it.
const SLOW_CLICK_RENAME_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

impl FilePane {
    /// The rename editor, sat on the row it renames (§6).
    ///
    /// In list view the row's position is computable — fixed row height, a
    /// known header, and the table's scroll offset — so the editor lands on
    /// the name it is editing. The other two views have no row geometry to
    /// borrow, so there it falls back to a panel at the foot of the pane.
    fn render_rename_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let renaming = self.renaming.as_ref()?;
        let theme = cx.theme();
        let editor = Input::new(&renaming.input).small();

        let row_top = (self.view_mode == ViewMode::List)
            .then(|| self.folder.read(cx).row_of(&renaming.path))
            .flatten()
            .map(|row| {
                let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
                    &self.table.read(cx).vertical_scroll_handle,
                );
                px(COLUMN_HEADER_HEIGHT + row as f32 * ROW_HEIGHT) + scrolled.y
            })
            // Only when the row is actually on screen; a rename started and
            // then scrolled away should not paint over the header.
            .filter(|top| *top >= px(COLUMN_HEADER_HEIGHT));

        let frame = h_flex()
            .absolute()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.primary)
            .bg(theme.background)
            .shadow_md();

        Some(match row_top {
            Some(top) => frame
                .top(top - px(3.))
                .left(px(28.))
                .w(px(NAME_MIN_WIDTH))
                .child(editor)
                .into_any_element(),
            None => frame
                .left(px(12.))
                .bottom(px(12.))
                .w(px(320.))
                .child(editor)
                .into_any_element(),
        })
    }

    /// Measure a column's widest rendered value and adopt that width (§8).
    ///
    /// Measured with the real text system rather than estimated from
    /// character counts: proportional digits and non-Latin names make any
    /// estimate wrong, and a "size to content" that clips its content is
    /// worse than none.
    fn size_column_to_content(
        &mut self,
        column: FileColumn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let font = window.text_style().font();
        let font_size = px(ROW_FONT_SIZE);
        let text_system = window.text_system();

        let mut widest = px(0.);
        // The header's own label sets the floor — a column narrower than its
        // title has not been sized to its content either.
        let mut measure = |text: &str| {
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let layout = text_system.layout_line(text, font_size, &[run], None);
            if layout.width > widest {
                widest = layout.width;
            }
        };
        measure(column.label());

        {
            let folder = self.folder.read(cx);
            for row in 0..folder.row_count() {
                let Some(entry) = folder.entry_at(row) else {
                    continue;
                };
                match column {
                    FileColumn::Modified => {
                        if let Some(modified) = entry.metadata.and_then(|m| m.modified) {
                            measure(&format_date(modified));
                        }
                    }
                    FileColumn::Size => match entry.display_len() {
                        Some(len) => measure(&format_size(len)),
                        None if entry.is_dir_like() => measure("—"),
                        None => {}
                    },
                    // Name absorbs spare width by design (§4) and the config
                    // slot is fixed, so neither is sized to content.
                    FileColumn::Name | FileColumn::Config => return,
                }
            }
        }

        // Cell padding, plus room for the sort caret the header draws.
        const CHROME: f32 = 28.;
        let measured = (f32::from(widest) + CHROME).ceil();
        self.widths.lock().unwrap().insert(column, measured);
        self.table.update(cx, |table, cx| {
            table.refresh(cx);
            cx.notify();
        });
        cx.emit(PaneEvent::ViewChanged);
    }

    /// §9: a subtle indeterminate bar along the bottom of the column header,
    /// and only once a listing has gone ~200 ms without producing a row.
    ///
    /// Returns nothing the rest of the time, which is most of the time — a
    /// local directory lands far inside the delay.
    fn render_loading_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.folder.read(cx).is_slow_to_start() {
            return None;
        }
        Some(
            div()
                .absolute()
                .top(px(COLUMN_HEADER_HEIGHT - 2.))
                .left_0()
                .right_0()
                .h(px(2.))
                .child(
                    gpui_component::progress::Progress::new("listing-progress")
                        .loading(true)
                        .color(cx.theme().primary),
                )
                .into_any_element(),
        )
    }
}

// ---------------------------------------------------------------------------
// Icon view

impl FilePane {
    fn render_icon_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.folder.read(cx).row_count();
        let rows = n.div_ceil(self.tiles_per_row());

        div()
            .id("icon-area")
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.start_marquee(event, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                this.update_marquee(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.marquee.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::uniform_list(
                    "icon-rows",
                    rows,
                    cx.processor(
                        move |this, row_range: std::ops::Range<usize>, _window, cx| {
                            let n = this.folder.read(cx).row_count();
                            let per_row = this.tiles_per_row();
                            row_range
                                .map(|row| {
                                    let start = row * per_row;
                                    let end = (start + per_row).min(n);
                                    let mut tiles = Vec::with_capacity(end - start);
                                    for ix in start..end {
                                        tiles.push(this.render_tile(ix, cx));
                                    }
                                    h_flex()
                                        .w_full()
                                        .px(px(Self::PAD))
                                        .gap(px(Self::GAP))
                                        .children(tiles)
                                        .into_any_element()
                                })
                                .collect()
                        },
                    ),
                )
                .size_full()
                .track_scroll(&self.icon_scroll),
            )
    }

    fn render_tile(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.folder.read(cx).entry_at(ix).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let palette = BrowserPalette::resolve(cx);
        let selected = self.folder.read(cx).is_row_selected(ix);
        let focused = self.pane_focused.load(Ordering::Relaxed);
        let broken = entry.kind.is_broken_symlink();

        let icon_color = if broken {
            theme.warning
        } else if entry.is_dir_like() {
            palette.folder
        } else {
            theme.muted_foreground
        };
        let label_color = if selected && focused {
            crate::theme::text_on(theme.selection, theme.foreground)
        } else {
            theme.foreground
        };
        let fill = if selected {
            if focused {
                theme.selection
            } else {
                palette.selected_idle
            }
        } else {
            gpui::transparent_black()
        };

        let tile_icon = entry_icon(&entry, Self::TILE_GLYPH, icon_color, cx);
        let dragged = if selected {
            self.folder.read(cx).selected_paths()
        } else {
            vec![entry.path.clone()]
        };
        let drag_tile = drag_payload(dragged);
        let tile_drop_dir = entry.is_dir_like().then(|| entry.path.clone());
        let drops = self.drops.clone();
        let accent = theme.primary;

        div()
            .id(("tile", ix))
            .w(px(Self::TILE_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(5.))
            .p(px(8.))
            .pt(px(10.))
            .rounded(px(6.))
            .bg(fill)
            .when(!selected, |tile| {
                tile.hover(|tile| tile.bg(theme.secondary))
            })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                this.tile_clicked(ix, event, cx);
            }))
            .on_drag(drag_tile.0, move |_, _, _, cx| {
                let label = drag_tile.1.clone();
                cx.new(|_| DragPreview { label })
            })
            .external_drag_payload({
                let folder = self.folder.clone();
                move |dragged: &DraggedPaths, _, cx| external_payload(dragged, folder.read(cx))
            })
            .when_some(tile_drop_dir, |tile, dest| {
                let drops = drops.clone();
                tile.drag_over::<DraggedPaths>(move |style, _, _, _| {
                    style.border_1().border_color(accent)
                })
                .on_drop(move |dragged: &DraggedPaths, window, _| {
                    queue_drop(&drops, dragged, dest.clone(), window);
                })
            })
            .child(tile_icon)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    // Select under the cursor, then open the target menu.
                    let path = entry.path.clone();
                    this.folder.update(cx, |f, cx| {
                        f.prepare_context_menu(ix);
                        cx.emit(FolderEvent::SelectionChanged);
                    });
                    this.open_menu_at(Some(path), event, window, cx);
                }),
            )
            .child(
                div()
                    .w_full()
                    .line_clamp(2)
                    .text_size(px(11.5))
                    .line_height(px(14.))
                    .text_center()
                    .text_color(label_color)
                    .when(broken, |label| label.line_through())
                    .child(entry.name.clone()),
            )
            .into_any_element()
    }

    /// Pointer selection on a tile: plain replaces, Cmd/Ctrl toggles,
    /// Shift extends — identical semantics to the list.
    fn tile_clicked(&mut self, ix: usize, event: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.folder.update(cx, |f, _| f.extend_select(ix));
        } else if modifiers.platform || modifiers.control {
            self.folder.update(cx, |f, cx| {
                f.toggle_select(ix);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else {
            self.folder.update(cx, |f, cx| {
                f.select_only(ix);
                cx.emit(FolderEvent::SelectionChanged);
            });
        }
        if event.click_count() >= 2 {
            let entry = self.folder.read(cx).entry_at(ix).cloned();
            if let Some(entry) = entry {
                let ev = if entry.is_dir_like() {
                    PaneEvent::EnterDirectory(entry.path.clone())
                } else {
                    PaneEvent::OpenFile(entry.path.clone())
                };
                cx.emit(ev);
            }
        }
        self.sync_table_cursor(cx);
        cx.notify();
    }

    // -- marquee ------------------------------------------------------------

    /// Marquee in list view (§8: "expected in list view").
    ///
    /// Starts only below the last row. A drag that begins *on* a row is the
    /// start of a file drag, not a selection band — which is what both
    /// Finder and Explorer do, and what §8's "Drag | Rows" entry describes.
    fn start_list_marquee(&mut self, event: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        if self.list_row_at(event.position, cx).is_some() {
            return;
        }
        self.begin_marquee(event.position, cx);
    }

    /// Which list row sits under a window-space point, if any.
    fn list_row_at(
        &self,
        position: gpui::Point<gpui::Pixels>,
        cx: &Context<Self>,
    ) -> Option<usize> {
        let origin = *self.pane_origin.lock().unwrap();
        let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
            &self.table.read(cx).vertical_scroll_handle,
        );
        let local_y = position.y - origin.y - px(COLUMN_HEADER_HEIGHT) - scrolled.y;
        if local_y < px(0.) {
            return None; // In the header.
        }
        let row = (f32::from(local_y) / ROW_HEIGHT).floor() as usize;
        (row < self.folder.read(cx).row_count()).then_some(row)
    }

    /// Rows whose bands intersect the marquee, in list view.
    fn list_rows_in_band(
        &self,
        top: gpui::Pixels,
        bottom: gpui::Pixels,
        cx: &Context<Self>,
    ) -> Vec<usize> {
        let n = self.folder.read(cx).row_count();
        if n == 0 {
            return Vec::new();
        }
        let origin = *self.pane_origin.lock().unwrap();
        let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
            &self.table.read(cx).vertical_scroll_handle,
        );
        let to_row = |y: gpui::Pixels| {
            let local = y - origin.y - px(COLUMN_HEADER_HEIGHT) - scrolled.y;
            (f32::from(local) / ROW_HEIGHT).floor() as isize
        };
        let first = to_row(top).clamp(0, n as isize - 1) as usize;
        let last = to_row(bottom).clamp(0, n as isize - 1) as usize;
        (first..=last).collect()
    }

    fn begin_marquee(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.marquee = Some(Marquee {
            start: at,
            current: at,
        });
        self.folder.update(cx, |f, cx| {
            f.clear_selection();
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    fn start_marquee(&mut self, event: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        // Only start on empty space: a tile under the cursor claims it first
        // (its own handler runs in the inner bubble phase and marks the
        // folder's selection).
        let n = self.folder.read(cx).row_count();
        if self.tile_at(event.position, n).is_some() {
            return;
        }
        self.marquee = Some(Marquee {
            start: event.position,
            current: event.position,
        });
        self.folder.update(cx, |f, cx| {
            f.clear_selection();
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    fn update_marquee(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(marquee) = self.marquee.as_ref().map(|m| (m.start, m.current)) else {
            return;
        };
        let (start, current) = marquee;
        let delta = event.position - current;
        if delta.x.abs() < gpui::px(1.) && delta.y.abs() < gpui::px(1.) {
            return;
        }
        self.marquee = Some(Marquee {
            start,
            current: event.position,
        });

        // A list row spans the full width, so only the vertical band matters.
        if self.view_mode == ViewMode::List {
            let top = start.y.min(event.position.y);
            let bottom = start.y.max(event.position.y);
            let hits = self.list_rows_in_band(top, bottom, cx);
            self.folder.update(cx, |f, cx| {
                f.select_rows(hits);
                cx.emit(FolderEvent::SelectionChanged);
            });
            cx.notify();
            return;
        }

        // Live-select every tile intersecting the band (content coordinates).
        let origin = *self.pane_origin.lock().unwrap();
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll);
        let to_content = |p: gpui::Point<gpui::Pixels>| {
            gpui::point(p.x - origin.x + offset.x, p.y - origin.y + offset.y)
        };
        let a = to_content(start);
        let b = to_content(event.position);
        let left = a.x.min(b.x);
        let right = a.x.max(b.x);
        let top = a.y.min(b.y);
        let bottom = a.y.max(b.y);

        let per_row = self.tiles_per_row();
        let n = self.folder.read(cx).row_count();
        let col_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first = (((lo - px(Self::PAD) + px(Self::GAP)) / px(Self::TILE_W + Self::GAP))
                .floor() as isize)
                .clamp(0, per_row as isize - 1);
            let last = (((hi - px(Self::PAD)) / px(Self::TILE_W + Self::GAP)).floor() as isize)
                .clamp(0, per_row as isize - 1);
            first..=last
        };
        let row_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first =
                (((lo - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor() as isize).max(0);
            let last =
                (((hi - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor() as isize).max(0);
            first..=last
        };

        let mut hits = Vec::new();
        if right >= left && bottom >= top {
            for r in row_range(top, bottom) {
                for c in col_range(left, right) {
                    let ix = (r as usize) * per_row + c as usize;
                    if ix < n {
                        hits.push(ix);
                    }
                }
            }
        }
        self.folder.update(cx, |f, cx| {
            f.select_rows(hits);
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    fn tile_at(&self, position: gpui::Point<gpui::Pixels>, n: usize) -> Option<usize> {
        let origin = *self.pane_origin.lock().unwrap();
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll);
        let local = gpui::point(
            position.x - origin.x + offset.x,
            position.y - origin.y + offset.y,
        );
        let per_row = self.tiles_per_row();
        let col = (((local.x - px(Self::PAD) + px(Self::GAP)) / px(Self::TILE_W + Self::GAP))
            .floor()) as isize;
        let row = (((local.y - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor()) as isize;
        if col < 0 || row < 0 || col as usize >= per_row {
            return None;
        }
        let ix = row as usize * per_row + col as usize;
        (ix < n).then_some(ix)
    }

    fn marquee_overlay(&self) -> gpui::AnyElement {
        let Some(marquee) = &self.marquee else {
            return div().into_any_element();
        };
        let origin = *self.pane_origin.lock().unwrap();
        let left = marquee.start.x.min(marquee.current.x) - origin.x;
        let top = marquee.start.y.min(marquee.current.y) - origin.y;
        let width = (marquee.start.x - marquee.current.x).abs();
        let height = (marquee.start.y - marquee.current.y).abs();

        div()
            .absolute()
            .left(left)
            .top(top)
            .w(width)
            .h(height)
            .border_1()
            .border_color(gpui::rgb(0x0a6cff))
            .bg(gpui::rgb(0x0a6cff).opacity(0.12))
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Column view

/// Width of one listing column (§5). Wide enough for a typical file name at
/// 13 px without the chain becoming unscrollable on a 760 px window.
const COLUMN_WIDTH: f32 = 216.;
/// The rightmost column: metadata for the selection, never a file preview —
/// the preview pane is an addon.
const METADATA_WIDTH: f32 = 208.;
/// How many strict ancestors get their own column.
///
/// Each column is a real directory walk, so an unbounded chain would list
/// every ancestor of a deep path on arrival. Three is also what fits beside
/// the browsed directory and its metadata at the default window width, which
/// keeps the column you are actually in on screen without a sideways scroll.
const MAX_ANCESTOR_COLUMNS: usize = 3;
/// File row height (§4). Fixed on purpose: it is what lets both the list and
/// the column view virtualise cheaply, and what makes a row's on-screen
/// position computable for the rename overlay.
const ROW_HEIGHT: f32 = 26.;

impl FilePane {
    /// Walk the ancestor chain into one [`Folder`] per column.
    ///
    /// Each ancestor gets the browsed folder's sort and hidden-file settings
    /// so the chain reads consistently, and keeps the child that lies on the
    /// path so it can be drawn selected.
    fn build_ancestor_columns(&mut self, cx: &mut Context<Self>) {
        self.ancestors.clear();
        self.anchor_chain_right = true;

        let (dir, sort_key, sort_order, show_hidden) = self.folder.read_with(cx, |f, _| {
            (f.dir.clone(), f.sort_key(), f.sort_order(), f.show_hidden())
        });

        for (parent, on_path) in ancestor_chain(&dir, MAX_ANCESTOR_COLUMNS) {
            let folder = cx.new(|_| Folder::new(parent, sort_key, sort_order, show_hidden));
            folder.update(cx, |f, cx| f.start_loading(cx));
            let scroll = gpui::UniformListScrollHandle::new();
            // Repaint as each ancestor's rows land, and bring the on-path
            // child into view once they have: a chain scrolled to the top of
            // a 300-entry directory does not read as a path.
            let subscription = cx.subscribe(&folder, {
                let on_path = on_path.clone();
                let scroll = scroll.clone();
                move |_, folder, event: &FolderEvent, cx| {
                    if *event == FolderEvent::LoadFinished
                        && let Some(row) = folder.read(cx).row_of(&on_path)
                    {
                        scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
                    }
                    cx.notify();
                }
            });
            self.ancestors.push(AncestorColumn {
                folder,
                on_path,
                scroll,
                _subscription: subscription,
            });
        }
        cx.notify();
    }

    fn render_column_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;

        // The chain is wider than the pane on any real path, so it scrolls
        // sideways; after a rebuild it lands on the right-hand end, which is
        // where the browsed directory and its metadata are.
        if self.anchor_chain_right {
            // The metadata column is the last child; scrolling it into view
            // brings the browsed directory with it.
            let last = self.ancestors.len() + 1;
            self.column_chain_scroll.scroll_to_item(last);
            // A scroll request needs child bounds, which only exist after the
            // chain has been laid out once — so ask again next frame until it
            // can take effect, then stop and leave the scroll to the user.
            if self.column_chain_scroll.bounds_for_item(last).is_some() {
                self.anchor_chain_right = false;
            } else {
                cx.notify();
            }
        }

        let mut columns = h_flex()
            .id("column-view")
            .size_full()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&self.column_chain_scroll);

        for ix in 0..self.ancestors.len() {
            columns = columns.child(self.render_ancestor_column(ix, cx));
        }
        columns
            .child(self.render_current_column(cx))
            .child(self.render_metadata_column(cx))
            .bg(background)
    }

    /// An ancestor listing: read-only, with the on-path child highlighted.
    /// Clicking any row moves the browser to that row's directory, which
    /// re-derives the whole chain.
    fn render_ancestor_column(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(column) = self.ancestors.get(ix) else {
            return div().into_any_element();
        };
        let folder = column.folder.clone();
        let on_path = column.on_path.clone();
        let scroll = column.scroll.clone();
        let count = folder.read(cx).row_count();

        column_frame(cx)
            .child(
                gpui::uniform_list(
                    ("ancestor-column", ix),
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|row| {
                                let Some(entry) = folder.read(cx).entry_at(row).cloned() else {
                                    return div().into_any_element();
                                };
                                let selected = entry.path == on_path;
                                let target = entry.path.clone();
                                let is_dir = entry.is_dir_like();
                                this.column_row(("ancestor-row", row), &entry, selected, cx)
                                    .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _, cx| {
                                        // A file's own column is its
                                        // parent, which is where clicking
                                        // it lands you.
                                        let dir = if is_dir {
                                            target.clone()
                                        } else {
                                            match target.parent() {
                                                Some(parent) => parent,
                                                None => return,
                                            }
                                        };
                                        cx.emit(PaneEvent::EnterDirectory(dir));
                                    }))
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&scroll),
            )
            .into_any_element()
    }

    /// The browsed directory's own column: the real selection lives here, so
    /// clicks use the same modifier semantics as the list and icon views.
    fn render_current_column(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let count = self.folder.read(cx).row_count();
        let scroll = self.column_scroll.clone();

        column_frame(cx)
            .child(
                gpui::uniform_list(
                    "current-column",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|row| {
                                let Some(entry) = this.folder.read(cx).entry_at(row).cloned()
                                else {
                                    return div().into_any_element();
                                };
                                let selected = this.folder.read(cx).is_row_selected(row);
                                this.column_row(("current-row", row), &entry, selected, cx)
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, _, cx| {
                                            this.column_row_clicked(row, event, cx);
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(
                                            move |this,
                                                  event: &gpui::MouseDownEvent,
                                                  window,
                                                  cx| {
                                                let path = this
                                                    .folder
                                                    .read(cx)
                                                    .entry_at(row)
                                                    .map(|e| e.path.clone());
                                                this.folder.update(cx, |f, cx| {
                                                    f.prepare_context_menu(row);
                                                    cx.emit(FolderEvent::SelectionChanged);
                                                });
                                                this.open_menu_at(path, event, window, cx);
                                            },
                                        ),
                                    )
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&scroll),
            )
            .into_any_element()
    }

    /// §5: a directory selection becomes the next listing; a file selection
    /// just selects, and the metadata column describes it.
    fn column_row_clicked(&mut self, row: usize, event: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.folder.update(cx, |f, cx| {
                f.extend_select(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else if modifiers.platform || modifiers.control {
            self.folder.update(cx, |f, cx| {
                f.toggle_select(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else {
            self.folder.update(cx, |f, cx| {
                f.select_only(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        }
        self.sync_table_cursor(cx);
        cx.notify();

        let Some(entry) = self.folder.read(cx).entry_at(row).cloned() else {
            return;
        };
        if entry.is_dir_like() && !modifiers.shift && !modifiers.platform && !modifiers.control {
            cx.emit(PaneEvent::EnterDirectory(entry.path));
        } else if event.click_count() >= 2 && !entry.is_dir_like() {
            cx.emit(PaneEvent::OpenFile(entry.path));
        }
    }

    /// One 26 px row: icon, name, and a chevron for directories so the chain
    /// reads as something you can descend.
    fn column_row(
        &self,
        id: impl Into<gpui::ElementId>,
        entry: &ferret_vfs::Entry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let palette = BrowserPalette::resolve(cx);
        let focused = self.pane_focused.load(Ordering::Relaxed);
        let broken = entry.kind.is_broken_symlink();

        let fill = match (selected, focused) {
            (true, true) => theme.selection,
            (true, false) => palette.selected_idle,
            (false, _) => gpui::transparent_black(),
        };
        let text = if selected && focused {
            crate::theme::text_on(theme.selection, theme.foreground)
        } else {
            theme.foreground
        };
        let icon_color = if broken {
            theme.warning
        } else if entry.is_dir_like() {
            palette.folder
        } else {
            theme.muted_foreground
        };

        h_flex()
            .id(id)
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px(px(8.))
            .bg(fill)
            .when(!selected, |row| row.hover(|row| row.bg(theme.secondary)))
            .child(entry_icon(entry, 15., icon_color, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(ROW_FONT_SIZE))
                    .text_color(text)
                    .when(broken, |label| label.line_through())
                    .child(entry.name.clone()),
            )
            .when(entry.is_dir_like(), |row| {
                row.child(
                    gpui_component::Icon::new(gpui_component::IconName::ChevronRight)
                        .with_size(px(12.))
                        .text_color(if selected && focused {
                            crate::theme::text_on(theme.selection, theme.foreground)
                        } else {
                            theme.muted_foreground.opacity(0.7)
                        }),
                )
            })
    }

    /// The rightmost column. Metadata for the selection, not a preview.
    fn render_metadata_column(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (count, size, selected_entry) = self.folder.read_with(cx, |folder, _| {
            let count = folder.selection_count();
            let entry = (count == 1)
                .then(|| folder.selected_paths().first().cloned())
                .flatten()
                .and_then(|path| {
                    (0..folder.row_count())
                        .find_map(|row| folder.entry_at(row).filter(|e| e.path == path))
                        .cloned()
                });
            (count, folder.selection_size(), entry)
        });

        let frame = v_flex()
            .w(px(METADATA_WIDTH))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .p(px(14.))
            .text_color(theme.muted_foreground);

        let Some(entry) = selected_entry else {
            let label = match count {
                0 => "No selection".to_string(),
                n => format!("{n} items selected — {}", format_size(size)),
            };
            return frame
                .justify_center()
                .child(div().text_size(px(12.)).text_center().child(label))
                .into_any_element();
        };

        let icon_path = cx.global::<FerretState>().icons.asset_path(&entry);
        let icon_color = if entry.is_dir_like() {
            BrowserPalette::resolve(cx).folder
        } else {
            theme.muted_foreground
        };
        let detail = metadata_line(&entry);

        frame
            .child(
                gpui_component::Icon::default()
                    .path(icon_path)
                    .text_color(icon_color)
                    .with_size(px(48.)),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(12.5))
                    .text_center()
                    .text_color(theme.foreground)
                    .child(entry.name.clone()),
            )
            .child(div().text_size(px(11.5)).text_center().child(detail))
            .into_any_element()
    }
}

/// `(column directory, the child on the path)` for the nearest `max` strict
/// ancestors of `dir`, root-most first.
///
/// The child comes back with its parent because that is what each ancestor
/// column draws selected — without it the chain is a stack of listings rather
/// than a path.
fn ancestor_chain(dir: &VfsPath, max: usize) -> Vec<(VfsPath, VfsPath)> {
    let mut chain = Vec::new();
    let mut child = dir.clone();
    while chain.len() < max {
        let Some(parent) = child.parent() else {
            break;
        };
        chain.push((parent.clone(), child));
        child = parent;
    }
    // `parent()` walks upward; the columns read downward.
    chain.reverse();
    chain
}

/// The shared frame every listing column sits in: fixed width, hairline on
/// the right, its own scroll.
fn column_frame(cx: &App) -> gpui::Div {
    div()
        .w(px(COLUMN_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_r_1()
        .border_color(cx.theme().border)
}

/// "174 KB · 25 Aug 2026" — size and date, with directories showing neither a
/// size they do not have nor a lie in its place.
fn metadata_line(entry: &ferret_vfs::Entry) -> String {
    let size = entry.display_len().map(format_size);
    let date = entry.metadata.and_then(|m| m.modified).map(format_date);
    match (size, date) {
        (Some(size), Some(date)) => format!("{size} · {date}"),
        (Some(size), None) => size,
        (None, Some(date)) => date,
        (None, None) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Delegate

const DATE_WIDTH: f32 = 190.;
const SIZE_WIDTH: f32 = 84.;
const NAME_MIN_WIDTH: f32 = 220.;
/// The config slot at the right of the header (§4): opens the
/// column-visibility menu.
const CONFIG_WIDTH: f32 = 56.;
/// The disclosure column (§4), and the per-level indent that follows it.
const DISCLOSURE_WIDTH: f32 = 14.;
/// How close to a column's right edge a double-click counts as "size to
/// content" (§8) rather than a pair of sort clicks.
const EDGE_GRAB_WIDTH: f32 = 12.;
const ROW_FONT_SIZE: f32 = 13.;

/// A column the list can show, independent of where it currently sits.
///
/// Hiding a column changes the *indices* the table works in, so everything
/// that used to switch on `col_ix` switches on one of these instead — an
/// index means nothing once Date Modified can be absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileColumn {
    Name,
    Modified,
    Size,
    /// Not a column of data: the header's menu button lives here.
    Config,
}

impl FileColumn {
    /// Stable key, used for the settings file.
    fn key(self) -> &'static str {
        match self {
            FileColumn::Name => "name",
            FileColumn::Modified => "modified",
            FileColumn::Size => "size",
            FileColumn::Config => "config",
        }
    }

    fn label(self) -> &'static str {
        match self {
            FileColumn::Name => "Name",
            FileColumn::Modified => "Date Modified",
            FileColumn::Size => "Size",
            FileColumn::Config => "",
        }
    }

    fn sort_key(self) -> Option<SortKey> {
        match self {
            FileColumn::Name => Some(SortKey::Name),
            FileColumn::Modified => Some(SortKey::Modified),
            FileColumn::Size => Some(SortKey::Size),
            FileColumn::Config => None,
        }
    }

    /// The columns a person may turn off. Name stays: a list of dates with no
    /// names is not a file list.
    pub const HIDEABLE: [FileColumn; 2] = [FileColumn::Modified, FileColumn::Size];

    fn from_key(key: &str) -> Option<Self> {
        Self::HIDEABLE.into_iter().find(|c| c.key() == key)
    }
}

/// One row's icon: the real Finder/Explorer one when it has been resolved,
/// the bundled SVG until then (D-18).
///
/// Both are the same size on screen; the swap is invisible except that the
/// icon becomes the one the user recognises.
fn entry_icon(
    entry: &ferret_vfs::Entry,
    size: f32,
    tint: gpui::Hsla,
    cx: &App,
) -> gpui::AnyElement {
    if let Some(source) = crate::icons::os_icon(entry, cx) {
        return gpui::img(source)
            .w(px(size))
            .h(px(size))
            .flex_shrink_0()
            .into_any_element();
    }
    gpui_component::Icon::default()
        .path(cx.global::<FerretState>().icons.asset_path(entry))
        .text_color(tint)
        .with_size(px(size))
        .into_any_element()
}

/// The columns on screen, left to right, given what is hidden.
///
/// Name leads and the config slot trails, always: they are the two the user
/// cannot move or remove, so the hideable ones sit between them in a fixed
/// order rather than in whatever order they were toggled back on.
fn visible_columns(hidden: &[FileColumn]) -> Vec<FileColumn> {
    let mut columns = vec![FileColumn::Name];
    columns.extend(
        FileColumn::HIDEABLE
            .into_iter()
            .filter(|c| !hidden.contains(c)),
    );
    columns.push(FileColumn::Config);
    columns
}

/// Per-column width overrides from a size-to-content double-click (§8).
///
/// Shared with the pane, which measures; the delegate only reports what it is
/// told, because `column()` runs inside the table and has no text system.
type ColumnWidths = Arc<std::sync::Mutex<HashMap<FileColumn, f32>>>;

struct ListDelegate {
    folder: gpui::Entity<Folder>,
    pane_focused: Arc<AtomicBool>,
    pane_width: Arc<AtomicUsize>,
    /// Shared with the pane, which owns the menu that mutates it.
    hidden_columns: Arc<std::sync::Mutex<Vec<FileColumn>>>,
    drops: DropQueue,
    /// Disclosure clicks, drained by the pane on the next frame.
    toggles: ToggleQueue,
    /// Size-to-content results, and the requests that produce them.
    widths: ColumnWidths,
    sizing_requests: SizeToContentQueue,
}

/// Columns whose edge was double-clicked, awaiting measurement.
type SizeToContentQueue = Arc<std::sync::Mutex<Vec<FileColumn>>>;

impl ListDelegate {
    fn palette(&self, cx: &App) -> BrowserPalette {
        BrowserPalette::resolve(cx)
    }

    /// The columns actually on screen, left to right.
    fn visible(&self) -> Vec<FileColumn> {
        visible_columns(&self.hidden_columns.lock().unwrap())
    }

    /// A column's width: the measured one if it has been sized to content,
    /// otherwise the fixed default from §4.
    fn width_for(&self, column: FileColumn, default: f32) -> f32 {
        self.widths
            .lock()
            .ok()
            .and_then(|w| w.get(&column).copied())
            .unwrap_or(default)
    }

    fn column_at(&self, col_ix: usize) -> FileColumn {
        self.visible()
            .get(col_ix)
            .copied()
            .unwrap_or(FileColumn::Config)
    }
}

impl TableDelegate for ListDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.visible().len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.folder.read(cx).row_count()
    }

    fn column(&self, col_ix: usize, cx: &App) -> Column {
        let folder = self.folder.read(cx);
        let initial_for = |key: SortKey| {
            if folder.sort_key() != key {
                ColumnSort::Default
            } else {
                match folder.sort_order() {
                    SortOrder::Ascending => ColumnSort::Ascending,
                    SortOrder::Descending => ColumnSort::Descending,
                }
            }
        };

        let column = self.column_at(col_ix);
        match column {
            FileColumn::Name => {
                // The Name column absorbs all spare width (§4): measured pane
                // width minus whatever fixed columns are actually showing.
                const SLACK: f32 = 24.;
                let visible = self.visible();
                let fixed: f32 = visible
                    .iter()
                    .map(|c| match c {
                        FileColumn::Modified => DATE_WIDTH,
                        FileColumn::Size => SIZE_WIDTH,
                        FileColumn::Config => CONFIG_WIDTH,
                        FileColumn::Name => 0.,
                    })
                    .sum();
                let available = self.pane_width.load(Ordering::Relaxed) as f32;
                let name_width = (available - fixed - SLACK).max(NAME_MIN_WIDTH);
                Column::new(column.key(), column.label())
                    .sortable()
                    .sort(initial_for(SortKey::Name))
                    .width(px(name_width))
                    .resizable(false)
            }
            FileColumn::Modified => Column::new(column.key(), column.label())
                .sortable()
                .sort(initial_for(SortKey::Modified))
                .width(px(self.width_for(column, DATE_WIDTH))),
            FileColumn::Size => Column::new(column.key(), column.label())
                .sortable()
                .sort(initial_for(SortKey::Size))
                .width(px(self.width_for(column, SIZE_WIDTH)))
                .text_right(),
            FileColumn::Config => Column::new(column.key(), column.label())
                .width(px(CONFIG_WIDTH))
                .resizable(false),
        }
    }

    /// The header's config slot: a checkable list of the hideable columns.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        _cx: &mut gpui::Context<TableState<Self>>,
    ) -> impl IntoElement {
        let column = self.column_at(col_ix);
        if column != FileColumn::Config {
            // §8: double-clicking a column edge sizes it to its content. The
            // library's own resize handle is `.occlude()`d and offers drag
            // only, so this is Ferret's own hit target, inset just clear of
            // it so a drag still resizes.
            let requests = self.sizing_requests.clone();
            let resizable = column != FileColumn::Name;
            // Hit-tested against the cell's own painted bounds rather than
            // positioned by an inset: how much of the cell the library gives
            // its content is its business, and an inset guessed against that
            // lands in the wrong place.
            let bounds = Arc::new(std::sync::Mutex::new(gpui::Bounds::default()));
            let probe = bounds.clone();

            return div()
                .id(("th", col_ix))
                .relative()
                .size_full()
                .on_prepaint(move |b, _, _| {
                    *probe.lock().unwrap() = b;
                })
                .child(div().size_full().child(column.label()))
                .when(resizable, |el| {
                    el.on_click(move |event: &gpui::ClickEvent, _, _| {
                        if event.click_count() < 2 {
                            return;
                        }
                        let cell = *bounds.lock().unwrap();
                        let edge = cell.origin.x + cell.size.width - px(EDGE_GRAB_WIDTH);
                        if event.position().x >= edge {
                            requests.lock().unwrap().push(column);
                        }
                    })
                })
                .into_any_element();
        }

        let hidden = self.hidden_columns.lock().unwrap().clone();
        h_flex()
            .size_full()
            .justify_center()
            .items_center()
            .child(
                gpui_component::button::Button::new("column-config")
                    .ghost()
                    .xsmall()
                    .icon(gpui_component::IconName::Settings2)
                    .tooltip("Columns")
                    .dropdown_menu(move |mut menu, _, _| {
                        for column in FileColumn::HIDEABLE {
                            menu = menu.menu_with_check(
                                column.label(),
                                !hidden.contains(&column),
                                match column {
                                    FileColumn::Size => {
                                        Box::new(actions::ToggleColumnSize) as Box<dyn gpui::Action>
                                    }
                                    _ => Box::new(actions::ToggleColumnModified),
                                },
                            );
                        }
                        menu
                    }),
            )
            .into_any_element()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) {
        let Some(key) = self.column_at(col_ix).sort_key() else {
            return; // The config slot is not a sort target.
        };
        // ColumnSort::Default means "no explicit order"; the header cycle
        // starts at Descending, so keep the previous order until told.
        let order = match sort {
            ColumnSort::Descending => SortOrder::Descending,
            ColumnSort::Ascending => SortOrder::Ascending,
            ColumnSort::Default => return,
        };
        self.folder.update(cx, |f, cx| {
            f.set_sort(key, order);
            cx.emit(FolderEvent::SortChanged);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) -> gpui::Stateful<gpui::Div> {
        let folder = self.folder.read(cx);
        let selected = folder.is_row_selected(row_ix);
        let focused = self.pane_focused.load(Ordering::Relaxed);
        let palette = self.palette(cx);

        // Dragging an unselected row carries that row; dragging a selected
        // one carries the whole selection (§8).
        let entry = folder.entry_at(row_ix).cloned();
        let dragged: Vec<VfsPath> = match &entry {
            Some(entry) if selected => folder.selected_paths(),
            Some(entry) => vec![entry.path.clone()],
            None => Vec::new(),
        };
        let drop_dir = entry
            .as_ref()
            .filter(|e| e.is_dir_like())
            .map(|e| e.path.clone());

        // No `size_full` here: the table applies w_full/h(row_height) and then
        // re-applies this style over its own, and a 100% height has no
        // intrinsic size — under ListSizingBehavior::Auto that measures the
        // row at zero and the list paints nothing.
        let mut row = div().id(("file-row", row_ix)).relative();

        if selected {
            let fill = if focused {
                cx.theme().selection
            } else {
                palette.selected_idle
            };
            row = row.child(div().absolute().inset_0().bg(fill));
        } else if row_ix % 2 == 1 {
            // The stripe paints as an overlay so the table's own hover style
            // cannot wash it out mid-hover.
            row = row.child(div().absolute().inset_0().bg(palette.row_alt));
        }

        if !dragged.is_empty() {
            let (payload, label) = drag_payload(dragged);
            let folder = self.folder.clone();
            row = row
                .on_drag(payload, move |_, _, _, cx| {
                    cx.new(|_| DragPreview {
                        label: label.clone(),
                    })
                })
                // Leaving the window turns the in-app drag into a real file
                // drag, so Ferret can drop into any application.
                .external_drag_payload(move |dragged: &DraggedPaths, _, cx| {
                    external_payload(dragged, folder.read(cx))
                });
        }

        // Only folders accept a drop; a file row is not a destination.
        if let Some(dest) = drop_dir {
            let drops = self.drops.clone();
            let accent = cx.theme().primary;
            row = row
                .drag_over::<DraggedPaths>(move |style, _, _, _| {
                    // The target has to be visible before the mouse comes up.
                    style.border_1().border_color(accent)
                })
                .on_drop(move |dragged: &DraggedPaths, window, cx| {
                    let _ = cx;
                    queue_drop(&drops, dragged, dest.clone(), window);
                });
        }
        row
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(entry) = self.folder.read(cx).entry_at(row_ix).cloned() else {
            return div().into_any_element();
        };

        let theme = cx.theme();
        let palette = self.palette(cx);
        let selected = self.folder.read(cx).is_row_selected(row_ix);
        let focused = self.pane_focused.load(Ordering::Relaxed);

        // The focused fill is an accent whose lightness differs per theme, so
        // the text colour is chosen against it rather than assumed.
        let selected_text = if focused {
            crate::theme::text_on(theme.selection, theme.foreground)
        } else {
            theme.foreground
        };

        match self.column_at(col_ix) {
            FileColumn::Name => {
                let broken = entry.kind.is_broken_symlink();
                let depth = self.folder.read(cx).depth_at(row_ix);
                let expansion = self.folder.read(cx).expansion_at(row_ix);
                let toggles = self.toggles.clone();
                let icon_color = if broken {
                    theme.warning
                } else if entry.is_dir_like() {
                    palette.folder
                } else {
                    theme.muted_foreground
                };
                let mut cell = h_flex()
                    .w_full()
                    .h_full()
                    .items_center()
                    .gap_2()
                    // Each level indents by one disclosure column, so the
                    // triangles line up down the tree.
                    .pl(px(6. + depth as f32 * DISCLOSURE_WIDTH))
                    .overflow_hidden()
                    // §4: a fixed 14 px disclosure column, present on every
                    // row so names stay aligned whether or not a row has one.
                    .child(
                        div()
                            .id(("disclosure", row_ix))
                            .w(px(DISCLOSURE_WIDTH))
                            .flex_shrink_0()
                            // §8: the triangle expands in place; it must not
                            // reach the row beneath, which would also select.
                            .when(expansion.is_some(), |el| {
                                let toggles = toggles.clone();
                                el.on_mouse_down(MouseButton::Left, move |_, _, cx| {
                                    cx.stop_propagation();
                                    *toggles.lock().unwrap() = Some(row_ix);
                                })
                            })
                            .children(expansion.map(|open| {
                                gpui_component::Icon::new(if open {
                                    gpui_component::IconName::ChevronDown
                                } else {
                                    gpui_component::IconName::ChevronRight
                                })
                                .with_size(px(11.))
                                .text_color(theme.muted_foreground.opacity(0.7))
                            })),
                    )
                    .child(entry_icon(&entry, 15., icon_color, cx));

                let label = div()
                    .truncate()
                    // The name never yields space to the target beside it:
                    // a row that shows only where a link pointed, and not
                    // what is broken, is worse than one without the target.
                    .flex_shrink_0()
                    .max_w_full()
                    .text_size(px(ROW_FONT_SIZE))
                    .text_color(if selected {
                        selected_text
                    } else {
                        theme.foreground
                    })
                    .child(entry.name.clone());

                cell = if broken {
                    cell.child(label.line_through())
                } else {
                    cell.child(label)
                };

                // §9: a broken link names its dead target, because "broken
                // link" without one is not something a person can act on.
                if let Some(target) = &entry.link_target {
                    cell = cell.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.warning)
                            .child(format!("→ {}", target.display())),
                    );
                }
                cell.into_any_element()
            }
            FileColumn::Modified => {
                let text = entry
                    .metadata
                    .and_then(|m| m.modified)
                    .map(format_date)
                    .unwrap_or_default();
                div()
                    .w_full()
                    .text_size(px(ROW_FONT_SIZE))
                    .text_color(if selected {
                        selected_text
                    } else {
                        theme.muted_foreground
                    })
                    .child(text)
                    .into_any_element()
            }
            FileColumn::Size => {
                let text = match entry.display_len() {
                    Some(len) => format_size(len),
                    None if entry.is_dir_like() => "—".into(),
                    None => String::new(), // metadata still streaming in
                };
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .pr(px(10.))
                    .text_size(px(ROW_FONT_SIZE))
                    .text_color(if selected {
                        selected_text
                    } else {
                        theme.muted_foreground
                    })
                    .child(text)
                    .into_any_element()
            }
            // The config slot carries the header's menu and nothing per row.
            FileColumn::Config => div().into_any_element(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) -> PopupMenu {
        let folder = self.folder.read(cx);
        if folder.entry_at(row_ix).is_none() {
            return menu;
        }
        entry_menu_items(menu, folder.selection_count())
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) -> impl IntoElement {
        let folder = self.folder.read(cx);
        // An empty folder and an empty filter result are different states
        // (§9) — never conflate them.
        if let Some(query) = folder.filter().filter(|_| folder.error().is_none()) {
            return h_flex()
                .size_full()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(format!("No items match \u{ab}{query}\u{bb}"))
                .child(
                    gpui_component::button::Button::new("clear-filter")
                        .small()
                        .label("Clear filter")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                std::boxed::Box::new(crate::actions::ClearFilter),
                                cx,
                            );
                        }),
                )
                .into_any_element();
        }
        // §9: named and actionable, not a raw EACCES. Every `VfsError` carries
        // its subject and its `Display` is written to be shown unedited, so
        // the message is the error itself rather than a generic stand-in.
        let message = match folder.error() {
            Some(err) => err.to_string(),
            None => "This folder is empty.".to_string(),
        };

        h_flex()
            .size_full()
            .justify_center()
            .px(px(24.))
            .text_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
            .into_any_element()
    }
}

/// The row and selection halves of §8's three menus.
///
/// They differ by more than a label: renaming is a single-item operation, so
/// a multi-selection menu that offered it would have to pick one of them
/// silently. Everything else states its count, because "Move to Trash" over
/// nine selected files should say so before it happens.
fn entry_menu_items(menu: PopupMenu, selected: usize) -> PopupMenu {
    let multiple = selected > 1;
    let count = selected.min(999);

    let menu = menu
        .item(
            PopupMenuItem::new(if multiple {
                format!("Open {count} Items")
            } else {
                "Open".to_string()
            })
            .action(Box::new(actions::Open)),
        )
        .separator()
        .item(PopupMenuItem::new("Cut").action(Box::new(actions::Cut)))
        .item(PopupMenuItem::new("Copy").action(Box::new(actions::Copy)))
        .separator();

    let menu = if multiple {
        menu
    } else {
        menu.item(PopupMenuItem::new("Rename…").action(Box::new(actions::Rename)))
    };

    menu.item(
        PopupMenuItem::new(if multiple {
            format!("Move {count} Items to Trash")
        } else {
            "Move to Trash".to_string()
        })
        .action(Box::new(actions::Trash)),
    )
    .item(PopupMenuItem::new("Delete Permanently…").action(Box::new(actions::DeletePermanently)))
}

fn stem_length(name: &str) -> usize {
    match name.rfind('.') {
        Some(ix) if ix > 0 => ix,
        _ => name.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{Entry, EntryKind, Metadata};

    fn entry(name: &str, kind: EntryKind, len: u64) -> Entry {
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind,
            metadata: Some(Metadata {
                len,
                modified: Some(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_756_000_000),
                ),
                ..Default::default()
            }),
            link_target: None,
        }
    }

    #[test]
    fn the_chain_reads_downward_and_pairs_each_column_with_its_child() {
        let dir = VfsPath::local("/a/b/c/d");
        let chain = ancestor_chain(&dir, 4);

        let columns: Vec<String> = chain.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(columns, ["/", "/a", "/a/b", "/a/b/c"], "root-most first");

        // Each column's highlighted child is the next column's directory, and
        // the last one is the browsed directory itself.
        assert_eq!(chain[0].1.to_string(), "/a");
        assert_eq!(chain[3].1.to_string(), "/a/b/c/d");
    }

    #[test]
    fn the_chain_is_capped_at_the_nearest_ancestors() {
        let dir = VfsPath::local("/a/b/c/d/e/f/g");
        let chain = ancestor_chain(&dir, 2);
        assert_eq!(chain.len(), 2);
        // Capping keeps the *nearest* ancestors, not the root-most ones.
        assert_eq!(chain[0].0.to_string(), "/a/b/c/d/e");
        assert_eq!(chain[1].0.to_string(), "/a/b/c/d/e/f");
    }

    #[test]
    fn a_root_directory_has_no_ancestor_columns() {
        assert!(ancestor_chain(&VfsPath::local("/"), 4).is_empty());
    }

    #[test]
    fn metadata_line_omits_a_size_directories_do_not_have() {
        let file = metadata_line(&entry("Cargo.lock", EntryKind::File, 178_000));
        assert!(file.starts_with("174 KB · "), "{file}");

        // A directory has no meaningful size, so the line is the date alone
        // rather than "0 bytes".
        let dir = metadata_line(&entry("crates", EntryKind::Dir, 0));
        assert!(!dir.contains('·'), "{dir}");
        assert!(!dir.is_empty());
    }

    #[test]
    fn hiding_a_column_leaves_name_and_the_config_slot_in_place() {
        assert_eq!(
            visible_columns(&[]),
            [
                FileColumn::Name,
                FileColumn::Modified,
                FileColumn::Size,
                FileColumn::Config
            ]
        );

        // Hiding one drops only that one.
        assert_eq!(
            visible_columns(&[FileColumn::Modified]),
            [FileColumn::Name, FileColumn::Size, FileColumn::Config]
        );

        // Hiding both still leaves a file list you can read and a menu to
        // undo it with.
        assert_eq!(
            visible_columns(&[FileColumn::Size, FileColumn::Modified]),
            [FileColumn::Name, FileColumn::Config]
        );
    }

    #[test]
    fn columns_come_back_in_their_canonical_order() {
        // Toggled off Size then Modified, then Size back on: Size must return
        // to its own slot, not to wherever the toggle order would put it.
        let columns = visible_columns(&[FileColumn::Modified]);
        assert_eq!(columns[1], FileColumn::Size);
    }

    #[test]
    fn only_hideable_columns_round_trip_through_settings() {
        assert_eq!(FileColumn::from_key("size"), Some(FileColumn::Size));
        assert_eq!(FileColumn::from_key("modified"), Some(FileColumn::Modified));
        // Name and the config slot are not the user's to hide, so a settings
        // file naming them is ignored rather than obeyed.
        assert_eq!(FileColumn::from_key("name"), None);
        assert_eq!(FileColumn::from_key("config"), None);
        assert_eq!(FileColumn::from_key("nonsense"), None);
    }

    #[test]
    fn the_config_slot_is_not_a_sort_target() {
        assert_eq!(FileColumn::Name.sort_key(), Some(SortKey::Name));
        assert_eq!(FileColumn::Size.sort_key(), Some(SortKey::Size));
        assert_eq!(FileColumn::Config.sort_key(), None);
    }

    #[test]
    fn stems_are_preselected_without_the_extension() {
        assert_eq!(stem_length("notes.txt"), 5);
        assert_eq!(stem_length("archive.tar.gz"), 11);
        // A dotfile is all stem: renaming ".gitignore" must not preselect
        // nothing.
        assert_eq!(stem_length(".gitignore"), 10);
        assert_eq!(stem_length("Makefile"), 8);
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    fn p(s: &str) -> VfsPath {
        VfsPath::local(s)
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_itself() {
        assert!(droppable(&[p("/a/docs")], &p("/a/docs")).is_empty());
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_its_own_child() {
        // The case that would otherwise move a directory inside itself and
        // orphan everything under it.
        assert!(droppable(&[p("/a/docs")], &p("/a/docs/notes")).is_empty());
        assert!(droppable(&[p("/a")], &p("/a/b/c/d")).is_empty());
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_still_droppable() {
        // "/a/docs" is not inside "/a/doc" despite the shared text.
        let ok = droppable(&[p("/a/doc")], &p("/a/docs"));
        assert_eq!(ok, vec![p("/a/doc")]);
    }

    #[test]
    fn dropping_where_it_already_is_does_nothing() {
        assert!(droppable(&[p("/a/file.txt")], &p("/a")).is_empty());
    }

    #[test]
    fn a_mixed_drag_keeps_only_the_valid_paths() {
        // Into /a/sub: one from elsewhere, one from the parent directory —
        // both real moves — plus the destination itself, which is not.
        let sources = [p("/other/keep.txt"), p("/a/also.txt"), p("/a/sub")];
        assert_eq!(
            droppable(&sources, &p("/a/sub")),
            vec![p("/other/keep.txt"), p("/a/also.txt")]
        );

        // Into /a: the one already in /a drops out, the rest proceed.
        let sources = [p("/other/keep.txt"), p("/a/already.txt")];
        assert_eq!(droppable(&sources, &p("/a")), vec![p("/other/keep.txt")]);
    }
}
