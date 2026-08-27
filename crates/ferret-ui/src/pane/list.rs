//! List view: the `DataTable` delegate, the columns it can show, and the
//! measurement behind "size to content".
//!
//! Hiding a column changes the indices the table works in, so everything
//! here switches on a [`FileColumn`] rather than on a column index.

use super::menu::entry_menu_items;
use super::*;

const DATE_WIDTH: f32 = 190.;
const SIZE_WIDTH: f32 = 84.;
pub(super) const NAME_MIN_WIDTH: f32 = 220.;
/// The config slot at the right of the header (§4): opens the
/// column-visibility menu.
const CONFIG_WIDTH: f32 = 56.;
/// The disclosure column (§4), and the per-level indent that follows it.
const DISCLOSURE_WIDTH: f32 = 14.;
/// How close to a column's right edge a double-click counts as "size to
/// content" (§8) rather than a pair of sort clicks.
const EDGE_GRAB_WIDTH: f32 = 12.;

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
    pub(super) fn key(self) -> &'static str {
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

    pub(super) fn from_key(key: &str) -> Option<Self> {
        Self::HIDEABLE.into_iter().find(|c| c.key() == key)
    }
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
pub(super) type ColumnWidths = Arc<std::sync::Mutex<HashMap<FileColumn, f32>>>;

/// Every field is a handle the pane holds the other end of, so the pane builds
/// the delegate itself rather than through a constructor that would just list
/// the same eight clones twice.
pub(super) struct ListDelegate {
    pub(super) folder: gpui::Entity<Folder>,
    pub(super) pane_focused: Arc<AtomicBool>,
    pub(super) pane_width: Arc<AtomicUsize>,
    /// Shared with the pane, which owns the menu that mutates it.
    pub(super) hidden_columns: Arc<std::sync::Mutex<Vec<FileColumn>>>,
    pub(super) drops: DropQueue,
    /// Disclosure clicks, drained by the pane on the next frame.
    pub(super) toggles: ToggleQueue,
    /// Size-to-content results, and the requests that produce them.
    pub(super) widths: ColumnWidths,
    pub(super) sizing_requests: SizeToContentQueue,
}

/// Columns whose edge was double-clicked, awaiting measurement.
pub(super) type SizeToContentQueue = Arc<std::sync::Mutex<Vec<FileColumn>>>;

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

impl FilePane {
    /// Measure a column's widest rendered value and adopt that width (§8).
    ///
    /// Measured with the real text system rather than estimated from
    /// character counts: proportional digits and non-Latin names make any
    /// estimate wrong, and a "size to content" that clips its content is
    /// worse than none.
    pub(super) fn size_column_to_content(
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
    pub(super) fn render_loading_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
