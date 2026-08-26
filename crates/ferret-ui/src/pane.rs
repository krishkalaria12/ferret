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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ferret_settings::{SortKey, SortOrder};
use ferret_vfs::{Vfs as _, VfsPath};
use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, h_flex,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState},
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
}

pub struct FilePane {
    pub folder: gpui::Entity<Folder>,
    table: gpui::Entity<TableState<ListDelegate>>,
    focus: FocusHandle,
    /// Latest known pane width, shared with the delegate so the Name column
    /// can absorb the spare width the way §4 demands.
    pane_width: Arc<AtomicUsize>,
    typeahead: String,
    typeahead_at: std::time::Instant,
    /// The active inline rename, rendered as a small overlay editor.
    renaming: Option<Renaming>,
    _subscriptions: Vec<Subscription>,
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
    pub fn new(folder: gpui::Entity<Folder>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane_focused = Arc::new(AtomicBool::new(false));
        let pane_width = Arc::new(AtomicUsize::new(0));
        let table = cx.new(|cx| {
            let delegate = ListDelegate {
                folder: folder.clone(),
                pane_focused: pane_focused.clone(),
                pane_width: pane_width.clone(),
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

        Self {
            folder,
            table,
            focus,
            pane_width,
            typeahead: String::new(),
            typeahead_at: std::time::Instant::now(),
            renaming: None,
            _subscriptions: subscriptions,
        }
    }

    // ------------------------------------------------------------------
    // Commands the browser dispatches here

    pub fn selected_paths(&self, cx: &App) -> Vec<VfsPath> {
        self.folder.read(cx).selected_paths()
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
            move |this, input, event, _window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(input.clone(), cx),
                InputEvent::Blur => this.commit_rename(input.clone(), cx),
                _ => {}
            },
        ));

        self.renaming = Some(Renaming { path, input });
        cx.notify();
    }

    fn commit_rename(&mut self, input: gpui::Entity<InputState>, cx: &mut Context<Self>) {
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

        cx.spawn(async move |_, cx| {
            if let Err(err) = fs.rename(&src, &dest).await {
                eprintln!("rename failed: {err}");
                cx.update(|_| {});
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

impl Render for FilePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rename_overlay = self.renaming.as_ref().map(|r| {
            let theme = cx.theme();
            h_flex()
                .absolute()
                .left(px(12.))
                .bottom(px(12.))
                .w(px(320.))
                .p_1()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .shadow_md()
                .child(Input::new(&r.input).small())
        });

        let width_probe = self.pane_width.clone();
        let width_probe_owner = cx.weak_entity();
        let table = div()
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
                    .with_size(px(26.)),
            );

        div()
            .id("file-pane")
            .relative()
            .flex_1()
            .min_h_0()
            .key_context(actions::context::FILE_PANE)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::handle_typeahead))
            .on_action(cx.listener(|this, _: &actions::Open, _, cx| {
                if let Some(event) = this.open_current(cx) {
                    cx.emit(event);
                }
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
            .child(table)
            .when_some(rename_overlay, |el, overlay| el.child(overlay))
    }
}

// ---------------------------------------------------------------------------
// Delegate

const DATE_WIDTH: f32 = 190.;
const SIZE_WIDTH: f32 = 84.;
const ROW_FONT_SIZE: f32 = 13.;

struct ListDelegate {
    folder: gpui::Entity<Folder>,
    pane_focused: Arc<AtomicBool>,
    pane_width: Arc<AtomicUsize>,
}

impl ListDelegate {
    fn palette(&self, cx: &App) -> BrowserPalette {
        BrowserPalette::resolve(cx)
    }
}

impl TableDelegate for ListDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        3
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
        match col_ix {
            0 => {
                // The Name column absorbs all spare width (§4): measured pane
                // width minus the fixed columns and a little slack.
                const DATE: f32 = 190.;
                const SIZE: f32 = 84.;
                const SLACK: f32 = 24.;
                let available = self.pane_width.load(Ordering::Relaxed) as f32;
                let name_width = (available - DATE - SIZE - SLACK).max(220.);
                Column::new("name", "Name")
                    .sortable()
                    .sort(initial_for(SortKey::Name))
                    .width(px(name_width))
                    .resizable(false)
            }
            1 => Column::new("modified", "Date Modified")
                .sortable()
                .sort(initial_for(SortKey::Modified))
                .width(px(DATE_WIDTH)),
            _ => Column::new("size", "Size")
                .sortable()
                .sort(initial_for(SortKey::Size))
                .width(px(SIZE_WIDTH))
                .text_right(),
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) {
        let key = match col_ix {
            0 => SortKey::Name,
            1 => SortKey::Modified,
            _ => SortKey::Size,
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

        // White text reads against the accent selection; the idle fill is
        // light enough that ordinary foreground stays legible.
        let selected_text = if focused {
            gpui::white()
        } else {
            theme.foreground
        };

        match col_ix {
            0 => {
                let broken = entry.kind.is_broken_symlink();
                let icon_color = if broken {
                    theme.warning
                } else if entry.is_dir_like() {
                    palette.folder
                } else {
                    theme.muted_foreground
                };
                let icon_path = cx.global::<FerretState>().icons.asset_path(&entry);

                let mut cell = h_flex()
                    .w_full()
                    .h_full()
                    .items_center()
                    .gap_2()
                    .pl(px(6.))
                    .overflow_hidden()
                    .child(
                        gpui_component::Icon::default()
                            .path(icon_path)
                            .text_color(icon_color)
                            .with_size(px(15.)),
                    );

                let label = div()
                    .truncate()
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
                cell.into_any_element()
            }
            1 => {
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
            _ => {
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
        let Some(_entry) = folder.entry_at(row_ix) else {
            return menu;
        };
        let count = folder.selection_count();

        let open_label = if count > 1 {
            format!("Open {} Items", count.min(99))
        } else {
            "Open".to_string()
        };
        menu.item(PopupMenuItem::new(open_label).action(Box::new(actions::Open)))
            .separator()
            .item(PopupMenuItem::new("Cut").action(Box::new(actions::Cut)))
            .item(PopupMenuItem::new("Copy").action(Box::new(actions::Copy)))
            .separator()
            .item(PopupMenuItem::new("Rename…").action(Box::new(actions::Rename)))
            .item(PopupMenuItem::new("Move to Trash").action(Box::new(actions::Trash)))
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut gpui::Context<TableState<Self>>,
    ) -> impl IntoElement {
        let folder = self.folder.read(cx);
        let message = if folder.error().is_some() {
            "Ferret can't read this folder."
        } else {
            "This folder is empty."
        };

        h_flex()
            .size_full()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(message)
    }
}

fn stem_length(name: &str) -> usize {
    match name.rfind('.') {
        Some(ix) if ix > 0 => ix,
        _ => name.len(),
    }
}
