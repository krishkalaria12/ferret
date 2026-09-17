//! Icon-view thumbnails: a bounded on-disk cache (F-33, second half).
//!
//! The docked pane decodes the selected image at pane resolution; the icon
//! view needs something cheaper for hundreds of tiles. Generation decodes the
//! source once through [`crate::image::decode`] — same pixel cap, same
//! decoder limits — keeps the first frame only (a directory of forty animated
//! GIFs must not become forty running animations), fits it inside
//! [`crate::config::THUMB_PX`], and stores lossless WebP under the user's
//! cache directory.
//!
//! The key is a hash of canonical path, mtime, size and target pixel size.
//! Size is in the key because a file replaced with one of a different length
//! is common; mtime because it is cheap; the canonical path so a rename
//! misses rather than serves another file's tile. A hit still decodes the
//! cached WebP — that decode is the price of never holding hundreds of
//! bitmaps resident.
//!
//! Everything here is synchronous `std::fs` and GPUI-free. Nothing runs on
//! the render path: the view asks from a task, and eviction runs on startup
//! and a low-frequency timer.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::config::{THUMB_CACHE_AGE, THUMB_CACHE_BYTES, THUMB_PX};
use super::image::Bitmap;

/// A file's cache identity: length plus mtime, the two facts an edit changes.
///
/// `None` mtime means "unknown freshness" and never forms a key — a tile that
/// cannot be invalidated is a stale tile waiting to happen, so the caller
/// draws the glyph instead.
pub fn fingerprint(len: u64, modified: Option<SystemTime>) -> Option<(u64, u64, u32)> {
    let modified = modified?;
    let elapsed = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    Some((len, elapsed.as_secs(), elapsed.subsec_nanos()))
}

/// Whether an already-lowercased extension has an in-process decoder behind
/// it, and so can grow a tile without leaving the process.
///
/// HEIC/AVIF/RAW go through [`is_native_tileable_extension`] instead:
/// generating for them in-process would schedule a job that can only fail.
pub fn is_tileable_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "tif" | "tiff" | "bmp" | "ico" | "qoi"
    )
}
/// The cache key: FNV-1a over length-prefixed fields, hex-encoded.
///
/// `std`'s hasher is randomly seeded per process, which is exactly wrong for
/// a disk cache — keys must survive a restart. FNV-1a is deterministic and
/// cache-grade: a collision serves a wrong tile until eviction, the same
/// class of staleness the mtime granularity already accepts.
pub fn thumb_key(canonical: &Path, len: u64, mtime_secs: u64, mtime_nanos: u32, px: u32) -> String {
    let mut hasher = Fnv1a::new();
    // Length-prefix the one variable-length field so ("ab","c") and
    // ("a","bc") cannot hash alike.
    let path = canonical.as_os_str().as_encoded_bytes();
    hasher.write(&(path.len() as u64).to_le_bytes());
    hasher.write(path);
    hasher.write(&len.to_le_bytes());
    hasher.write(&mtime_secs.to_le_bytes());
    hasher.write(&mtime_nanos.to_le_bytes());
    hasher.write(&px.to_le_bytes());
    format!("{:016x}", hasher.finish())
}

struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Largest cache file honoured on lookup.
///
/// Our entries are tens of kilobytes. Anything past this in our directory is
/// not ours — refused, never decoded, and left alone rather than deleted, in
/// case a future tile size legitimately exceeds it.
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

/// The on-disk thumbnail cache: a directory of `<key>.webp` files.
///
/// Cheap to construct and to clone (a path); every method is best-effort and
/// blocking, for background tasks only.
#[derive(Clone, Debug)]
pub struct ThumbCache {
    dir: PathBuf,
}

