//! Keyboard movement and sorting: what the arrow keys and the sort
//! actions do to the cursor and to the row order.
//!
//! Left and right mean something different in each view, so the
//! horizontal moves fan out by view mode rather than assuming a list.

use super::*;

impl FilePane {
    pub(super) fn move_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
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
    pub(super) fn extend_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
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
    pub(super) fn move_cursor_horizontal(&mut self, delta: isize, cx: &mut Context<Self>) {
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

    fn scroll_cursor_into_view(&mut self, row: usize, cx: &mut Context<Self>) {
        match self.view_mode {
            ViewMode::Icons => {
                let per_row = self.tiles_per_row(cx);
                self.icon_scroll
                    .scroll_to_item(row / per_row, gpui::ScrollStrategy::Nearest);
            }
            ViewMode::Columns => self
                .column_scroll
                .scroll_to_item(row, gpui::ScrollStrategy::Nearest),
            ViewMode::List => {}
        }
    }

    pub(super) fn apply_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
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

    pub(super) fn apply_order(&mut self, order: SortOrder, cx: &mut Context<Self>) {
        self.folder.update(cx, |f, cx| {
            f.set_sort(f.sort_key(), order);
            cx.emit(FolderEvent::SortChanged);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        self.table.update(cx, |_, cx| cx.notify());
    }
}
