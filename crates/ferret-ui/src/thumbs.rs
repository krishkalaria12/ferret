//! UI-side thumbnail tiles: memory cache plus the warm path (F-33).
//!
//! Three tiers, each cheaper than the last to ask:
//!
//! 1. [`RenderedThumbCache`]: decoded `RenderImage`s keyed by [`VfsPath`],
//!    read lock-free on the render path. The only thing `render_tile` may
//!    touch.
//! 2. [`ferret_preview::ThumbCache`]: validated WebP on disk, decoded off
//!    the render path.
//! 3. Generation: bounded source read, the same capped decode the pane uses,
//!    first frame only, fitted and stored.
//!
//! Warming is visibility-bounded (visible tiles plus one viewport of
//! lookahead) and generation-guarded: a new range replaces the previous
//! task, which is what cancels it. Stills only — animation plays in the
//! docked pane, never in the tiles.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use ferret_vfs::{Entry, VfsPath};

/// How often the disk cache re-enforces its age and byte caps past the
/// startup sweep. A directory not visited in a month should not still be
/// costing disk, but nothing about that needs checking hourly.
pub const THUMB_EVICT_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Most tiles one warm task resolves before it yields the range.
///
/// A viewport plus lookahead is tens of tiles; the cap is what keeps a stale
/// task (range already replaced) from grinding through a whole directory
/// before noticing.
pub const THUMB_WARM_CAP: usize = 256;

/// Most decoded tiles held in memory.
///
/// Scrolling a 200,000-photo directory must not grow this without bound: past
/// the cap an insert drops an arbitrary entry rather than growing. The disk
/// cache below remains the source of truth; memory is only the visible
/// working set.
pub const MAX_RENDERED_THUMBS: usize = 1024;

/// One tile, decoded and carrying the fingerprint it was built from.
///
/// The fingerprint is what keeps a path-keyed memory entry honest: an edit
/// changes length or mtime, the next warm pass sees the mismatch and
/// regenerates instead of serving the old pixels.
#[derive(Clone)]
pub struct ThumbImage {
    pub image: Arc<gpui::RenderImage>,
    pub width: u32,
    pub height: u32,
    pub len: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
}

/// Decoded tiles keyed by path: lock-free reads, copy-on-write inserts.
///
/// Same shape as [`crate::icons::RenderedIconCache`]: the render path gets
/// the exact same `RenderImage` identity every frame, and a new tile lands
/// once. Share with `Arc`: background warm tasks hold a clone.
pub struct RenderedThumbCache {
    images: ArcSwap<HashMap<VfsPath, ThumbImage>>,
}

impl Default for RenderedThumbCache {
    fn default() -> Self {
        Self {
            images: ArcSwap::from_pointee(HashMap::new()),
        }
    }
}

impl RenderedThumbCache {
    /// The tile for an entry, when it is cached *and* fresh.
    ///
    /// Synchronous and allocation-free on a hit (one `Arc` clone out); safe
    /// to call once per visible tile per frame, which is what the icon view
    /// does. A fingerprint mismatch is a miss: the warm pass regenerates.
    pub fn get(&self, entry: &Entry) -> Option<ThumbImage> {
        let meta = entry.metadata?;
        let (len, secs, nanos) = ferret_preview::fingerprint(meta.len, meta.modified)?;
        let cached = self.images.load().get(&entry.path)?.clone();
        (cached.len == len && cached.mtime_secs == secs && cached.mtime_nanos == nanos)
            .then_some(cached)
    }

    pub fn insert(&self, path: VfsPath, image: ThumbImage) {
        loop {
            let current = self.images.load_full();
            let mut next = (*current).clone();
            next.insert(path.clone(), image.clone());
            // Bounded: an unbounded path-keyed map over a 200k directory is
            // gigabytes. Drop an arbitrary entry past the cap rather than
            // growing; the disk cache regenerates it on revisit.
            if next.len() > MAX_RENDERED_THUMBS
                && let Some(victim) = next.keys().next().cloned()
            {
                next.remove(&victim);
            }
            let previous = self.images.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&previous, &current) {
                return;
            }
        }
    }

    /// Drop every tile: called when the directory changes, so a visited
    /// photo folder does not keep its tiles resident forever.
    pub fn clear(&self) {
        self.images.store(Arc::new(HashMap::new()));
    }

    /// Drop one path's tile: called on trash/rename/delete so a removed
    /// file's pixels do not survive in memory.
    pub fn remove(&self, path: &VfsPath) {
        loop {
            let current = self.images.load_full();
            if !current.contains_key(path) {
                return;
            }
            let mut next = (*current).clone();
            next.remove(path);
            let previous = self.images.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&previous, &current) {
                return;
            }
        }
    }

    pub fn remove_many(&self, paths: &[VfsPath]) {
        loop {
            let current = self.images.load_full();
            if !paths.iter().any(|path| current.contains_key(path)) {
                return;
            }
            let removed = paths.iter().collect::<std::collections::HashSet<_>>();
            let mut next = (*current).clone();
            next.retain(|path, _| !removed.contains(path));
            let previous = self.images.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&previous, &current) {
                return;
            }
        }
    }
}

/// Build the `RenderImage` for a validated tile bitmap, once, off the render
/// path.
///
/// The bytes arrive BGRA inside an `RgbaImage` shell — the same layout the
/// pane's `render_image` hands the renderer — so the shell is passed through
/// untouched and the single frame plays as a still.
pub fn render_thumb(
    bitmap: &ferret_preview::Bitmap,
    len: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
) -> Option<ThumbImage> {
    let frame = ferret_preview::image::still_frame(bitmap)?;
    Some(ThumbImage {
        image: Arc::new(gpui::RenderImage::new(vec![frame])),
        width: bitmap.width,
        height: bitmap.height,
        len,
        mtime_secs,
        mtime_nanos,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, Metadata};

    fn file_entry(len: u64) -> Entry {
        let mut entry = Entry::new(
            VfsPath::local("/pics/photo.jpg"),
            "photo.jpg",
            EntryKind::File,
        );
        entry.metadata = Some(Metadata {
            len,
            modified: Some(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            hidden: false,
            name_hidden: false,
            platform_hidden: false,
        });
        entry
    }

    #[test]
    fn a_tile_is_a_hit_until_the_file_changes() {
        let cache = RenderedThumbCache::default();
        let bitmap = ferret_preview::Bitmap {
            width: 4,
            height: 2,
            bgra: vec![1; 4 * 2 * 4],
            has_transparency: false,
        };
        let entry = file_entry(100);
        let thumb = render_thumb(&bitmap, 100, 1_700_000_000, 0).unwrap();
        assert_eq!((thumb.width, thumb.height), (4, 2));
        cache.insert(entry.path.clone(), thumb);
        assert!(cache.get(&entry).is_some());
        // Same path, new length: the old pixels must not serve.
        assert!(cache.get(&file_entry(101)).is_none());
    }
}