impl ThumbCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The standard location: the platform cache directory, else the temp
    /// directory. Temp is a degraded cache that the OS may sweep — bounded
    /// either way, and glyphs cover a missing entry, so a strange machine
    /// costs tiles, never correctness.
    pub fn default_dir() -> PathBuf {
        dirs::cache_dir()
            .map(|dir| dir.join("ferret").join("thumbs"))
            .unwrap_or_else(|| std::env::temp_dir().join("ferret-thumbs"))
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.webp"))
    }

    /// A cached tile, decoded and validated — or `None` for a miss.
    ///
    /// A corrupt entry is deleted: it failed validation once and will fail it
    /// every visit, so keeping it turns one bad write into a permanent miss.
    pub fn lookup(&self, key: &str) -> Option<Bitmap> {
        let path = self.path_for(key);
        let meta = std::fs::metadata(&path).ok()?;
        if meta.len() == 0 || meta.len() > MAX_ENTRY_BYTES {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let Some(bitmap) = decode_entry(&bytes) else {
            let _ = std::fs::remove_file(&path);
            return None;
        };
        if !bitmap.is_well_formed() || bitmap.width > THUMB_PX * 4 || bitmap.height > THUMB_PX * 4 {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some(bitmap)
    }

    /// Persist a tile, atomically: temp file in the same directory plus
    /// rename, so a crash leaves the old entry or nothing, never half a
    /// WebP. `false` means "keep the glyph" — the caller never retries; the
    /// next scroll re-requests.
    pub fn store(&self, key: &str, thumb: &Bitmap) -> bool {
        if !thumb.is_well_formed() {
            return false;
        }
        let rgba = ::image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.bgra.clone());
        let Some(rgba) = rgba else {
            return false;
        };
        let mut encoded = Vec::new();
        let encoder = ::image::codecs::webp::WebPEncoder::new_lossless(&mut encoded);
        // This `image` version exposes only lossless WebP encoding: larger
        // entries than lossy would write, still tens of kilobytes at 128 px,
        // and exact — which is what a cache wants.
        if encoder
            .encode(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                ::image::ExtendedColorType::Rgba8,
            )
            .is_err()
        {
            return false;
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return false;
        }
        let tmp = self.dir.join(format!(".{key}.tmp"));
        if std::fs::write(&tmp, &encoded).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        std::fs::rename(&tmp, self.path_for(key)).is_ok()
    }

    /// Fit a decoded first frame inside the tile square, aspect preserved.
    ///
    /// Already-small bitmaps pass through by clone — no resample, no
    /// softening. The resize blends channels independently under a linear
    /// filter, so operating on BGRA-ordered bytes and keeping the BGRA label
    /// is exact, not a shortcut.
    pub fn make(bitmap: &Bitmap, px: u32) -> Option<Bitmap> {
        if !bitmap.is_well_formed() || px == 0 {
            return None;
        }
        if bitmap.width <= px && bitmap.height <= px {
            return Some(bitmap.clone());
        }
        let scale = px as f32 / bitmap.width.max(bitmap.height) as f32;
        let target_width = ((bitmap.width as f32 * scale).round() as u32).max(1);
        let target_height = ((bitmap.height as f32 * scale).round() as u32).max(1);
        let rgba = ::image::RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.bgra.clone())?;
        let resized = ::image::imageops::resize(
            &rgba,
            target_width,
            target_height,
            ::image::imageops::Triangle,
        );
        let (width, height) = (resized.width(), resized.height());
        Some(Bitmap {
            width,
            height,
            bgra: resized.into_raw(),
            has_transparency: bitmap.has_transparency,
        })
    }

    /// Enforce the age and byte caps: entries older than
    /// [`THUMB_CACHE_AGE`] go, then oldest-first past
    /// [`THUMB_CACHE_BYTES`].
    ///
    /// Startup and a low-frequency timer own this; never the render path.
    /// Every I/O failure is skipped over — a cache that cannot be read is a
    /// cache that stays big, not an error the user can act on.
    pub fn evict(&self) {
        let now = SystemTime::now();
        let entries: Vec<(PathBuf, u64, Option<SystemTime>)> = self
            .inventory()
            .into_iter()
            .map(|entry| (entry.path, entry.len, entry.modified))
            .collect();
        for path in plan_eviction(entries, now) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn inventory(&self) -> Vec<CacheEntry> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for child in dir.flatten() {
            let path = child.path();
            if path.extension().is_none_or(|ext| ext != "webp") {
                continue;
            }
            let Ok(meta) = child.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            entries.push(CacheEntry {
                path,
                len: meta.len(),
                modified: meta.modified().ok(),
            });
        }
        entries
    }
}

