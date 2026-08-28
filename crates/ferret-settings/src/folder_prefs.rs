//! Per-folder view preferences (F-06), as a bounded LRU.
//!
//! The requirement is "remember the folders I use", not "remember every folder
//! forever". Windows Explorer already caps its equivalent store and evicts old
//! entries; nobody notices a folder reverting after a year away.
//!
//! Three properties matter, and this shape is the cheapest way to get all
//! three:
//!
//! * **Lookup is synchronous and O(1).** The whole map is in memory. Anything
//!   that has to await here shows the wrong view mode for a frame before
//!   correcting itself, which is very visible.
//! * **The file is bounded by construction.** [`FOLDER_PREFS_CAPACITY`] entries,
//!   LRU-evicted, so it cannot grow without limit however long Ferret runs.
//! * **Writes are cheap.** Callers debounce and call [`FolderPrefs::to_json`]
//!   on the way out; a whole-file rewrite of a few hundred kilobytes is
//!   irrelevant at that rate.
//!
//! Deliberately *not* sidecar files. `.DS_Store` needs write permission the
//! user may not have, fails on read-only volumes and network shares, and
//! litters every directory it touches — including their repositories.
//!
//! Keys come from `VfsPath::preference_key()` in `ferret-vfs`, but this module
//! takes plain `&str` so settings never has to depend on the storage layer.

use crate::config::FOLDER_PREFS_CAPACITY;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// View/sort enums are the schema's, re-used here so there is exactly one
// `SortKey` in the crate. The store only records what the settings file
// already defines.
pub use crate::schema::{SortKey, SortOrder, ViewMode};

/// What is remembered about one directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderView {
    #[serde(default)]
    pub mode: ViewMode,
    #[serde(default)]
    pub sort_key: SortKey,
    #[serde(default)]
    pub sort_order: SortOrder,
    /// Columns hidden in this folder. Empty means the default set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_columns: Vec<String>,
}

impl FolderView {
    /// Whether this is worth a slot in the store at all.
    ///
    /// A folder left at every default is indistinguishable from one never
    /// visited, so storing it would only push a real preference out of the LRU.
    pub fn is_default(&self) -> bool {
        self == &FolderView::default()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    view: FolderView,
    /// Monotonic counter, not a clock: it survives timezone changes and system
    /// clock adjustments, both of which would otherwise scramble the LRU order.
    used: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    clock: u64,
    #[serde(default)]
    folders: HashMap<String, Record>,
}

/// The store. Load once at startup, keep it in memory, save debounced.
#[derive(Debug, Default)]
pub struct FolderPrefs {
    entries: HashMap<String, Record>,
    clock: u64,
    dirty: bool,
}

impl FolderPrefs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse the store, tolerating a corrupt or truncated file.
    ///
    /// View preferences are a convenience, so a damaged file costs the user
    /// their layouts and nothing else. Refusing to start over it would be a
    /// far worse failure than forgetting.
    pub fn from_json(text: &str) -> Self {
        let persisted: Persisted = serde_json::from_str(text).unwrap_or_default();
        let clock = persisted
            .folders
            .values()
            .map(|r| r.used)
            .max()
            .unwrap_or(0)
            .max(persisted.clock);
        let mut prefs = Self {
            entries: persisted.folders,
            clock,
            dirty: false,
        };
        // A file edited by hand, or written by a future version with a larger
        // capacity, must not stay over the limit.
        prefs.evict_to_capacity();
        prefs
    }

    /// Serialise the whole store.
    ///
    /// Compact, not pretty: the module header says this file is written by the
    /// app rather than by hand, and at [`FOLDER_PREFS_CAPACITY`] records
    /// pretty-printing roughly doubles the bytes and takes the slower
    /// serialiser, for a file nobody reads. `settings.json` is the one people
    /// edit, and that one is still formatted.
    pub fn to_json(&self) -> String {
        let persisted = Persisted {
            clock: self.clock,
            folders: self.entries.clone(),
        };
        serde_json::to_string(&persisted).unwrap_or_else(|_| "{}".into())
    }

    /// Look up a folder's view, marking it as recently used.
    ///
    /// Takes `&mut self` because reading is what keeps an entry alive — that is
    /// what makes this an LRU rather than a first-4000-wins cache.
    pub fn get(&mut self, key: &str) -> Option<FolderView> {
        self.clock += 1;
        let clock = self.clock;
        let record = self.entries.get_mut(key)?;
        record.used = clock;
        self.dirty = true;
        Some(record.view.clone())
    }

    /// Look up without disturbing the LRU order. For diagnostics and tests.
    pub fn peek(&self, key: &str) -> Option<&FolderView> {
        self.entries.get(key).map(|r| &r.view)
    }

