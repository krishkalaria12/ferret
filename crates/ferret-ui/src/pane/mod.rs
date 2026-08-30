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

use parking_lot::Mutex;
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
use crate::browser::GitHandle;
use crate::folder::{Folder, FolderEvent, ListingProfile};
use crate::format::{format_date, format_size};
use crate::state::FerretState;
use crate::theme::BrowserPalette;

mod column_view;
mod drag;
mod icon_view;
mod list;
mod menu;
mod navigation;
mod rename;
mod render;

pub use drag::{DraggedPaths, droppable};
pub use list::FileColumn;

use column_view::AncestorColumn;
use drag::{DragPreview, DropQueue, ToggleQueue, drag_label, external_payload, queue_drop};
use icon_view::Marquee;
use list::{ColumnLayout, ColumnWidths, Columns, ListDelegate, SizeToContentQueue};
use menu::SpaceMenu;
use rename::Renaming;

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
    /// Which columns are on screen, shared with the delegate. What is hidden
    /// is persisted per folder through `FolderView::hidden_columns`.
    columns: Columns,
    /// Latest known pane width, shared with the delegate so the Name column
    /// can absorb the spare width the way §4 demands.
    pane_width: Arc<AtomicUsize>,
    /// The browser's git status snapshot, for the badge each view draws
    /// (F-51). One atomic load per read; the browser swaps it per refresh.
    git: GitHandle,
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
    pane_origin: Arc<Mutex<gpui::Point<gpui::Pixels>>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaneEvent> for FilePane {}

impl Focusable for FilePane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// File row height. Fixed on purpose: it is what lets both the list and the
/// column view virtualise cheaply, and what makes a row's on-screen position
/// computable for the rename overlay.
///
/// §4 says 26. The reference UI gives its rows more room, and at 26 with a
/// 15 px icon the text has almost no leading — so this follows the reference.
pub(super) const ROW_HEIGHT: f32 = 30.;
/// Column header height (§4), and where the loading hint sits.
pub(super) const COLUMN_HEADER_HEIGHT: f32 = 26.;
pub(super) const ROW_FONT_SIZE: f32 = 13.;
/// Column header type size (§4): chrome, one step down from the rows.
pub(super) const HEADER_FONT_SIZE: f32 = 11.5;

impl FilePane {
    pub fn new(
        folder: gpui::Entity<Folder>,
        view_mode: ViewMode,
        hidden_columns: &[String],
        git: GitHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pane_focused = Arc::new(AtomicBool::new(false));
        let pane_width = Arc::new(AtomicUsize::new(0));
        let drops: DropQueue = Arc::new(Mutex::new(None));
        let toggles: ToggleQueue = Arc::new(Mutex::new(None));
        let widths: ColumnWidths = Arc::new(Mutex::new(HashMap::new()));
        let sizing_requests: SizeToContentQueue = Arc::new(Mutex::new(Vec::new()));
        let columns: Columns = Arc::new(arc_swap::ArcSwap::from_pointee(ColumnLayout::new(
            hidden_columns
                .iter()
                .filter_map(|key| FileColumn::from_key(key)),
        )));
        let table = cx.new(|cx| {
            let delegate = ListDelegate {
                folder: folder.clone(),
                pane_focused: pane_focused.clone(),
                pane_width: pane_width.clone(),
                columns: columns.clone(),
                drops: drops.clone(),
                toggles: toggles.clone(),
                widths: widths.clone(),
                sizing_requests: sizing_requests.clone(),
                git: git.clone(),
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
            columns,
            icon_scroll: gpui::UniformListScrollHandle::new(),
            ancestors: Vec::new(),
            column_scroll: gpui::UniformListScrollHandle::new(),
            column_chain_scroll: gpui::ScrollHandle::new(),
            anchor_chain_right: false,
            marquee: None,
            pane_width,
            git,
            pane_origin: Arc::new(Mutex::new(gpui::point(px(0.), px(0.)))),
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

    /// Repaint the rows without changing anything about them.
    ///
    /// The git snapshot lives outside the pane's own state, so nothing here
    /// notices when it is swapped — the browser calls this after a refresh.
    /// The table needs its own notify: it is a separate entity, and notifying
    /// only the pane leaves the listing drawing the previous badges.
    pub fn repaint_rows(&self, cx: &mut Context<Self>) {
        self.table.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// The git badge for one entry, or nothing.
    ///
    /// One atomic load and one hash of a borrowed path — no allocation, so
    /// this is safe to call once per row per frame (Gate 7).
    fn git_status(&self, entry: &ferret_vfs::Entry) -> Option<ferret_git::FileStatus> {
        let path = entry.path.as_local()?;
        self.git.load().status_for(path, entry.is_dir_like())
    }

    /// Hidden column keys for the settings file, in a stable order.
    ///
    /// `ColumnLayout` already holds them in `HIDEABLE` order, so this is the
    /// stable order rather than the order they were toggled off in.
    pub fn hidden_column_keys(&self) -> Vec<String> {
        self.columns
            .load()
            .hidden()
            .iter()
            .map(|c| c.key().to_string())
            .collect()
    }

    /// Show or hide one column from the header's config menu.
    fn toggle_column(&mut self, column: FileColumn, cx: &mut Context<Self>) {
        let next = self.columns.load().with_toggled(column);
        self.columns.store(Arc::new(next));
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

/// Diameter of the git status dot on a file row (`docs/git.html` §7).
pub(super) const GIT_BADGE_SIZE: f32 = 7.;
/// …and on a directory, whose dot is a roll-up of what is beneath it rather
/// than a fact about the directory itself.
pub(super) const GIT_BADGE_DIR_SIZE: f32 = 6.;

/// The git status dot (F-51).
///
/// A dot rather than a letter or a coloured filename. Colour on the name would
/// fight the folder tint, which is the one place colour already carries meaning
/// in the listing (`ui-base.html` §7); a letter at 11 px inside a 30 px row is
/// a smudge. A directory's roll-up draws smaller and dimmer so it can be
/// followed down a tree without competing with its children.
pub(super) fn git_badge(
    status: ferret_git::FileStatus,
    is_dir: bool,
    palette: &BrowserPalette,
) -> gpui::AnyElement {
    let size = if is_dir {
        GIT_BADGE_DIR_SIZE
    } else {
        GIT_BADGE_SIZE
    };
    let fill = palette.git(status);
    div()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded_full()
        .bg(if is_dir { fill.opacity(0.7) } else { fill })
        // Staged *and* edited again is the one state where both facts matter,
        // so it draws as staged wearing the modified colour as a ring rather
        // than taking a sixth token nobody could tell apart from the other five.
        .when(status == ferret_git::FileStatus::StagedAndModified, |dot| {
            dot.border_1().border_color(palette.git_modified)
        })
        .into_any_element()
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