struct CacheEntry {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

/// Which cached entries an eviction pass removes, as paths.
///
/// Pure so the policy is testable without touching the disk: `evict` lists,
/// this decides, `evict` deletes.
fn plan_eviction(
    mut entries: Vec<(PathBuf, u64, Option<SystemTime>)>,
    now: SystemTime,
) -> Vec<PathBuf> {
    let mut remove = Vec::new();
    let mut live = Vec::with_capacity(entries.len());
    let mut retained_bytes = 0u64;
    for (path, len, modified) in entries.drain(..) {
        let stale = modified
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > THUMB_CACHE_AGE);
        if stale {
            remove.push(path);
        } else {
            retained_bytes += len;
            live.push((path, len, modified));
        }
    }
    if retained_bytes > THUMB_CACHE_BYTES {
        // Oldest first. Unknown mtime sorts as the epoch — evictable before
        // anything with a proven age — because an entry whose freshness
        // cannot be shown is regenerable cache, not data.
        live.sort_by_key(|(_, _, modified)| modified.unwrap_or(UNIX_EPOCH));
        for (path, len, _) in live {
            if retained_bytes <= THUMB_CACHE_BYTES {
                break;
            }
            retained_bytes = retained_bytes.saturating_sub(len);
            remove.push(path);
        }
    }
    remove
}

