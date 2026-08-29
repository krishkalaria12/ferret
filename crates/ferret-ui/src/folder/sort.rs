//! Sorting and filtering: the order rows appear in, and which of them are
//! visible at all.
//!
//! Both re-derive the display order rather than moving rows, which is what
//! lets an id-keyed selection survive them (see the module header).

use aho_corasick::AhoCorasick;
use rayon::slice::ParallelSliceMut as _;

use super::*;

/// Above this many entries, sorting goes parallel.
///
/// A 200,000-entry directory is ~3.4 million comparisons and is embarrassingly
/// parallel; a few thousand is not worth waking a pool for. The threshold is a
/// guess at where the hand-off stops costing more than the sort — it is safe
/// to be wrong by a factor of two in either direction.
const PARALLEL_SORT_THRESHOLD: usize = 20_000;

/// The inline name filter, compiled once per keystroke.
///
/// The needle is kept because the empty state names it back ("No items match
/// «x»"); the matcher is what runs. Filtering used to lowercase the needle
/// *and* every name into fresh `String`s once per entry — roughly 400,000
/// allocations for one keystroke on a large directory, and the field fires on
/// every keystroke (A-13).
pub(super) struct NameFilter {
    needle: String,
    /// `None` when the needle is not ASCII. `ascii_case_insensitive` folds
    /// only ASCII, so a needle containing `Ä` would silently stop matching
    /// `ärger`, which is a behaviour change and not a speed-up. Those fall
    /// back to the old lowercase-both-sides path, which is correct and rare.
    matcher: Option<AhoCorasick>,
}

impl NameFilter {
    pub(super) fn new(needle: String) -> Self {
        let matcher = needle.is_ascii().then(|| {
            AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .build([needle.as_bytes()])
                .expect("a single pattern always builds")
        });
        Self { needle, matcher }
    }

    fn matches(&self, name: &str) -> bool {
        match &self.matcher {
            // SIMD, over the name's bytes, with no allocation on either side.
            Some(matcher) => matcher.is_match(name),
            None => name.to_lowercase().contains(&self.needle.to_lowercase()),
        }
    }

