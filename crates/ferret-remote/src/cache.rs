//! What a mount remembers between calls, and for how long.
//!
//! Two caches with one job: stop the column view's ancestors and back-and-
//! forward navigation from paying a round trip for something read seconds ago.
//! Neither is allowed to pretend the server is local, so three rules hold:
//!
//! 1. **Everything expires.** A hit older than [`CACHE_TTL`] is a miss.
//! 2. **A mutation invalidates immediately.** Anything Ferret changes drops
//!    the entry and its parent directory, so a rename never leaves the old
//!    name on screen.
//! 3. **Refresh always reaches the server.** ⌘R invalidates before it lists;
//!    a Refresh that could return a cached answer is not a refresh.
//!
//! A reconnect needs no clearing step: it builds a whole new backend, and this
//! goes with the old one. That is deliberate — a link that dropped proves
//! nothing about what changed on the other side while it was gone, and the
//! cheapest way to guarantee nothing survives is for there to be no path by
//! which it could.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use ferret_vfs::{Entry, Metadata};
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::{
    CACHE_TTL, LISTING_CACHE_MAX_ENTRIES, LISTING_CACHE_SIZE, MAX_CONCURRENT_LISTINGS,
};

/// One mount's caches, plus the permit pool that caps concurrent listings.
pub(crate) struct MountCache {
    /// Attributes by absolute remote path.
    ///
    /// Keyed on the server's own path strings, which are attacker-choosable —
    /// so this is a `std` `HashMap` with its seeded SipHash rather than the
    /// workspace's `foldhash`, which is chosen for keys read off the user's
    /// own disk (R-D11, and the note in `Cargo.toml`).
    attrs: Mutex<HashMap<String, (Instant, Metadata)>>,
    /// Recent directory listings, newest last. A `VecDeque` rather than a real
    /// LRU: at sixteen entries a linear scan is faster than the bookkeeping.
    listings: Mutex<VecDeque<CachedListing>>,
    /// How many `opendir` sequences this mount may have open at once.
    listing_slots: Arc<Semaphore>,
}

struct CachedListing {
    path: String,
    read_at: Instant,
    entries: Arc<Vec<Entry>>,
}

impl Default for MountCache {
    fn default() -> Self {
        Self {
            attrs: Mutex::new(HashMap::new()),
            listings: Mutex::new(VecDeque::with_capacity(LISTING_CACHE_SIZE)),
            listing_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_LISTINGS)),
        }
    }
}

impl MountCache {
    /// Wait for a listing slot. The permit is held for the whole walk and
    /// released when the task ends — including when it ends early because the
    /// reader dropped the stream.
    pub(crate) async fn listing_slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.listing_slots).acquire_owned().await.ok()
    }

    pub(crate) fn metadata(&self, path: &str) -> Option<Metadata> {
        let attrs = self.attrs.lock();
        let (read_at, metadata) = attrs.get(path)?;
        fresh(*read_at).then_some(*metadata)
    }

    pub(crate) fn store_metadata(&self, path: &str, metadata: Metadata) {
        self.attrs
            .lock()
            .insert(path.to_string(), (Instant::now(), metadata));
    }

    pub(crate) fn listing(&self, path: &str) -> Option<Arc<Vec<Entry>>> {
        let listings = self.listings.lock();
        listings
            .iter()
            .find(|cached| cached.path == path && fresh(cached.read_at))
            .map(|cached| Arc::clone(&cached.entries))
    }

    /// Remember a listing, unless it is too big to be worth copying back.
    pub(crate) fn store_listing(&self, path: &str, entries: Vec<Entry>) {
        if entries.len() > LISTING_CACHE_MAX_ENTRIES {
            return;
        }
        // Each entry's metadata is already in hand, so seeding the attribute
        // cache here costs nothing and saves the path bar and column view a
        // stat apiece.
        {
            let mut attrs = self.attrs.lock();
            let now = Instant::now();
            for entry in &entries {
                if let Some(metadata) = entry.metadata {
                    attrs.insert(entry.path.to_string(), (now, metadata));
                }
            }
        }

        let mut listings = self.listings.lock();
        listings.retain(|cached| cached.path != path);
        if listings.len() == LISTING_CACHE_SIZE {
            listings.pop_front();
        }
        listings.push_back(CachedListing {
            path: path.to_string(),
            read_at: Instant::now(),
            entries: Arc::new(entries),
        });
    }

    /// Forget `path`, everything beneath it, and the directory holding it.
    ///
    /// The parent matters as much as the path itself: renaming a file changes
    /// what its *directory* contains, and a cached listing that still had the
    /// old name would put it back on screen.
    pub(crate) fn invalidate(&self, path: &str) {
        let path = path.trim_end_matches('/');
        let parent = path.rsplit_once('/').map(|(head, _)| head).unwrap_or("");

        self.attrs
            .lock()
            .retain(|key, _| !covers(path, key) && key != parent);
        self.listings
            .lock()
            .retain(|cached| !covers(path, &cached.path) && cached.path != parent);
    }
}