fn decode_entry(bytes: &[u8]) -> Option<Bitmap> {
    let image = ::image::load_from_memory_with_format(bytes, ::image::ImageFormat::WebP).ok()?;
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let mut bgra = rgba.into_raw();
    let mut transparent = false;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        transparent |= pixel[3] != 255;
    }
    Some(Bitmap {
        width,
        height,
        bgra,
        has_transparency: transparent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Bitmap, decode};
    use ::image::{DynamicImage, Rgba, RgbaImage};

    fn solid(width: u32, height: u32) -> Bitmap {
        let image = RgbaImage::from_pixel(width, height, Rgba([9, 9, 9, 255]));
        let (width, height) = (image.width(), image.height());
        Bitmap {
            width,
            height,
            bgra: image.into_raw(),
            has_transparency: false,
        }
    }

    fn encode_png(bitmap: &Bitmap) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(
            RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.bgra.clone()).unwrap(),
        )
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            ::image::ImageFormat::Png,
        )
        .unwrap();
        bytes
    }

    fn test_cache(name: &str) -> (ThumbCache, ScopedDir) {
        let dir = std::env::temp_dir().join(format!(
            "ferret-thumbs-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (ThumbCache::new(dir.clone()), ScopedDir(dir))
    }

    /// Removes only its own directory on drop, so parallel tests sharing a
    /// process id cannot delete each other's fixtures mid-test.
    struct ScopedDir(PathBuf);
    impl Drop for ScopedDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn keys_are_stable_and_cover_every_field() {
        let a = Path::new("/pics/a.jpg");
        let key = |len, secs, nanos, px| thumb_key(a, len, secs, nanos, px);
        assert_eq!(key(1, 2, 3, 128), key(1, 2, 3, 128));
        assert_ne!(key(1, 2, 3, 128), key(2, 2, 3, 128), "size matters");
        assert_ne!(key(1, 2, 3, 128), key(1, 4, 3, 128), "mtime matters");
        assert_ne!(key(1, 2, 3, 128), key(1, 2, 5, 128), "mtime nanos matter");
        assert_ne!(key(1, 2, 3, 128), key(1, 2, 3, 256), "target size matters");
        assert_ne!(
            thumb_key(a, 1, 2, 3, 128),
            thumb_key(Path::new("/pics/b.jpg"), 1, 2, 3, 128),
            "a rename must miss, never serve another file's tile"
        );
    }

    #[test]
    fn unknown_mtime_has_no_key() {
        assert_eq!(fingerprint(100, None), None);
        let stamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(fingerprint(100, Some(stamp)), Some((100, 1_700_000_000, 0)));
    }

    #[test]
    fn only_decodable_extensions_tile() {
        for ext in [
            "png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp", "ico", "qoi",
        ] {
            assert!(is_tileable_extension(ext), "{ext} must tile");
        }
        // Formats with no in-process decoder go through the native service
        // instead (checked below); office documents keep the glyph until
        // handler coverage is verified on both platforms.
        for ext in [
            "heic", "heif", "avif", "psd", "cr2", "nef", "arw", "pdf", "mp4",
        ] {
            assert!(!is_tileable_extension(ext), "{ext} must keep its glyph");
        }
    }

    #[test]
    fn make_fits_inside_the_square_preserving_aspect() {
        // 400x100 landscape -> 128x32.
        let thumb = ThumbCache::make(&solid(400, 100), 128).unwrap();
        assert_eq!((thumb.width, thumb.height), (128, 32));
        assert!(thumb.is_well_formed());
        // 100x400 portrait -> 32x128.
        let thumb = ThumbCache::make(&solid(100, 400), 128).unwrap();
        assert_eq!((thumb.width, thumb.height), (32, 128));
        // Already small passes through untouched.
        let small = solid(48, 32);
        let thumb = ThumbCache::make(&small, 128).unwrap();
        assert_eq!((thumb.width, thumb.height), (48, 32));
        assert_eq!(thumb.bgra, small.bgra, "no resample below the cap");
    }

    #[test]
    fn make_from_a_real_decode_round_trips_the_disk() {
        let (cache, _guard) = test_cache("roundtrip");
        let decoded = decode(&encode_png(&solid(640, 480))).unwrap();
        let thumb = ThumbCache::make(&decoded.frames[0].bitmap, THUMB_PX).unwrap();
        assert_eq!((thumb.width, thumb.height), (128, 96));
        let key = thumb_key(Path::new("/pics/p.png"), 1, 2, 3, THUMB_PX);
        assert!(cache.store(&key, &thumb));
        let back = cache.lookup(&key).expect("just stored");
        assert_eq!((back.width, back.height), (128, 96));
        assert!(back.is_well_formed());
        // Lossless, so the tile survives the disk byte-identically.
        assert_eq!(back.bgra, thumb.bgra);
    }

    #[test]
    fn corrupt_entries_miss_and_are_deleted() {
        let (cache, _guard) = test_cache("corrupt");
        let key = thumb_key(Path::new("/pics/p.png"), 1, 2, 3, THUMB_PX);
        std::fs::create_dir_all(&cache.dir).unwrap();
        std::fs::write(cache.path_for(&key), b"not a webp at all").unwrap();
        assert!(cache.lookup(&key).is_none());
        assert!(
            !cache.path_for(&key).exists(),
            "a corrupt entry must not fail every visit forever"
        );
    }

    #[test]
    fn oversized_entries_are_refused_not_decoded() {
        let (cache, _guard) = test_cache("keys");
        let key = thumb_key(Path::new("/pics/p.png"), 1, 2, 3, THUMB_PX);
        std::fs::create_dir_all(&cache.dir).unwrap();
        std::fs::write(cache.path_for(&key), vec![0u8; 2 * 1024 * 1024]).unwrap();
        assert!(cache.lookup(&key).is_none());
        // Refused, not deleted: it may be a future tile size, not corruption.
        assert!(cache.path_for(&key).exists());
    }

    #[test]
    fn eviction_drops_stale_then_oldest_first() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let old = now - THUMB_CACHE_AGE - Duration::from_secs(1);
        let recent = now - Duration::from_secs(1);
        let entry = |name: &str, len: u64, at: Option<SystemTime>| (PathBuf::from(name), len, at);
        // Stale goes even under budget.
        let remove = plan_eviction(vec![entry("stale", 10, Some(old))], now);
        assert_eq!(remove, vec![PathBuf::from("stale")]);
        // Over budget: oldest first, newest kept. Three entries each past
        // half the cap need two removals; the newest survives.
        let big = THUMB_CACHE_BYTES / 2 + 1;
        let remove = plan_eviction(
            vec![
                entry("a", big, Some(old + Duration::from_secs(2))),
                entry("b", big, Some(recent)),
                entry("c", big, Some(recent)),
            ],
            now,
        );
        assert_eq!(
            remove,
            vec![PathBuf::from("a"), PathBuf::from("b")],
            "oldest-first, newest kept"
        );
        // Under budget and fresh: nothing goes.
        let remove = plan_eviction(vec![entry("fine", 10, Some(recent))], now);
        assert!(remove.is_empty());
    }
}