    pub fn set(&mut self, key: impl Into<String>, view: FolderView) {
        let key = key.into();

        // Storing an all-defaults view would evict a real preference to record
        // the absence of one.
        if view.is_default() {
            if self.entries.remove(&key).is_some() {
                self.dirty = true;
            }
            return;
        }

        self.clock += 1;
        self.entries.insert(
            key,
            Record {
                view,
                used: self.clock,
            },
        );
        self.dirty = true;
        self.evict_to_capacity();
    }

    pub fn forget(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.dirty = true;
        }
    }

    /// Backs the "reset all folder views" action in settings.
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.dirty = true;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether anything has changed since the last [`FolderPrefs::mark_saved`].
    /// The debounced writer checks this so an idle app never rewrites the file.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
    }

    fn evict_to_capacity(&mut self) {
        if self.entries.len() <= FOLDER_PREFS_CAPACITY {
            return;
        }
        let excess = self.entries.len() - FOLDER_PREFS_CAPACITY;
        let mut by_age: Vec<(u64, String)> = self
            .entries
            .iter()
            .map(|(k, r)| (r.used, k.clone()))
            .collect();
        by_age.sort_unstable();
        for (_, key) in by_age.into_iter().take(excess) {
            self.entries.remove(&key);
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icons() -> FolderView {
        FolderView {
            mode: ViewMode::Icons,
            ..Default::default()
        }
    }

    fn by_size() -> FolderView {
        FolderView {
            sort_key: SortKey::Size,
            sort_order: SortOrder::Descending,
            ..Default::default()
        }
    }

    #[test]
    fn round_trips_through_json() {
        let mut prefs = FolderPrefs::new();
        prefs.set("/a", icons());
        prefs.set("/b", by_size());

        let restored = FolderPrefs::from_json(&prefs.to_json());
        assert_eq!(restored.peek("/a"), Some(&icons()));
        assert_eq!(restored.peek("/b"), Some(&by_size()));
    }

    #[test]
    fn default_views_are_not_stored() {
        let mut prefs = FolderPrefs::new();
        prefs.set("/a", FolderView::default());
        assert!(prefs.is_empty(), "an all-defaults view wastes an LRU slot");
    }

    #[test]
    fn resetting_a_folder_to_defaults_removes_its_record() {
        let mut prefs = FolderPrefs::new();
        prefs.set("/a", icons());
        assert_eq!(prefs.len(), 1);
        prefs.set("/a", FolderView::default());
        assert!(prefs.is_empty());
    }

    #[test]
    fn evicts_the_least_recently_used_past_capacity() {
        let mut prefs = FolderPrefs::new();
        for i in 0..FOLDER_PREFS_CAPACITY {
            prefs.set(format!("/f{i}"), icons());
        }
        assert_eq!(prefs.len(), FOLDER_PREFS_CAPACITY);

        // Touch the oldest so it is no longer the eviction candidate.
        assert!(prefs.get("/f0").is_some());

        prefs.set("/newcomer", by_size());

        assert_eq!(prefs.len(), FOLDER_PREFS_CAPACITY);
        assert!(prefs.peek("/newcomer").is_some(), "the new entry survives");
        assert!(prefs.peek("/f0").is_some(), "the touched entry survives");
        assert!(
            prefs.peek("/f1").is_none(),
            "the untouched oldest is evicted"
        );
    }

    #[test]
    fn a_corrupt_file_loses_layouts_and_nothing_else() {
        let prefs = FolderPrefs::from_json("{ this is not json");
        assert!(prefs.is_empty());
    }

    #[test]
    fn an_oversized_file_is_trimmed_on_load() {
        let mut oversized = Persisted::default();
        for i in 0..(FOLDER_PREFS_CAPACITY + 50) {
            oversized.folders.insert(
                format!("/f{i}"),
                Record {
                    view: icons(),
                    used: i as u64,
                },
            );
        }
        let prefs = FolderPrefs::from_json(&serde_json::to_string(&oversized).unwrap());
        assert_eq!(prefs.len(), FOLDER_PREFS_CAPACITY);
    }

    #[test]
    fn dirty_tracking_lets_an_idle_app_skip_writing() {
        let mut prefs = FolderPrefs::new();
        assert!(!prefs.is_dirty());
        prefs.set("/a", icons());
        assert!(prefs.is_dirty());
        prefs.mark_saved();
        assert!(!prefs.is_dirty());
        assert!(prefs.peek("/a").is_some(), "peek must not dirty the store");
        assert!(!prefs.is_dirty());
    }

    #[test]
    fn the_clock_survives_a_reload_so_lru_order_is_not_reset() {
        let mut prefs = FolderPrefs::new();
        prefs.set("/a", icons());
        prefs.set("/b", by_size());
        let reloaded = FolderPrefs::from_json(&prefs.to_json());
        assert!(reloaded.clock >= 2, "clock must not restart at zero");
    }
}
