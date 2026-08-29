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
//!    stored as [`EntryId`]s, never as display rows. A watcher reload
//!    reshuffles rows; the user's selection must not follow the shuffle.
//!
//! Rule 2 used to be spelled "as paths, never indices" (D-36). The property
//! it was protecting is *stability under re-sorting*, and an `EntryId` has
//! that while costing a `u32` hash instead of a heap allocation and a hash of
//! a sixty-byte string. What an id does not survive is a reload, which throws
//! the whole slab away — so a reload carries the user's selection across as
//! paths and re-resolves it. See [`CarriedState`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ferret_settings::{SortKey, SortOrder};
use ferret_vfs::{Entry, Vfs, VfsError, VfsPath, WatchEvent};
use futures::StreamExt;
use gpui::{Context, EventEmitter};
use smol_str::SmolStr;

mod expansion;
mod listing;
mod sort;

#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;

/// The hasher for every index in this module.
///
/// These maps are keyed on [`EntryId`]s and on names read from the user's own
/// disk, never on attacker-chosen input, so SipHash was buying HashDoS
/// resistance against a threat model Ferret does not have — at a cost paid on
/// every watcher burst. See `docs/audit.html` §9; a map keyed on
/// server-supplied names would want a seeded hasher instead.
type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;
type FastSet<T> = HashSet<T, foldhash::fast::RandomState>;

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

/// What a consumer of a listing actually needs from it (A-14).
///
/// A `Folder` had one lifecycle and every consumer paid for all of it.
/// Pressing Cmd-2 in a large directory built three ancestor `Folder`s and each
/// one attached an OS watcher, started a full metadata backfill and resolved
/// platform icons — for a column that draws an icon, a name and a chevron, and
/// that is thrown away the moment you leave column view.
///
/// The same seam is what Phase 2's terminal will want when it needs to know
/// about a directory it is not rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListingProfile {
    /// Fill in size and modification dates. The Size and Date columns need
    /// this; a listing that draws neither does not, and on macOS it is one
    /// `stat` per entry.
    pub metadata: bool,
    /// Keep the listing live against the filesystem. Every watch is a
    /// subscription in the registry, and on macOS registering one briefly
    /// interrupts delivery to every other watched directory
    /// (`ferret_vfs::watch`), so a listing nobody is editing should not hold
    /// one.
    pub watch: bool,
    /// Ask the platform for real Finder/Explorer icons. Deduplicated by type,
    /// but still a pass over the listing.
    pub icons: bool,
}

impl ListingProfile {
    /// Everything: the directory the user is actually browsing.
    pub const BROWSING: Self = Self {
        metadata: true,
        watch: true,
        icons: true,
    };

    /// Names and icons, no metadata and no watch — an ancestor column in
    /// column view, which exists to be read as a path rather than worked in.
    /// Icons stay because the column draws them.
    pub const NAVIGATING: Self = Self {
        metadata: false,
        watch: false,
        icons: true,
    };
}

/// The stable identity of one loaded entry within one [`Folder`] (D-36).
///
/// Not a display row, and not an index into any listing: it is a slot in the
/// folder's slab, and sorting, filtering and expanding never touch it. That is
/// exactly the property the module's selection rule was protecting, which is
/// why selection, cursor, anchor and expansion all key on this.
///
/// Ids are scoped to one `Folder` and to one listing of it. A reload allocates
/// a fresh set, so nothing may hold one across a `start_loading`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(u32);

/// One visible row, after the tree has been flattened.
///
/// The flat model is a deliberate choice (§6 leaves it open): the table is
/// already virtualised over this vector, and a separate tree widget would
/// mean a second row model and a second proof of Gate 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    /// Which entry the row draws.
    id: EntryId,
    /// How many folders deep, for the row's indent.
    depth: u16,
}

/// Whether an entry belongs to the browsed directory or to an expanded one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// A direct child of `self.dir`, and therefore indexed by name.
    Top,
    /// A child of an expanded folder. Names are only unique within their own
    /// parent, so these stay out of the name index.
    Nested,
}