fn fresh(read_at: Instant) -> bool {
    read_at.elapsed() < CACHE_TTL
}

/// Whether `candidate` is `path` or lies beneath it.
///
/// Component-wise, never a bare `starts_with`: `/srv/data2` begins with the
/// text `/srv/data` and is not inside it, so invalidating one would silently
/// drop the other — which is a cache bug that looks like a flicker.
fn covers(path: &str, candidate: &str) -> bool {
    if path.is_empty() || path == "/" {
        return true;
    }
    candidate == path
        || (candidate.starts_with(path) && candidate.as_bytes().get(path.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, MountId, VfsPath};

    fn entry(dir: &str, name: &str) -> Entry {
        Entry::new(
            VfsPath::remote(MountId(1), format!("{dir}/{name}")),
            name,
            EntryKind::File,
        )
        .with_metadata(Some(Metadata {
            len: 1,
            ..Default::default()
        }))
    }

    #[test]
    fn a_listing_comes_back_and_seeds_the_attributes_it_contained() {
        let cache = MountCache::default();
        cache.store_listing("/srv", vec![entry("/srv", "a.txt"), entry("/srv", "b.txt")]);

        let hit = cache.listing("/srv").expect("a fresh listing is a hit");
        assert_eq!(hit.len(), 2);
        // The listing already carried these, so asking for one must not cost a
        // round trip either.
        assert!(cache.metadata("/srv/a.txt").is_some());
        assert!(cache.metadata("/srv/missing").is_none());
    }

    #[test]
    fn a_mutation_drops_the_entry_and_the_directory_holding_it() {
        let cache = MountCache::default();
        cache.store_listing("/srv", vec![entry("/srv", "a.txt")]);
        cache.store_listing("/srv/inner", vec![entry("/srv/inner", "deep.txt")]);

        // Renaming /srv/a.txt changes what /srv contains, so the parent's
        // listing has to go too or the old name stays on screen.
        cache.invalidate("/srv/a.txt");
        assert!(cache.listing("/srv").is_none());
        assert!(cache.metadata("/srv/a.txt").is_none());
        // A sibling directory is untouched.
        assert!(cache.listing("/srv/inner").is_some());
    }

    #[test]
    fn invalidating_a_directory_drops_everything_beneath_it() {
        let cache = MountCache::default();
        cache.store_listing("/srv/data", vec![entry("/srv/data", "a")]);
        cache.store_listing("/srv/data/inner", vec![entry("/srv/data/inner", "b")]);
        cache.store_listing("/srv/data2", vec![entry("/srv/data2", "c")]);

        cache.invalidate("/srv/data");
        assert!(cache.listing("/srv/data").is_none());
        assert!(cache.listing("/srv/data/inner").is_none());
        // `/srv/data2` begins with the same text and is a different directory.
        assert!(
            cache.listing("/srv/data2").is_some(),
            "a textual prefix is not containment"
        );
    }

    #[test]
    fn the_cache_is_bounded_and_a_huge_listing_is_never_stored() {
        let cache = MountCache::default();
        for i in 0..LISTING_CACHE_SIZE + 4 {
            cache.store_listing(&format!("/srv/{i}"), vec![entry("/srv", "x")]);
        }
        assert_eq!(cache.listings.lock().len(), LISTING_CACHE_SIZE);
        // The oldest was evicted, the newest is there.
        assert!(cache.listing("/srv/0").is_none());
        assert!(
            cache
                .listing(&format!("/srv/{}", LISTING_CACHE_SIZE + 3))
                .is_some()
        );

        let huge: Vec<Entry> = (0..LISTING_CACHE_MAX_ENTRIES + 1)
            .map(|i| entry("/big", &format!("f{i}")))
            .collect();
        cache.store_listing("/big", huge);
        assert!(
            cache.listing("/big").is_none(),
            "a hit would copy the whole directory back"
        );
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        // The TTL is the backstop behind every explicit invalidation: even a
        // path nothing touched stops being trusted.
        let cache = MountCache::default();
        cache.store_listing("/srv", vec![entry("/srv", "a")]);
        assert!(cache.listing("/srv").is_some());

        cache.listings.lock()[0].read_at = std::time::Instant::now() - CACHE_TTL * 2;
        assert!(cache.listing("/srv").is_none(), "a stale listing is a miss");
    }
}
