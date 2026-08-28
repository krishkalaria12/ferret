//! One browsed directory: the streamed listing plus everything the file pane
//! manipulates — sort, filter, selection.
//!
//! Two rules shape this type:
//!
//! 1. **Nothing blocks a frame.** Entries arrive from the VFS stream in
//!    batches and are appended as they land (`docs/ui-base.html` §9: show
//!    what has arrived, no spinner-then-swap). Sorting happens once, when the
//!    walk finishes — re-sorting mid-stream jumbles rows under the user's
//!    cursor for no benefit on directories small enough to care.
//! 2. **Selection survives re-sorting.** Selection, anchor and cursor are
//!    stored as paths, never indices. A watcher reload reshuffles rows; the
//!    user's selection must not follow the shuffle.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ferret_settings::{SortKey, SortOrder};
use ferret_vfs::{Entry, Vfs, VfsError, VfsPath, WatchEvent};
use futures::StreamExt;
use gpui::{Context, EventEmitter};

mod expansion;
mod listing;
mod sort;

#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;

/// How often the metadata backfill reports progress to the UI.
pub const META_NOTIFY_EVERY: Duration = Duration::from_millis(120);

/// Entries resolved by one metadata blocking-pool task.
///
/// A chunk amortises task dispatch without holding back so many updates that
/// Size and Date appear frozen on a large directory.
pub const METADATA_BATCH_SIZE: usize = 256;

/// How long a listing may produce nothing before the header admits it is
/// working (`docs/ui-base.html` §9).
///
/// Long enough that a local directory — which lands in under a millisecond —
/// never flashes a bar, short enough that a stalled network mount does not
/// look like an empty folder.
pub const LOADING_HINT_DELAY: Duration = Duration::from_millis(200);

/// Watcher bursts larger than this fall back to a full re-list.
///
/// Below the threshold, one `stat` per changed path is far cheaper than
/// re-reading a 200,000-entry directory; above it, the bookkeeping per path
/// costs more than one clean walk.
pub const WATCH_RELOAD_THRESHOLD: usize = 512;

/// How long to wait before repainting to pick up resolved OS icons.
///
/// One repaint rather than a subscription: icon resolution is a handful of
/// fast calls, and polling for them would cost more than the wait.
pub const OS_ICON_REPAINT_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderEvent {
    /// Row content or order changed; the list should repaint.
    RowsChanged,
    /// Only the selection moved.
    SelectionChanged,
    /// Loading finished (successfully or not).
    LoadFinished,
    /// The sort key or order changed, from wherever — the column header, the
    /// context menu or an action. The pane turns this into the event the
    /// browser persists on, so every route to a sort is remembered.
    SortChanged,
    /// The watched directory vanished; offer the nearest existing parent.
    Gone,
}

/// One visible row, after the tree has been flattened.
///
/// The flat model is a deliberate choice (§6 leaves it open): the table is
/// already virtualised over this vector, and a separate tree widget would
/// mean a second row model and a second proof of Gate 1.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    /// Where the entry lives.
    source: RowSource,
    /// How many folders deep, for the row's indent.
    depth: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowSource {
    /// An index into the browsed directory's own listing.
    Top(usize),
    /// An index into an expanded folder's listing.
    Child { parent: VfsPath, ix: usize },
}

pub struct Folder {
    pub dir: VfsPath,

    /// The browsed directory's own listing, sorted.
    entries: Vec<Entry>,
    /// Loaded listings of expanded folders, keyed by the folder.
    ///
    /// Kept beside `entries` rather than spliced into it so that sorting,
    /// the watcher and the metadata backfill all keep working on the flat
    /// listing they were written for.
    children: HashMap<VfsPath, Vec<Entry>>,
    /// Folders the user has expanded in place (§8).
    expanded: HashSet<VfsPath>,
    /// Visible rows only, in display order, flattened depth-first.
    order: Vec<Row>,
    /// Path → display row, for the visible rows only.
    row_of_path: HashMap<VfsPath, usize>,
    /// Path → index into `entries`, for the browsed directory's own listing.
    ///
    /// Distinct from `row_of_path`: a display row is not an entry index once
    /// rows can be filtered out or nested under an expanded folder, and the
    /// watcher edits `entries` rather than the display order.
    top_index: HashMap<VfsPath, usize>,
    /// Every path currently loaded, visible or filtered out.
    ///
    /// Separate from `row_of_path` because a selection must survive being
    /// hidden by a filter — pruning against visible rows alone would drop it.
    known_paths: HashSet<VfsPath>,

    loading: bool,
    error: Option<VfsError>,

    sort_key: SortKey,
    sort_order: SortOrder,
    show_hidden: bool,
    filter: Option<String>,