/// The user's identity state, held by path across a reload.
///
/// Selection, cursor and expansion belong to the person, and a refresh must
/// not discard them — but every [`EntryId`] they were expressed in belongs to
/// the listing being thrown away. `start_loading` converts them to paths and
/// `apply_batch` resolves each one back as its entry lands, so a row is
/// already painted selected the moment it reappears.
#[derive(Default)]
struct CarriedState {
    selection: HashSet<VfsPath>,
    anchor: Option<VfsPath>,
    cursor: Option<VfsPath>,
    expanded: HashSet<VfsPath>,
}

impl CarriedState {
    fn is_empty(&self) -> bool {
        self.selection.is_empty()
            && self.anchor.is_none()
            && self.cursor.is_none()
            && self.expanded.is_empty()
    }
}

pub struct Folder {
    pub dir: VfsPath,

    /// Every loaded entry, addressed by [`EntryId`]. Never reordered.
    ///
    /// A vacant slot is a removed entry whose id has gone back to `free`, so
    /// a directory that churns for hours does not grow this without bound.
    slab: Vec<Option<Entry>>,
    /// Slots ready to be reused.
    free: Vec<u32>,

    /// The browsed directory's own listing, sorted. Sorting permutes `u32`s.
    entries: Vec<EntryId>,
    /// Loaded listings of expanded folders, keyed by the folder.
    ///
    /// Kept beside `entries` rather than spliced into it so that sorting,
    /// the watcher and the metadata backfill all keep working on the flat
    /// listing they were written for.
    children: FastMap<EntryId, Vec<EntryId>>,
    /// Folders the user has expanded in place (§8).
    expanded: FastSet<EntryId>,
    /// Visible rows only, in display order, flattened depth-first.
    order: Vec<Row>,
    /// Id → display row, for the visible rows only. Rebuilt per reindex,
    /// which is now a pass over `u32`s into a map that keeps its capacity.
    row_of_id: FastMap<EntryId, usize>,

    /// Path → id, for every loaded entry, visible or filtered out.
    ///
    /// Maintained as entries arrive and leave rather than rebuilt per watcher
    /// burst (A-05) — that rebuild was ~n allocations and n hashes of a full
    /// path string for every file saved. It exists because everything above
    /// this module speaks in paths: drags, drops, the path bar and the rename
    /// overlay all name an entry that way.
    by_path: FastMap<VfsPath, EntryId>,
    /// Name → id, for the browsed directory's own listing only.
    ///
    /// Every entry in `entries` shares one parent, so the name is already a
    /// complete key inside this map, and it is an order of magnitude shorter
    /// to hash than a full path. The watcher resolves probes through this, and
    /// so does the "is this name taken?" behind new-file naming (A-23).
    by_name: FastMap<SmolStr, EntryId>,

    loading: bool,
    error: Option<VfsError>,

    sort_key: SortKey,
    sort_order: SortOrder,
    show_hidden: bool,
    filter: Option<sort::NameFilter>,
    /// Which parts of the listing lifecycle this folder runs (A-14).
    profile: ListingProfile,

    selection: FastSet<EntryId>,
    /// Selected paths in display order. This is kept separately from
    /// `selection` because selection survives filtering, while commands and
    /// the status bar operate only on rows the person can currently see.
    selected_visible_paths: Arc<[VfsPath]>,
    /// Cached alongside `selected_visible_paths` so rendering the status bar
    /// never has to walk a large listing.
    selected_visible_bytes: u64,
    anchor: Option<EntryId>,
    cursor: Option<EntryId>,

    /// Identity to restore as the current listing arrives; see
    /// [`CarriedState`]. Empty except during a reload.
    carried: CarriedState,

