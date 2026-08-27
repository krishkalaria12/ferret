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
use std::time::{Duration, Instant, SystemTime};

use ferret_settings::{SortKey, SortOrder};
use ferret_vfs::{Entry, Vfs, VfsError, VfsPath, WatchEvent};
use futures::StreamExt;
use gpui::{Context, EventEmitter};

/// How many entries accumulate before a batch is flushed to the model.
///
/// Large enough that a 200,000-entry walk costs ~50 model updates rather than
/// 200,000; small enough that rows visibly stream in.
pub const BATCH_SIZE: usize = 4_096;

/// How often the metadata backfill reports progress to the UI.
pub const META_NOTIFY_EVERY: Duration = Duration::from_millis(120);

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
        self.visible_entries()
            .filter(|e| self.selection.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
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
        self.visible_entries()
            .filter(|e| self.selection.contains(&e.path))
            .count()
    }

    /// Sum of selected sizes. Directories contribute nothing rather than a
    /// lie (`docs/ui-base.html` §9).
    pub fn selection_size(&self) -> u64 {
        self.visible_entries()
            .filter(|e| self.selection.contains(&e.path))
            .filter_map(|e| e.display_len())
            .sum()
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
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.selection.clear();
    }

    fn path_at(&self, row: usize) -> Option<VfsPath> {
        self.entry_at(row).map(|e| e.path.clone())
    }

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
                .is_some_and(|e| e.name.to_lowercase().starts_with(&prefix));
            if matches {
                self.select_only(row);
                return true;
            }
        }
        false
    }

    fn resort(&mut self) {
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
    fn rebuild_index_and_order(&mut self) {
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
    fn entry_mut(&mut self, path: &VfsPath) -> Option<&mut Entry> {
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

    fn is_visible(&self, e: &Entry) -> bool {
        if !self.show_hidden && e.is_hidden() {
            return false;
        }
        if let Some(f) = &self.filter {
            return e.name.to_lowercase().contains(&f.to_lowercase());
        }
        true
    }

    // ------------------------------------------------------------------
    // Listing lifecycle (needs a GPUI context; driven by the pane)

    /// Begin (or restart) streaming the listing of `self.dir`.
    ///
    /// Runs on the GPUI executor; batches arrive via [`Self::apply_batch`]
    /// and completion via [`Self::finish_loading`]. Stale generations drop
    /// their work silently — navigation ahead of a slow walk is normal.
    pub fn start_loading(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;

        // Every index derived from the old listing goes with it. Leaving
        // `top_index` behind is not cosmetic: a watcher probe arriving during
        // the reload would look a stale path up, get an index that now points
        // at a different entry, and overwrite that one instead.
        self.entries.clear();
        self.order.clear();
        self.row_of_path.clear();
        self.top_index.clear();
        self.known_paths.clear();
        // The children are re-read once the parent listing lands; which
        // folders are *open* is the user's intent and survives a refresh.
        self.children.clear();
        self.error = None;
        self.loading = true;
        self.loading_since = Some(Instant::now());
        // Keep the selection paths: a watcher reload must preserve them even
        // though rows will be rebuilt.
        self.watch = None;
        cx.notify();

        let fs = cx.global::<crate::state::FerretState>().fs;
        let dir = self.dir.clone();

        // Nothing else will wake the view once the delay passes — the whole
        // point of the hint is that no rows have arrived — so the deadline
        // schedules its own repaint.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_HINT_DELAY).await;
            let _ = this.update(cx, |folder, cx| {
                if folder.generation == generation && folder.is_slow_to_start() {
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&dir);
            let mut buffer: Vec<Entry> = Vec::new();
            let mut first_error: Option<VfsError> = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(entry) => buffer.push(entry),
                    Err(err) => {
                        // A failed open ends the stream; a bad child does not.
                        first_error = Some(err);
                        break;
                    }
                }
                if buffer.len() >= BATCH_SIZE {
                    let batch = std::mem::take(&mut buffer);
                    let ok = this.update(cx, |folder, cx| {
                        if folder.generation != generation {
                            return false;
                        }
                        folder.apply_batch(batch, cx);
                        true
                    });
                    if ok.is_err() {
                        return; // Folder dropped: stop walking.
                    }
                }
            }

            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation {
                    return;
                }
                if !buffer.is_empty() {
                    folder.apply_batch(buffer, cx);
                }
                folder.finish_loading(first_error, cx);
            });
        })
        .detach();
    }

    fn apply_batch(&mut self, batch: Vec<Entry>, cx: &mut Context<Self>) {
        // Appended, never rebuilt: a full re-index per batch is O(n) forty
        // times over on a 200,000-entry walk, which is Gate 1 gone. Arrival
        // order *is* display order while loading, and nothing can be expanded
        // yet, so every new row is a flat one at the end.
        for entry in batch {
            let ix = self.entries.len();
            let path = entry.path.clone();
            let visible = self.is_visible(&entry);
            self.entries.push(entry);
            self.top_index.insert(path.clone(), ix);
            self.known_paths.insert(path.clone());
            if visible {
                self.row_of_path.insert(path, self.order.len());
                self.order.push(Row {
                    source: RowSource::Top(ix),
                    depth: 0,
                });
            }
        }
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn finish_loading(&mut self, error: Option<VfsError>, cx: &mut Context<Self>) {
        self.loading = false;
        self.loading_since = None;
        if error.as_ref().is_some_and(VfsError::is_gone) {
            self.error = Some(VfsError::NotFound {
                path: self.dir.clone(),
            });
            cx.emit(FolderEvent::Gone);
            cx.notify();
            return;
        }
        self.error = error;
        self.resort();
        self.attach_watch(cx);
        self.reload_expanded_children(cx);
        self.start_metadata_backfill(cx);
        self.request_os_icons(cx);
        cx.emit(FolderEvent::LoadFinished);
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn attach_watch(&mut self, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        let watch = match fs.watch(&self.dir) {
            Ok(watch) => watch,
            // No notification is a degraded mode, not a failure: manual
            // refresh still works.
            Err(_) => return,
        };
        let rx = watch.events.clone();
        self.watch = Some(watch);

        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                // Debounce bursts: editors rewrite files several times in a
                // few tens of milliseconds.
                cx.background_executor()
                    .timer(ferret_vfs::watch::DEBOUNCE)
                    .await;

                let mut paths: Vec<std::path::PathBuf> = Vec::new();
                let mut gone = false;
                match event {
                    WatchEvent::Changed(changed) => paths.extend(changed),
                    WatchEvent::Gone => gone = true,
                }
                // Drain whatever else piled up behind the debounce.
                while let Ok(more) = rx.try_recv() {
                    match more {
                        WatchEvent::Changed(changed) => paths.extend(changed),
                        WatchEvent::Gone => gone = true,
                    }
                }

                let ok = this.update(cx, |folder, cx| {
                    if folder.generation != generation {
                        return false;
                    }
                    if gone {
                        folder.mark_gone(cx);
                    } else if !paths.is_empty() {
                        // A huge burst is cheaper to answer with one clean
                        // walk than with hundreds of stats.
                        if paths.len() > WATCH_RELOAD_THRESHOLD {
                            folder.reload_preserving_state(cx);
                        } else {
                            folder.start_watch_apply(paths, cx);
                        }
                    }
                    true
                });
                if ok.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Turn watcher paths into row updates: one `probe` per path, applied in
    /// a single model update.
    fn start_watch_apply(&mut self, mut paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        let generation = self.generation;
        let dir = self.dir.clone();

        cx.spawn(async move |this, cx| {
            let mut probed = Vec::with_capacity(paths.len());
            for path in paths.drain(..) {
                let under_dir = dir
                    .as_local()
                    .zip(path.parent())
                    .is_some_and(|(d, parent)| parent == d);
                if !under_dir {
                    continue;
                }
                let result = fs.probe(&VfsPath::local(path.clone())).await;
                probed.push((path, result));
            }
            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation {
                    return;
                }
                folder.integrate_probes(probed, cx);
            });
        })
        .detach();
    }

    fn integrate_probes(
        &mut self,
        probed: Vec<(std::path::PathBuf, ferret_vfs::Result<Entry>)>,
        cx: &mut Context<Self>,
    ) {
        if self.apply_probes(probed) {
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        }
    }

    // ------------------------------------------------------------------
    // Inline expansion (§8)

    /// Expand or collapse the folder at `row` in place, without changing the
    /// browsed directory.
    ///
    /// Returns whether anything happened, so callers can tell a no-op on a
    /// file row from a real toggle.
    pub fn toggle_expanded(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.entry_at(row) else {
            return false;
        };
        if !entry.is_dir_like() {
            return false;
        }
        let path = entry.path.clone();

        if self.expanded.remove(&path) {
            // Collapsing drops the loaded children: re-reading on the next
            // expand is cheap, and holding them would keep a deep tree's
            // worth of entries alive for a row nobody can see.
            self.forget_subtree(&path);
            self.rebuild_index_and_order();
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
            return true;
        }

        self.expanded.insert(path.clone());
        self.load_children(path, cx);
        cx.notify();
        true
    }

    /// Expand a folder row, or do nothing if it already is (→ key).
    pub fn expand(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        match self.expansion_at(row) {
            Some(false) => self.toggle_expanded(row, cx),
            _ => false,
        }
    }

    /// Collapse a folder row, or do nothing if it is not expanded (← key).
    pub fn collapse(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        match self.expansion_at(row) {
            Some(true) => self.toggle_expanded(row, cx),
            _ => false,
        }
    }

    /// Drop a collapsed folder's children, and its descendants' with it.
    fn forget_subtree(&mut self, parent: &VfsPath) {
        let Some(children) = self.children.remove(parent) else {
            return;
        };
        for child in children {
            if self.expanded.remove(&child.path) {
                self.forget_subtree(&child.path);
            }
        }
    }

    /// Read one expanded folder's listing.
    ///
    /// Collected rather than streamed: a child listing is spliced into the
    /// middle of the display order, so rows arriving one batch at a time
    /// would shift everything below them repeatedly.
    fn load_children(&mut self, parent: VfsPath, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        let generation = self.generation;
        let (key, order) = (self.sort_key, self.sort_order);

        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&parent);
            let mut entries = Vec::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(entry) => entries.push(entry),
                    // An unreadable child folder expands to nothing rather
                    // than failing the whole listing it sits in.
                    Err(_) => break,
                }
            }
            entries.sort_by(|a, b| cmp_entries(a, b, key, order));

            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation || !folder.expanded.contains(&parent) {
                    return; // Navigated away, or collapsed while loading.
                }
                folder.children.insert(parent, entries);
                folder.rebuild_index_and_order();
                folder.start_metadata_backfill(cx);
                folder.request_os_icons(cx);
                cx.emit(FolderEvent::RowsChanged);
                cx.notify();
            });
        })
        .detach();
    }

    /// The pure model half of [`Self::integrate_probes`]; returns whether
    /// anything changed.
    fn apply_probes(
        &mut self,
        probed: Vec<(std::path::PathBuf, ferret_vfs::Result<Entry>)>,
    ) -> bool {
        let mut changed = false;
        let mut metadata_changed = false;

        for (path, result) in probed {
            let vp = VfsPath::local(path);
            match result {
                Ok(entry) => match self.top_index.get(&vp).copied() {
                    Some(ix) => {
                        if let Some(existing) = self.entries.get_mut(ix) {
                            if existing.metadata != entry.metadata {
                                metadata_changed = true;
                            }
                            *existing = entry;
                            changed = true;
                        }
                    }
                    None => {
                        // Keep `entries` sorted; insert at the sorted spot.
                        let pos = self.entries.partition_point(|existing| {
                            cmp_entries(existing, &entry, self.sort_key, self.sort_order)
                                == std::cmp::Ordering::Less
                        });
                        self.entries.insert(pos, entry);
                        changed = true;
                    }
                },
                Err(err) if err.is_gone() => {
                    if let Some(ix) = self.top_index.get(&vp).copied() {
                        self.entries.remove(ix);
                        changed = true;
                    }
                }
                // Transient stat failures leave the row as it was; the next
                // event or a manual refresh corrects it.
                Err(_) => {}
            }
        }

        if !changed {
            return false;
        }
        // Size/Date ordering can be invalidated by metadata that arrived
        // after the sort; Name order never is.
        if metadata_changed
            && matches!(
                self.sort_key,
                ferret_settings::SortKey::Modified | ferret_settings::SortKey::Size
            )
        {
            self.entries
                .sort_by(|a, b| cmp_entries(a, b, self.sort_key, self.sort_order));
        }
        self.rebuild_index_and_order();
        true
    }

    fn mark_gone(&mut self, cx: &mut Context<Self>) {
        self.error = Some(VfsError::NotFound {
            path: self.dir.clone(),
        });
        self.loading = false;
        self.watch = None;
        cx.emit(FolderEvent::Gone);
        cx.notify();
    }

    /// Re-list the same directory, keeping selection and cursor by path.
    pub fn reload_preserving_state(&mut self, cx: &mut Context<Self>) {
        self.start_loading(cx);
    }

    /// Re-read the listings of folders that were open before a reload.
    ///
    /// `start_loading` drops the cached children but keeps `expanded`, so a
    /// refresh has to fetch them again — otherwise a folder stays marked open
    /// with nothing under it. Only the ones that still exist are re-read; the
    /// rest are forgotten here, since nothing else will.
    fn reload_expanded_children(&mut self, cx: &mut Context<Self>) {
        if self.expanded.is_empty() {
            return;
        }
        // A folder that survived the reload is worth re-opening; one that did
        // not is no longer expandable.
        let (alive, gone): (Vec<VfsPath>, Vec<VfsPath>) = self
            .expanded
            .iter()
            .cloned()
            .partition(|p| self.known_paths.contains(p));
        for path in gone {
            self.expanded.remove(&path);
        }
        for path in alive {
            self.load_children(path, cx);
        }
    }

    /// Ask the platform for the real icons of the types now listed (D-18).
    ///
    /// Deduplicated by type inside the provider, so this is a handful of OS
    /// calls even for a 200,000-entry directory, and it runs off the render
    /// path — a row draws the bundled icon until the real one lands.
    fn request_os_icons(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .chain(self.children.values().flatten())
            .cloned()
            .collect();
        cx.spawn(async move |this, cx| {
            cx.update(|cx| crate::icons::resolve_os_icons(&entries, cx));
            // The provider fills its cache in the background; one repaint
            // shortly after is enough to pick the results up.
            cx.background_executor().timer(OS_ICON_REPAINT_DELAY).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn start_metadata_backfill(&mut self, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        if fs.capabilities().metadata_in_listing {
            return; // Windows filled it during the walk.
        }

        let generation = self.generation;
        let targets: Vec<VfsPath> = self
            .entries
            .iter()
            .chain(self.children.values().flatten())
            .filter(|e| e.metadata.is_none())
            .map(|e| e.path.clone())
            .collect();
        if targets.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            const CHUNK: usize = 256;
            for chunk in targets.chunks(CHUNK) {
                let mut filled: Vec<(VfsPath, ferret_vfs::Metadata)> =
                    Vec::with_capacity(chunk.len());
                for path in chunk {
                    if let Ok(md) = fs.metadata(path).await {
                        filled.push((path.clone(), md));
                    }
                }
                let done = targets.last() == chunk.last();
                let ok = this.update(cx, |folder, cx| {
                    if folder.generation != generation {
                        return false;
                    }
                    folder.apply_metadata(filled, done, cx);
                    true
                });
                if ok.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn apply_metadata(
        &mut self,
        filled: Vec<(VfsPath, ferret_vfs::Metadata)>,
        final_chunk: bool,
        cx: &mut Context<Self>,
    ) {
        for (path, md) in filled {
            if let Some(entry) = self.entry_mut(&path) {
                entry.metadata = Some(md);
            }
        }

        let now = Instant::now();
        let due = self
            .last_meta_notify
            .map(|t| now.duration_since(t) >= META_NOTIFY_EVERY)
            .unwrap_or(true);
        if due || final_chunk {
            self.last_meta_notify = Some(now);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        }
    }
}

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

impl EventEmitter<FolderEvent> for Folder {}

/// Directories sort above files regardless of the sort key
/// (`settings.browser.directories_first`).
fn cmp_entries(a: &Entry, b: &Entry, key: SortKey, order: SortOrder) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a.is_dir_like(), b.is_dir_like()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    let ord = match key {
        SortKey::Name => crate::format::natural_compare(&a.name, &b.name),
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
            .then_with(|| crate::format::natural_compare(&a.name, &b.name)),
    };

    let ord = match order {
        SortOrder::Ascending => ord,
        SortOrder::Descending => ord.reverse(),
    };

    // Stable, sensible tiebreak so equal keys keep names findable.
    ord.then_with(|| crate::format::natural_compare(&a.name, &b.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, Metadata, SymlinkTarget};

    fn entry(name: &str, kind: EntryKind, len: u64, modified: Option<SystemTime>) -> Entry {
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind,
            metadata: Some(Metadata {
                len,
                modified,
                // Mirror what LocalFs produces: the leading dot is hidden.
                hidden: name.starts_with('.'),
                ..Default::default()
            }),
            link_target: None,
        }
    }

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn populated() -> Folder {
        let mut f = Folder::new(
            VfsPath::local("/x"),
            SortKey::Name,
            SortOrder::Ascending,
            false,
        );
        f.entries = vec![
            entry("report.pdf", EntryKind::File, 500, t(300)),
            entry("zebra.txt", EntryKind::File, 100, t(200)),
            entry("src", EntryKind::Dir, 0, t(100)),
            entry(".hidden", EntryKind::File, 10, t(400)),
        ];
        // Tests skip the streaming phase; sort as `finish_loading` would.
        f.loading = false;
        f.resort();
        f
    }

    #[test]
    fn hidden_entries_are_filtered_until_shown() {
        let f = populated();
        assert_eq!(f.row_count(), 3);
        assert_eq!(f.entry_at(0).unwrap().name, "src");

        let mut f = populated();
        f.set_show_hidden(true);
        assert_eq!(f.row_count(), 4);
    }

    #[test]
    fn folders_come_first_then_natural_name_order() {
        let mut f = populated();
        f.set_show_hidden(true);
        let names: Vec<_> = (0..f.row_count())
            .map(|r| f.entry_at(r).unwrap().name.clone())
            .collect();
        assert_eq!(names, ["src", ".hidden", "report.pdf", "zebra.txt"]);
    }

    #[test]
    fn size_sort_uses_display_len_and_reverses() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.set_sort(SortKey::Size, SortOrder::Ascending);
        assert_eq!(f.entry_at(0).unwrap().name, "src"); // dirs pinned first
        assert_eq!(
            f.entry_at(1).unwrap().display_len(),
            Some(10),
            "smallest file first"
        );

        f.set_sort(SortKey::Size, SortOrder::Descending);
        assert_eq!(f.entry_at(1).unwrap().display_len(), Some(500));
    }

    #[test]
    fn modified_sort_orders_by_time() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.set_sort(SortKey::Modified, SortOrder::Ascending);
        assert_eq!(f.entry_at(0).unwrap().name, "src");
        assert_eq!(f.entry_at(1).unwrap().name, "zebra.txt");
        assert_eq!(f.entry_at(f.row_count() - 1).unwrap().name, ".hidden");
    }

    #[test]
    fn selection_survives_resorting() {
        let mut f = populated();
        f.set_show_hidden(true);
        // Select "zebra" (last row).
        f.select_only(f.row_count() - 1);
        assert_eq!(f.selected_paths().len(), 1);

        // Flip the sort; zebra moves but stays selected.
        f.set_sort(SortKey::Name, SortOrder::Descending);
        let sel = &f.selected_paths()[0];
        assert!(sel.to_string().ends_with("zebra.txt"));
    }

    #[test]
    fn shift_click_extends_from_anchor() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.select_only(1); // .hidden
        f.extend_select(3); // report.pdf
        assert_eq!(f.selection_count(), 3);
        assert_eq!(f.selection_size(), 10 + 100 + 500);

        // Extend backwards past the anchor collapses onto the new range.
        f.extend_select(2);
        assert_eq!(f.selection_count(), 2);
    }

    #[test]
    fn cmd_click_toggles_without_disturbing_others() {
        let mut f = populated();
        f.select_only(0);
        f.toggle_select(2);
        assert_eq!(f.selection_count(), 2);
        f.toggle_select(2);
        assert_eq!(f.selection_count(), 1);
    }

    #[test]
    fn filter_narrows_and_reports_misses() {
        let mut f = populated();
        f.set_filter(Some("PDF".into()));
        assert_eq!(f.row_count(), 1);
        assert_eq!(f.entry_at(0).unwrap().name, "report.pdf");

        f.set_filter(Some("zzz".into()));
        assert_eq!(f.row_count(), 0);
        // And clearing restores.
        f.set_filter(None);
        assert_eq!(f.row_count(), 3);
    }

    #[test]
    fn shift_arrow_grows_and_shrinks_against_a_fixed_anchor() {
        let mut f = populated();
        f.set_show_hidden(true); // src, .hidden, report.pdf, zebra.txt
        f.select_only(1); // anchor on .hidden

        f.extend_cursor(1);
        f.extend_cursor(1);
        assert_eq!(f.selection_count(), 3, "grew to rows 1..=3");

        // Reversing shrinks the range rather than starting a new one, which
        // is the whole difference from a plain arrow.
        f.extend_cursor(-1);
        assert_eq!(f.selection_count(), 2);

        // Past the anchor it grows the other way.
        f.extend_cursor(-1);
        f.extend_cursor(-1);
        assert_eq!(f.selection_count(), 2, "rows 0..=1");
        assert_eq!(f.cursor_row(), Some(0));
    }

    /// Expand a folder without touching the executor, by planting its
    /// children directly — the model half of what `load_children` does.
    fn expand_with(f: &mut Folder, parent: &str, children: Vec<Entry>) {
        let parent = VfsPath::local(parent);
        f.expanded.insert(parent.clone());
        f.children.insert(parent, children);
        f.rebuild_index_and_order();
    }

    #[test]
    fn expanded_children_are_spliced_in_beneath_their_parent() {
        let mut f = populated(); // src, report.pdf, zebra.txt (hidden off)
        assert_eq!(f.row_count(), 3);

        expand_with(
            &mut f,
            "/x/src",
            vec![
                entry("main.rs", EntryKind::File, 10, t(1)),
                entry("lib.rs", EntryKind::File, 20, t(2)),
            ],
        );

        // Children land directly under their parent, not at the end.
        let names: Vec<_> = (0..f.row_count())
            .map(|r| f.entry_at(r).unwrap().name.clone())
            .collect();
        assert_eq!(
            names,
            ["src", "main.rs", "lib.rs", "report.pdf", "zebra.txt"]
        );

        // And they are one level deeper, which is what the indent draws.
        assert_eq!(f.depth_at(0), 0);
        assert_eq!(f.depth_at(1), 1);
        assert_eq!(f.depth_at(3), 0);
    }

    #[test]
    fn only_folders_report_an_expansion_state() {
        let f = populated();
        assert_eq!(f.expansion_at(0), Some(false), "src is a collapsed folder");
        assert_eq!(f.expansion_at(1), None, "report.pdf is a file");
    }

    #[test]
    fn a_child_row_is_selectable_like_any_other() {
        let mut f = populated();
        expand_with(
            &mut f,
            "/x/src",
            vec![entry("main.rs", EntryKind::File, 10, t(1))],
        );

        f.select_only(1); // main.rs
        assert_eq!(f.selection_count(), 1);
        assert!(f.selected_paths()[0].to_string().ends_with("main.rs"));

        // Range selection spans the tree boundary without noticing it.
        f.extend_select(3);
        assert_eq!(f.selection_count(), 3);
    }

    #[test]
    fn collapsing_forgets_the_subtree_it_was_showing() {
        let mut f = populated();
        expand_with(
            &mut f,
            "/x/src",
            vec![entry("main.rs", EntryKind::File, 10, t(1))],
        );
        assert_eq!(f.row_count(), 4);

        // Collapse by hand, mirroring `toggle_expanded`'s model half.
        f.expanded.remove(&VfsPath::local("/x/src"));
        f.forget_subtree(&VfsPath::local("/x/src"));
        f.rebuild_index_and_order();

        assert_eq!(f.row_count(), 3);
        assert!(
            !f.has_entry(&VfsPath::local("/x/src/main.rs")),
            "a collapsed subtree is not still loaded"
        );
    }

    #[test]
    fn the_filter_reaches_into_expanded_children() {
        let mut f = populated();
        expand_with(
            &mut f,
            "/x/src",
            vec![
                entry("main.rs", EntryKind::File, 10, t(1)),
                entry("notes.txt", EntryKind::File, 20, t(2)),
            ],
        );

        f.set_filter(Some("rs".into()));
        let names: Vec<_> = (0..f.row_count())
            .map(|r| f.entry_at(r).unwrap().name.clone())
            .collect();
        // "src" does not itself match, but it is kept as the route to a child
        // that does — a filter that hid it would hide the match with it.
        assert_eq!(names, ["src", "main.rs"], "{names:?}");
    }

    /// The model half of `start_loading`, without the executor.
    fn begin_reload(f: &mut Folder) {
        f.entries.clear();
        f.order.clear();
        f.row_of_path.clear();
        f.top_index.clear();
        f.known_paths.clear();
        f.children.clear();
    }

    #[test]
    fn a_reload_leaves_no_index_pointing_at_the_old_listing() {
        let mut f = populated(); // src, report.pdf, zebra.txt
        expand_with(
            &mut f,
            "/x/src",
            vec![entry("main.rs", EntryKind::File, 10, t(1))],
        );
        assert!(f.has_entry(&VfsPath::local("/x/report.pdf")));

        begin_reload(&mut f);

        // Every derived index goes with the listing it described. A stale
        // `top_index` is the dangerous one: a watcher probe arriving mid-
        // reload would resolve an old path to an index that now points at a
        // different entry, and overwrite that one instead.
        assert!(f.top_index.is_empty(), "top_index must not survive");
        assert!(f.known_paths.is_empty(), "known_paths must not survive");
        assert!(f.children.is_empty(), "children must be re-read");
        assert!(!f.has_entry(&VfsPath::local("/x/report.pdf")));

        // Which folders are open is the user's intent, not derived state, so
        // a refresh must not collapse the tree.
        assert!(f.expanded.contains(&VfsPath::local("/x/src")));
    }

    #[test]
    fn a_probe_after_a_reload_edits_the_right_entry() {
        let mut f = populated();
        begin_reload(&mut f);

        // The new listing happens to put a different file where "src" was.
        f.entries = vec![
            entry("aaa.txt", EntryKind::File, 1, t(1)),
            entry("bbb.txt", EntryKind::File, 2, t(2)),
        ];
        f.rebuild_index_and_order();

        // A probe for a path from the *old* listing must find nothing rather
        // than land on whatever now sits at the old index.
        let changed = f.apply_probes(vec![(
            std::path::PathBuf::from("/x/zebra.txt"),
            Err(ferret_vfs::VfsError::NotFound {
                path: VfsPath::local("/x/zebra.txt"),
            }),
        )]);
        assert!(!changed, "a vanished stale path changes nothing");
        assert_eq!(f.row_count(), 2);
        assert_eq!(f.entry_at(0).unwrap().name, "aaa.txt");
        assert_eq!(f.entry_at(1).unwrap().name, "bbb.txt");
    }

    #[test]
    fn marquee_selection_replaces_rather_than_accumulates() {
        let mut f = populated();
        f.set_show_hidden(true);

        f.select_rows([0, 1]);
        assert_eq!(f.selection_count(), 2);

        // Dragging the band elsewhere selects only what it now covers — a
        // marquee that accumulated would make a downward drag unshrinkable.
        f.select_rows([3]);
        assert_eq!(f.selection_count(), 1);
        assert!(f.is_row_selected(3));

        // An empty band clears.
        f.select_rows([]);
        assert_eq!(f.selection_count(), 0);
    }

    #[test]
    fn extending_in_an_empty_folder_is_harmless() {
        let mut f = Folder::new(
            VfsPath::local("/x"),
            SortKey::Name,
            SortOrder::Ascending,
            false,
        );
        f.loading = false;
        assert_eq!(f.extend_cursor(1), None);
        assert_eq!(f.selection_count(), 0);
    }

    #[test]
    fn the_status_bar_never_counts_what_the_filter_hides() {
        let mut f = populated();
        f.select_all();
        assert_eq!(f.selection_count(), 3);

        // Narrowing to one match leaves the other two selected in the model,
        // but they are neither visible nor actionable, so they are not
        // reported — `selected_paths` is what Trash and Copy operate on.
        f.set_filter(Some("pdf".into()));
        assert_eq!(f.selection_count(), 1);
        assert_eq!(f.selection_size(), 500);
        assert_eq!(f.selected_paths().len(), 1);

        // Clearing the filter brings the rest back.
        f.set_filter(None);
        assert_eq!(f.selection_count(), 3);
    }

    #[test]
    fn type_ahead_finds_wrapping_matches() {
        let mut f = populated();
        f.set_show_hidden(true);
        // Visible order: src, .hidden, report.pdf, zebra.txt.
        assert!(f.select_by_prefix("rep", 0));
        assert_eq!(f.cursor_row(), Some(2));

        // From after the match it wraps around.
        assert!(f.select_by_prefix("z", 3));
        assert_eq!(f.cursor_row(), Some(3));

        assert!(!f.select_by_prefix("qq", 0));
    }

    #[test]
    fn watcher_probes_insert_and_remove_without_relisting() {
        let mut f = populated(); // src, report.pdf, zebra.txt (hidden off)

        // A new file lands: inserted at its sorted spot.
        let new_entry = entry("aaa.txt", EntryKind::File, 7, t(999));
        let changed = f.apply_probes(vec![(
            std::path::PathBuf::from("/x/aaa.txt"),
            Ok(new_entry),
        )]);
        assert!(changed);
        assert_eq!(f.row_count(), 4);
        assert_eq!(f.entry_at(1).unwrap().name, "aaa.txt"); // after src, before report

        // It vanishes: the row goes without a re-list.
        let changed = f.apply_probes(vec![(
            std::path::PathBuf::from("/x/aaa.txt"),
            Err(ferret_vfs::VfsError::NotFound {
                path: VfsPath::local("/x/aaa.txt"),
            }),
        )]);
        assert!(changed);
        assert_eq!(f.row_count(), 3);
    }

    #[test]
    fn selecting_out_of_range_is_harmless() {
        let mut f = populated();
        f.select_only(99);
        assert!(f.selection.is_empty());
        assert!(f.cursor.is_none());
    }

    #[test]
    fn symlinked_directories_sort_as_directories() {
        let mut f = Folder::new(
            VfsPath::local("/x"),
            SortKey::Name,
            SortOrder::Ascending,
            true,
        );
        f.entries = vec![
            entry("b.txt", EntryKind::File, 1, None),
            entry(
                "a-link",
                EntryKind::Symlink {
                    target_kind: Some(SymlinkTarget::Dir),
                },
                0,
                None,
            ),
        ];
        f.loading = false;
        f.resort();
        assert_eq!(f.entry_at(0).unwrap().name, "a-link");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use gpui::AppContext as _;
    use std::fs;

    #[gpui::test]
    async fn streams_a_listing_into_rows(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "ferret-ui-stream-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        for i in 0..50 {
            fs::write(root.join(format!("file-{i:03}.txt")), b"x").unwrap();
        }
        fs::create_dir(root.join("subdir")).unwrap();

        cx.update(|cx| {
            cx.set_global(crate::state::load_state());
        });

        let folder = cx.new(|_| {
            Folder::new(
                VfsPath::local(root.clone()),
                ferret_settings::SortKey::Name,
                ferret_settings::SortOrder::Ascending,
                true,
            )
        });
        folder.update(cx, |f, cx| f.start_loading(cx));

        // The listing producer runs on its own OS thread (smol::unblock), so
        // this is not a fully deterministic test: allow real threads, then
        // wait for the walk to land.
        cx.executor().allow_parking();
        let mut rows = 0;
        let mut loading = true;
        for _ in 0..600 {
            cx.run_until_parked();
            (rows, loading) = folder.read_with(cx, |f, _| (f.row_count(), f.is_loading()));
            if rows == 51 && !loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(rows, 51, "all entries must land");
        assert!(!loading, "loading must finish");

        // Dirs first, then natural name order.
        let first = folder.read_with(cx, |f, _| f.entry_at(0).unwrap().name.clone());
        assert_eq!(first, "subdir");

        fs::remove_dir_all(&root).ok();
    }
}