    selection: HashSet<VfsPath>,
    /// Selected paths in display order. This is kept separately from
    /// `selection` because selection survives filtering, while commands and
    /// the status bar operate only on rows the person can currently see.
    selected_visible_paths: Arc<[VfsPath]>,
    /// Cached alongside `selected_visible_paths` so rendering the status bar
    /// never has to walk a large listing.
    selected_visible_bytes: u64,
    anchor: Option<VfsPath>,
    cursor: Option<VfsPath>,

    /// Bumps on every navigation/reload; background tasks check it before
    /// touching state, which is how stale walks cancel themselves.
    generation: u64,
    watch: Option<ferret_vfs::Watch>,
    last_meta_notify: Option<Instant>,
    /// When the current listing began, for the slow-start hint.
    loading_since: Option<Instant>,
}

impl Folder {
    pub fn new(dir: VfsPath, sort_key: SortKey, sort_order: SortOrder, show_hidden: bool) -> Self {
        Self {
            dir,
            entries: Vec::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            order: Vec::new(),
            row_of_path: HashMap::new(),
            top_index: HashMap::new(),
            known_paths: HashSet::new(),
            loading: true,
            error: None,
            sort_key,
            sort_order,
            show_hidden,
            filter: None,
            selection: HashSet::new(),
            selected_visible_paths: Arc::from([]),
            selected_visible_bytes: 0,
            anchor: None,
            cursor: None,
            generation: 0,
            watch: None,
            last_meta_notify: None,
            loading_since: None,
        }
    }

    // ------------------------------------------------------------------
    // Introspection for the views

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Whether the listing has produced nothing for long enough to say so.
    ///
    /// Deliberately not "is loading": a directory that streams rows needs no
    /// reassurance, and §9 is explicit that rows appearing beat a spinner.
    pub fn is_slow_to_start(&self) -> bool {
        self.loading
            && self.order.is_empty()
            && self
                .loading_since
                .is_some_and(|since| since.elapsed() >= LOADING_HINT_DELAY)
    }

    pub fn error(&self) -> Option<&VfsError> {
        self.error.as_ref()
    }

    pub fn row_count(&self) -> usize {
        self.order.len()
    }

    pub fn entry_at(&self, row: usize) -> Option<&Entry> {
        match &self.order.get(row)?.source {
            RowSource::Top(ix) => self.entries.get(*ix),
            RowSource::Child { parent, ix } => self.children.get(parent)?.get(*ix),
        }
    }

    /// Indent level of a row: 0 for the browsed directory's own entries.
    pub fn depth_at(&self, row: usize) -> u16 {
        self.order.get(row).map(|r| r.depth).unwrap_or(0)
    }

    /// Whether this row is a folder that can be expanded in place, and
    /// whether it currently is.
    pub fn expansion_at(&self, row: usize) -> Option<bool> {
        let entry = self.entry_at(row)?;
        entry
            .is_dir_like()
            .then(|| self.expanded.contains(&entry.path))
    }