    /// Bumps on every navigation/reload; background tasks check it before
    /// touching state, which is how stale walks cancel themselves.
    generation: u64,
    watch: Option<ferret_vfs::Watch>,
    last_meta_notify: Option<Instant>,
    /// When the current listing began, for the slow-start hint.
    loading_since: Option<Instant>,
}

impl Folder {
    pub fn new(
        dir: VfsPath,
        sort_key: SortKey,
        sort_order: SortOrder,
        show_hidden: bool,
        profile: ListingProfile,
    ) -> Self {
        Self {
            dir,
            profile,
            slab: Vec::new(),
            free: Vec::new(),
            entries: Vec::new(),
            children: FastMap::default(),
            expanded: FastSet::default(),
            order: Vec::new(),
            row_of_id: FastMap::default(),
            by_path: FastMap::default(),
            by_name: FastMap::default(),
            loading: true,
            error: None,
            sort_key,
            sort_order,
            show_hidden,
            filter: None,
            selection: FastSet::default(),
            selected_visible_paths: Arc::from([]),
            selected_visible_bytes: 0,
            anchor: None,
            cursor: None,
            carried: CarriedState::default(),
            generation: 0,
            watch: None,
            last_meta_notify: None,
            loading_since: None,
        }
    }

    // ------------------------------------------------------------------
    // The slab

    /// Put `entry` in the slab and index it, returning its stable id.
    fn insert(&mut self, entry: Entry, placement: Placement) -> EntryId {
        let path = entry.path.clone();
        let name = entry.name_handle().clone();
        let id = match self.free.pop() {
            Some(slot) => {
                self.slab[slot as usize] = Some(entry);
                EntryId(slot)
            }
            None => {
                self.slab.push(Some(entry));
                EntryId((self.slab.len() - 1) as u32)
            }
        };
        self.by_path.insert(path, id);
        if placement == Placement::Top {
            self.by_name.insert(name, id);
        }
        id
    }

    /// Drop one entry and every reference to it.
    ///
    /// The identity state goes with it here rather than in a sweep afterwards:
    /// a selection that outlived its row is what "the wrong file got deleted"
    /// is made of, and the removal is the only place that knows for certain.
    fn remove(&mut self, id: EntryId) -> Option<Entry> {
        let entry = self.slab.get_mut(id.0 as usize)?.take()?;
        self.free.push(id.0);
        self.by_path.remove(&entry.path);
        // Guarded: a nested entry may share a name with a top-level one, and
        // removing it must not unindex the row that still exists.
        if self.by_name.get(entry.name_handle()) == Some(&id) {
            self.by_name.remove(entry.name_handle());
        }
        self.selection.remove(&id);
        self.expanded.remove(&id);
        self.children.remove(&id);
        if self.anchor == Some(id) {
            self.anchor = None;
        }
        if self.cursor == Some(id) {
            self.cursor = None;
        }
        Some(entry)
    }

    fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.slab.get(id.0 as usize)?.as_ref()
    }

    fn entry_by_id_mut(&mut self, id: EntryId) -> Option<&mut Entry> {
        self.slab.get_mut(id.0 as usize)?.as_mut()
    }

    fn id_of(&self, path: &VfsPath) -> Option<EntryId> {
        self.by_path.get(path).copied()
    }

    fn id_at(&self, row: usize) -> Option<EntryId> {
        self.order.get(row).map(|r| r.id)
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
        self.entry(self.id_at(row)?)
    }

    /// Indent level of a row: 0 for the browsed directory's own entries.
    pub fn depth_at(&self, row: usize) -> u16 {
        self.order.get(row).map(|r| r.depth).unwrap_or(0)
    }

    /// Whether this row is a folder that can be expanded in place, and
    /// whether it currently is.
    pub fn expansion_at(&self, row: usize) -> Option<bool> {
        let id = self.id_at(row)?;
        self.entry(id)?
            .is_dir_like()
            .then(|| self.expanded.contains(&id))
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

    pub fn filter(&self) -> Option<&str> {
        self.filter.as_ref().map(sort::NameFilter::needle)
    }

    /// Whether the row at `row` is in the selection.
    pub fn is_row_selected(&self, row: usize) -> bool {
        self.id_at(row)
            .is_some_and(|id| self.selection.contains(&id))
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
    fn visible_entries(&self) -> impl Iterator<Item = (EntryId, &Entry)> {
        self.order
            .iter()
            .filter_map(|row| Some((row.id, self.entry(row.id)?)))
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
        self.row_of_id.get(&self.cursor?).copied()
    }

    /// The cursor row's path, for Open/Rename actions.
    pub fn cursor_path(&self) -> Option<VfsPath> {
        Some(self.entry(self.cursor?)?.path.clone())
    }

    pub fn has_entry(&self, path: &VfsPath) -> bool {
        self.by_path.contains_key(path)
    }

    /// Whether the browsed directory already holds something called `name`.
    ///
    /// One hash of a short key, answered from an index the listing already
    /// maintains. It replaces a `HashSet<String>` built out of every name in
    /// the directory — 200,000 allocations to answer one yes-or-no question,
    /// on the UI thread, four times over (A-23).
    pub fn contains_name(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Display row of `path`, or `None` when it is filtered out or absent.
    pub fn row_of(&self, path: &VfsPath) -> Option<usize> {
        self.row_of_id.get(&self.id_of(path)?).copied()
    }

    /// The one selected entry when exactly one visible row is selected.
    pub fn selected_entry(&self) -> Option<&Entry> {
        let [only] = self.selected_visible_paths.as_ref() else {
            return None;
        };
        self.entry(self.id_of(only)?)
    }

    /// Whether the entry at `path` navigates like a directory.
    pub fn is_dir_like(&self, path: &VfsPath) -> Option<bool> {
        Some(self.entry(self.id_of(path)?)?.is_dir_like())
    }

    // ------------------------------------------------------------------
    // Selection

    /// Select exactly one row (plain click, arrow-key move).
    pub fn select_only(&mut self, row: usize) {
        let Some(id) = self.id_at(row) else {
            return;
        };
        self.selection.clear();
        self.selection.insert(id);
        self.anchor = Some(id);
        self.cursor = Some(id);
        self.refresh_selection_summary();
    }

    /// Toggle one row in the selection (Cmd/Ctrl-click).
    pub fn toggle_select(&mut self, row: usize) {
        let Some(id) = self.id_at(row) else {
            return;
        };
        if !self.selection.remove(&id) {
            self.selection.insert(id);
        }
        self.anchor = Some(id);
        self.cursor = Some(id);
        self.refresh_selection_summary();
    }

    /// Extend the selection from the anchor through `row` (Shift-click).
    pub fn extend_select(&mut self, row: usize) {
        let Some(anchor_row) = self.anchor.and_then(|id| self.row_of_id.get(&id)).copied() else {
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
            if let Some(id) = self.id_at(r) {
                self.selection.insert(id);
            }
        }
        self.cursor = self.id_at(row);
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
        self.selection.extend(self.order.iter().map(|row| row.id));
        self.refresh_selection_summary();
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.selection.clear();
        self.refresh_selection_summary();
    }

    /// Rebuild the display-order portion of selection after a selection or
    /// visibility change. Rendering consumes the resulting scalar/vector and
    /// never scans the directory.
    pub(super) fn refresh_selection_summary(&mut self) {
        if self.selection.is_empty() {
            self.selected_visible_paths = Arc::from([]);
            self.selected_visible_bytes = 0;
            return;
        }
        let mut paths = Vec::new();
        let mut bytes = 0;
        for (id, entry) in self.visible_entries() {
            if self.selection.contains(&id) {
                paths.push(entry.path.clone());
                bytes += entry.display_len().unwrap_or(0);
            }
        }
        self.selected_visible_paths = paths.into();
        self.selected_visible_bytes = bytes;
    }
}

impl EventEmitter<FolderEvent> for Folder {}
