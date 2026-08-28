//! Sorting and filtering: the order rows appear in, and which of them are
//! visible at all.
//!
//! Both re-derive the display order rather than moving rows, which is what
//! lets a path-keyed selection survive them (see the module header).

use super::*;

impl Folder {
    // ------------------------------------------------------------------
    // Sorting and filtering

    pub fn set_sort(&mut self, key: SortKey, order: SortOrder) {
        let changed = self.sort_key != key || self.sort_order != order;
        self.sort_key = key;
        self.sort_order = order;
        if changed && !self.loading {
            self.resort();
        }
    }

    #[allow(dead_code)]
    pub fn toggle_sort_order(&mut self) {
        self.sort_order = match self.sort_order {
            SortOrder::Ascending => SortOrder::Descending,
            SortOrder::Descending => SortOrder::Ascending,
        };
        if !self.loading {
            self.resort();
        }
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        if self.show_hidden == show {
            return;
        }
        self.show_hidden = show;
        if !self.loading {
            self.rebuild_order();
        }
    }

    #[allow(dead_code)] // the inline filter box arrives with view modes
    pub fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
        if !self.loading {
            self.rebuild_order();
        }
    }

    /// Move the cursor by `delta` rows (icon/list keyboard navigation).
    /// Returns the new row, if any.
    pub fn move_cursor(&mut self, delta: isize) -> Option<usize> {
        let n = self.order.len() as isize;
        if n == 0 {
            return None;
        }
        let current = self.cursor_row().map(|r| r as isize).unwrap_or(-1);
        let next = (current + delta).clamp(0, n - 1) as usize;
        self.select_only(next);
        Some(next)
    }

    /// Move the cursor by `delta` and extend the selection from the anchor
    /// (Shift + arrow). Returns the new row, if any.
    ///
    /// Distinct from [`Self::move_cursor`] in that the anchor stays put, so
    /// reversing direction shrinks the range rather than starting a new one.
    pub fn extend_cursor(&mut self, delta: isize) -> Option<usize> {
        let n = self.order.len() as isize;
        if n == 0 {
            return None;
        }
        let current = self.cursor_row().map(|r| r as isize).unwrap_or(-1);
        let next = (current + delta).clamp(0, n - 1) as usize;
        self.extend_select(next);
        Some(next)
    }

    /// Select a set of display rows (marquee selection).
    pub fn select_rows(&mut self, rows: impl IntoIterator<Item = usize>) {
        self.selection.clear();
        for row in rows {
            if let Some(e) = self.entry_at(row) {
                self.selection.insert(e.path.clone());
            }
        }
        if let Some(last) = self.selection.iter().next().cloned() {
            self.anchor = Some(last);
        }
        self.refresh_selection_summary();
    }

    /// Type-ahead: select the first row at or after `from` whose name starts
    /// with `prefix`, wrapping around. Returns whether anything matched.
    pub fn select_by_prefix(&mut self, prefix: &str, from: usize) -> bool {
        let n = self.order.len();
        if n == 0 || prefix.is_empty() {
            return false;
        }
        let prefix = prefix.to_lowercase();
        for step in 0..n {
            let row = (from + step) % n;
            let matches = self
                .entry_at(row)
                .is_some_and(|e| e.name().to_lowercase().starts_with(&prefix));
            if matches {
                self.select_only(row);
                return true;
            }
        }
        false
    }

    pub(super) fn resort(&mut self) {
        let (key, order) = (self.sort_key, self.sort_order);
        self.entries.sort_by(|a, b| cmp_entries(a, b, key, order));
        self.rebuild_index_and_order();
    }

    fn rebuild_order(&mut self) {
        // Preserve the sorted entry order; just refresh visibility.
        self.rebuild_index_and_order();
    }

    /// Flatten the listing — and the listings of any expanded folders —
    /// into the display order the table renders.
    pub(super) fn rebuild_index_and_order(&mut self) {
        let mut order = Vec::with_capacity(self.entries.len());
        for ix in 0..self.entries.len() {
            let entry = &self.entries[ix];
            let path = entry.path.clone();

            // A folder whose subtree contains a match stays visible even when
            // its own name does not match — otherwise the filter would hide
            // the only route to a row it just told the user exists.
            let mut subtree = Vec::new();
            self.push_expanded_children(&path, 1, &mut subtree);
            if !self.is_visible(entry) && subtree.is_empty() {
                continue;
            }
            order.push(Row {
                source: RowSource::Top(ix),
                depth: 0,
            });
            order.append(&mut subtree);
        }
        self.order = order;

        self.row_of_path.clear();
        self.top_index.clear();
        self.known_paths.clear();
        for (ix, entry) in self.entries.iter().enumerate() {
            self.top_index.insert(entry.path.clone(), ix);
        }
        for (row, source) in self.order.iter().map(|r| r.source.clone()).enumerate() {
            if let Some(entry) = self.entry_for(&source) {
                self.row_of_path.insert(entry.path.clone(), row);
            }
        }
        for entry in self.entries.iter().chain(self.children.values().flatten()) {
            self.known_paths.insert(entry.path.clone());
        }
        self.prune_stale_paths();
        self.refresh_selection_summary();
    }

    /// Splice one expanded folder's visible children in beneath it, and
    /// theirs beneath them.
    fn push_expanded_children(&self, parent: &VfsPath, depth: u16, order: &mut Vec<Row>) {
        if !self.expanded.contains(parent) {
            return;
        }
        let Some(children) = self.children.get(parent) else {
            return; // Expanded but still loading.
        };
        for (ix, child) in children.iter().enumerate() {
            let mut subtree = Vec::new();
            self.push_expanded_children(&child.path, depth + 1, &mut subtree);
            if !self.is_visible(child) && subtree.is_empty() {
                continue;
            }
            order.push(Row {
                source: RowSource::Child {
                    parent: parent.clone(),
                    ix,
                },
                depth,
            });
            order.append(&mut subtree);
        }
    }

    /// A loaded entry by path, wherever it lives — the browsed listing or an
    /// expanded folder's.
    ///
    /// Indexed through `top_index`, never `row_of_path`: a display row is not
    /// an entry index once rows can be filtered or nested.
    pub(super) fn entry_mut(&mut self, path: &VfsPath) -> Option<&mut Entry> {
        if let Some(ix) = self.top_index.get(path).copied() {
            return self.entries.get_mut(ix);
        }
        let parent = path.parent()?;
        let children = self.children.get_mut(&parent)?;
        children.iter_mut().find(|e| e.path == *path)
    }

    fn entry_for(&self, source: &RowSource) -> Option<&Entry> {
        match source {
            RowSource::Top(ix) => self.entries.get(*ix),
            RowSource::Child { parent, ix } => self.children.get(parent)?.get(*ix),
        }
    }

    fn prune_stale_paths(&mut self) {
        // Against everything loaded, not just what is on screen: a filter
        // hides rows, it does not delete them.
        self.selection.retain(|p| self.known_paths.contains(p));
        self.anchor.retain(|p| self.known_paths.contains(p));
        self.cursor.retain(|p| self.known_paths.contains(p));
        self.expanded.retain(|p| self.known_paths.contains(p));
        self.children.retain(|p, _| self.known_paths.contains(p));
    }

    pub(super) fn is_visible(&self, e: &Entry) -> bool {
        if !self.show_hidden && e.is_hidden() {
            return false;
        }
        if let Some(f) = &self.filter {
            return e.name().to_lowercase().contains(&f.to_lowercase());
        }
        true
    }
}

/// Directories sort above files regardless of the sort key
/// (`settings.browser.directories_first`).
pub(super) fn cmp_entries(
    a: &Entry,
    b: &Entry,
    key: SortKey,
    order: SortOrder,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a.is_dir_like(), b.is_dir_like()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    let ord = match key {
        SortKey::Name => crate::format::natural_compare(a.name(), b.name()),
        SortKey::Modified => {
            let av = a
                .metadata
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let bv = b
                .metadata
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            av.cmp(&bv)
        }
        SortKey::Size => a
            .display_len()
            .unwrap_or(0)
            .cmp(&b.display_len().unwrap_or(0)),
        // Kind: group by extension, then by name.
        SortKey::Kind => a
            .path
            .extension()
            .cmp(&b.path.extension())
            .then_with(|| crate::format::natural_compare(a.name(), b.name())),
    };

    let ord = match order {
        SortOrder::Ascending => ord,
        SortOrder::Descending => ord.reverse(),
    };

    // Stable, sensible tiebreak so equal keys keep names findable.
    ord.then_with(|| crate::format::natural_compare(a.name(), b.name()))
}