    #[allow(dead_code)] // wired to the filter UI and status bar polish
    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }

    pub fn sort_key(&self) -> SortKey {
        self.sort_key
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    #[allow(dead_code)]
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    #[allow(dead_code)]
    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// Whether the row at `row` is in the selection.
    pub fn is_row_selected(&self, row: usize) -> bool {
        self.entry_at(row)
            .is_some_and(|e| self.selection.contains(&e.path))
    }

    /// Paths of all selected entries, in display order.
    pub fn selected_paths(&self) -> Vec<VfsPath> {
        self.selected_visible_paths.to_vec()
    }

    /// Shared drag payload source for the current visible selection.
    ///
    /// The list can build one drag source per visible row. Cloning this handle
    /// must therefore not clone every selected path per row or per frame.
    pub fn selected_paths_shared(&self) -> Arc<[VfsPath]> {
        self.selected_visible_paths.clone()
    }

    /// Every entry currently on screen, in display order.
    fn visible_entries(&self) -> impl Iterator<Item = &Entry> {
        (0..self.order.len()).filter_map(|row| self.entry_at(row))
    }

    #[allow(dead_code)]
    pub fn has_selection(&self) -> bool {
        !self.selection.is_empty()
    }

    /// How many selected entries are actually on screen.
    ///
    /// Counted over visible rows, not the raw selection set: a filter that
    /// hides a selected row must not leave the status bar claiming items the
    /// user can neither see nor act on — every mutation runs over
    /// [`Self::selected_paths`], which is visible-only too.
    pub fn selection_count(&self) -> usize {
        self.selected_visible_paths.len()
    }

    /// Sum of selected sizes. Directories contribute nothing rather than a
    /// lie (`docs/ui-base.html` §9).
    pub fn selection_size(&self) -> u64 {
        self.selected_visible_bytes
    }

    pub fn cursor_row(&self) -> Option<usize> {
        self.cursor
            .as_ref()
            .and_then(|p| self.row_of_path.get(p))
            .copied()
    }

    /// The cursor row's path, for Open/Rename actions.
    pub fn cursor_path(&self) -> Option<VfsPath> {
        self.cursor.clone()
    }

    pub fn has_entry(&self, path: &VfsPath) -> bool {
        self.known_paths.contains(path)
    }

    /// Display row of `path`, or `None` when it is filtered out or absent.
    pub fn row_of(&self, path: &VfsPath) -> Option<usize> {
        self.row_of_path.get(path).copied()
    }

    /// The one selected entry when exactly one visible row is selected.
    pub fn selected_entry(&self) -> Option<&Entry> {
        (self.selected_visible_paths.len() == 1)
            .then(|| self.selected_visible_paths.first())
            .flatten()
            .and_then(|path| self.row_of(path))
            .and_then(|row| self.entry_at(row))
    }

    /// Every name in the browsed directory, for conflict-free "new file"
    /// naming. Children of expanded folders live elsewhere and cannot clash.
    pub fn entry_names(&self) -> std::collections::HashSet<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// Whether the entry at `path` navigates like a directory.
    pub fn is_dir_like(&self, path: &VfsPath) -> Option<bool> {
        let row = *self.row_of_path.get(path)?;
        self.entry_at(row).map(|e| e.is_dir_like())
    }

    // ------------------------------------------------------------------
    // Selection

    /// Select exactly one row (plain click, arrow-key move).
    pub fn select_only(&mut self, row: usize) {
        let Some(path) = self.path_at(row) else {
            return;
        };
        self.selection.clear();
        self.selection.insert(path.clone());
        self.anchor = Some(path.clone());
        self.cursor = Some(path);
        self.refresh_selection_summary();
    }

    /// Toggle one row in the selection (Cmd/Ctrl-click).
    pub fn toggle_select(&mut self, row: usize) {
        let Some(path) = self.path_at(row) else {
            return;
        };
        if !self.selection.remove(&path) {
            self.selection.insert(path.clone());
        }
        self.anchor = Some(path.clone());
        self.cursor = Some(path);
        self.refresh_selection_summary();
    }

    /// Extend the selection from the anchor through `row` (Shift-click).
    pub fn extend_select(&mut self, row: usize) {
        let Some(anchor_row) = self
            .anchor
            .as_ref()
            .and_then(|p| self.row_of_path.get(p))
            .copied()
        else {
            self.select_only(row);
            return;
        };
        let (lo, hi) = if anchor_row <= row {
            (anchor_row, row)
        } else {
            (row, anchor_row)
        };
        self.selection.clear();
        for r in lo..=hi {
            if let Some(e) = self.entry_at(r) {
                self.selection.insert(e.path.clone());
            }
        }
        self.cursor = self.path_at(row);
        self.refresh_selection_summary();
    }

    /// Right-click on an unselected row selects it first (Finder behaviour).
    pub fn prepare_context_menu(&mut self, row: usize) {
        if !self.is_row_selected(row) {
            self.select_only(row);
        }
    }

    pub fn select_all(&mut self) {
        self.selection.clear();
        for path in self
            .visible_entries()
            .map(|e| e.path.clone())
            .collect::<Vec<_>>()
        {
            self.selection.insert(path);
        }
        self.refresh_selection_summary();
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.selection.clear();
        self.refresh_selection_summary();
    }

    fn path_at(&self, row: usize) -> Option<VfsPath> {
        self.entry_at(row).map(|e| e.path.clone())
    }

    /// Rebuild the display-order portion of selection after a selection or
    /// visibility change. Rendering consumes the resulting scalar/vector and
    /// never scans the directory.
    pub(super) fn refresh_selection_summary(&mut self) {
        let mut paths = Vec::new();
        let mut bytes = 0;
        for entry in self.visible_entries() {
            if self.selection.contains(&entry.path) {
                paths.push(entry.path.clone());
                bytes += entry.display_len().unwrap_or(0);
            }
        }
        self.selected_visible_paths = paths.into();
        self.selected_visible_bytes = bytes;
    }
}

impl EventEmitter<FolderEvent> for Folder {}

trait OptionExt<T> {
    fn retain(&mut self, f: impl Fn(&T) -> bool);
}

impl<T> OptionExt<T> for Option<T> {
    fn retain(&mut self, f: impl Fn(&T) -> bool) {
        if self.as_ref().is_some_and(|v| !f(v)) {
            *self = None;
        }
    }
}