    pub(super) fn needle(&self) -> &str {
        &self.needle
    }
}

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

    pub fn set_filter(&mut self, filter: Option<String>) {
        // Built once here, not once per entry: this is the whole of A-13.
        self.filter = filter.map(NameFilter::new);
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
            if let Some(id) = self.id_at(row) {
                self.selection.insert(id);
            }
        }
        if let Some(first) = self.selection.iter().next().copied() {
            self.anchor = Some(first);
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
        for step in 0..n {
            let row = (from + step) % n;
            let matches = self
                .entry_at(row)
                .is_some_and(|e| crate::format::starts_with_ignore_case(e.name(), prefix));
            if matches {
                self.select_only(row);
                return true;
            }
        }
        false
    }

    pub(super) fn resort(&mut self) {
        let (key, order) = (self.sort_key, self.sort_order);
        // Taken out so the comparator can read the slab while the id vector is
        // mutable. Sorting now permutes `u32`s, not `Entry`s.
        let mut ids = std::mem::take(&mut self.entries);
        // The comparator closes over the slab rather than over `self`: a
        // `Folder` holds GPUI tasks and is not `Sync`, but a slice of entries
        // is. Unstable, because the comparator ends in a name tiebreak and
        // names are unique within one directory — so the order is total and
        // stability buys nothing but an n/2 scratch buffer.
        let slab: &[Option<Entry>] = &self.slab;
        if ids.len() >= PARALLEL_SORT_THRESHOLD {
            // On Ferret's pool, not rayon's global one: a bare `par_sort`
            // would spin up a second pool alongside the one the listing walk
            // uses (`ferret_vfs::pool`).
            ferret_vfs::pool::install(|| {
                ids.par_sort_unstable_by(|a, b| cmp_slots(slab, *a, *b, key, order))
            });
        } else {
            ids.sort_unstable_by(|a, b| cmp_slots(slab, *a, *b, key, order));
        }
        self.entries = ids;
        self.rebuild_index_and_order();
    }

    fn rebuild_order(&mut self) {
        // Preserve the sorted entry order; just refresh visibility.
        self.rebuild_index_and_order();
    }

    /// [`cmp_slots`] against this folder's own slab.
    pub(super) fn cmp_ids(
        &self,
        a: EntryId,
        b: EntryId,
        key: SortKey,
        order: SortOrder,
    ) -> std::cmp::Ordering {
        cmp_slots(&self.slab, a, b, key, order)
    }

    /// Where `id` belongs in the sorted top-level listing.
    ///
    /// Used for watcher inserts, which have to land in place without
    /// consulting an index that earlier inserts in the same batch invalidated.
    pub(super) fn sorted_position(&self, id: EntryId) -> usize {
        let (key, order) = (self.sort_key, self.sort_order);
        self.entries.partition_point(|&other| {
            self.cmp_ids(other, id, key, order) == std::cmp::Ordering::Less
        })
    }

    /// Flatten the listing — and the listings of any expanded folders —
    /// into the display order the table renders.
    ///
    /// This runs on every watcher burst, so it does no heap allocation in the
    /// steady state: the row vector and the row index are reused in place and
    /// keep their capacity, and both are keyed on `u32`s rather than on cloned
    /// paths (A-05, D-36).
    pub(super) fn rebuild_index_and_order(&mut self) {
        let mut order = std::mem::take(&mut self.order);
        order.clear();

        for ix in 0..self.entries.len() {
            let id = self.entries[ix];
            let mark = order.len();
            order.push(Row { id, depth: 0 });
            self.push_expanded_children(id, 1, &mut order);
            // A folder whose subtree contains a match stays visible even when
            // its own name does not — otherwise the filter would hide the only
            // route to a row it just told the user exists.
            let has_visible_subtree = order.len() > mark + 1;
            if !has_visible_subtree && !self.is_visible_id(id) {
                order.truncate(mark);
            }
        }
        self.order = order;

        let mut row_of_id = std::mem::take(&mut self.row_of_id);
        row_of_id.clear();
        for (row, entry) in self.order.iter().enumerate() {
            row_of_id.insert(entry.id, row);
        }
        self.row_of_id = row_of_id;

        self.refresh_selection_summary();
    }

    /// Splice one expanded folder's visible children in beneath it, and
    /// theirs beneath them.
    fn push_expanded_children(&self, parent: EntryId, depth: u16, order: &mut Vec<Row>) {
        if !self.expanded.contains(&parent) {
            return;
        }
        let Some(children) = self.children.get(&parent) else {
            return; // Expanded but still loading.
        };
        for &id in children {
            let mark = order.len();
            order.push(Row { id, depth });
            self.push_expanded_children(id, depth + 1, order);
            if order.len() == mark + 1 && !self.is_visible_id(id) {
                order.truncate(mark);
            }
        }
    }

    fn is_visible_id(&self, id: EntryId) -> bool {
        self.entry(id).is_some_and(|e| self.is_visible(e))
    }

    pub(super) fn is_visible(&self, e: &Entry) -> bool {
        if !self.show_hidden && e.is_hidden() {
            return false;
        }
        if let Some(filter) = &self.filter {
            return filter.matches(e.name());
        }
        true
    }
}

/// Compare two slab slots by id.
///
/// A missing slot sorts last. That cannot happen for ids drawn from `entries`,
/// but it keeps the ordering total, which a comparator handed to `sort_by`
/// has to be.
fn cmp_slots(
    slab: &[Option<Entry>],
    a: EntryId,
    b: EntryId,
    key: SortKey,
    order: SortOrder,
) -> std::cmp::Ordering {
    let slot = |id: EntryId| slab.get(id.0 as usize).and_then(Option::as_ref);
    match (slot(a), slot(b)) {
        (Some(a), Some(b)) => cmp_entries(a, b, key, order),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
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
        // Kind: group by extension, then by name. The extension is the one
        // resolved at listing time (D-35) — deriving it here allocated a
        // `String` per comparison, millions of them per header click (A-12).
        SortKey::Kind => a
            .extension()
            .cmp(&b.extension())
            .then_with(|| crate::format::natural_compare(a.name(), b.name())),
    };

    let ord = match order {
        SortOrder::Ascending => ord,
        SortOrder::Descending => ord.reverse(),
    };

    // Stable, sensible tiebreak so equal keys keep names findable.
    ord.then_with(|| crate::format::natural_compare(a.name(), b.name()))
}
