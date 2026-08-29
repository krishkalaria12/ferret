//! The macOS backend.
//!
//! Opening and revealing go through `/usr/bin/open`, which is the same call
//! Finder makes; the Trash goes through the `trash` crate, which uses
//! `NSWorkspace` rather than a blind `rename` into `~/.Trash` — a rename
//! would silently break on a different volume and would not record the
//! original path for "Put Back".

use crate::config::{MAX_VOLUMES, VOLUME_EXCLUSIONS};
use crate::errors::{PlatformError, Result};
use crate::{Place, PlaceKind, Platform};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// The macOS implementation of [`Platform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MacOs;

async fn run_open(args: &[&str], action: &'static str, path: &Path) -> Result<()> {
    let owned = path.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let outcome = smol::unblock(move || {
        std::process::Command::new("/usr/bin/open")
            .args(&args)
            .arg(&owned)
            .status()
    })
    .await;

    match outcome {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(PlatformError::Io {
            action,
            path: path.to_path_buf(),
            source: std::io::Error::other("the open command failed"),
        }),
        Err(source) => Err(PlatformError::from_io(action, path.to_path_buf(), source)),
    }
}

fn place(kind: PlaceKind, name: &str, path: PathBuf) -> Option<Place> {
    if path.is_dir() {
        Some(Place {
            name: name.to_string(),
            kind,
            path,
        })
    } else {
        None
    }
}

#[async_trait]
impl Platform for MacOs {
    async fn open_with_default(&self, path: &Path) -> Result<()> {
        run_open(&[], "open", path).await
    }

    async fn reveal_in_file_manager(&self, path: &Path) -> Result<()> {
        run_open(&["-R"], "reveal", path).await
    }

    async fn move_to_trash(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        if !owned.exists() {
            return Err(PlatformError::NotFound { path: owned });
        }
        // `trash::delete` routes through NSWorkspace, so it works across
        // volumes and records the original location for Put Back.
        smol::unblock(move || trash::delete(&owned))
            .await
            .map_err(|source| match source {
                trash::Error::CouldNotAccess { .. } => PlatformError::PermissionDenied {
                    path: path.to_path_buf(),
                },
                other => PlatformError::Io {
                    action: "move to the Trash",
                    path: path.to_path_buf(),
                    source: std::io::Error::other(other.to_string()),
                },
            })
    }

    fn places(&self) -> Vec<Place> {
        let mut out = Vec::new();
        if let Some(home) = dirs::home_dir() {
            out.push(Place {
                name: "Home".into(),
                kind: PlaceKind::Home,
                path: home.clone(),
            });
            let push = |out: &mut Vec<Place>, kind: PlaceKind, name: &str, p: PathBuf| {
                if let Some(place) = place(kind, name, p) {
                    out.push(place);
                }
            };
            push(
                &mut out,
                PlaceKind::Desktop,
                "Desktop",
                home.join("Desktop"),
            );
            push(
                &mut out,
                PlaceKind::Documents,
                "Documents",
                dirs::document_dir().unwrap_or_else(|| home.join("Documents")),
            );
            push(
                &mut out,
                PlaceKind::Downloads,
                "Downloads",
                dirs::download_dir().unwrap_or_else(|| home.join("Downloads")),
            );
            push(
                &mut out,
                PlaceKind::Music,
                "Music",
                dirs::audio_dir().unwrap_or_else(|| home.join("Music")),
            );
            push(
                &mut out,
                PlaceKind::Pictures,
                "Pictures",
                dirs::picture_dir().unwrap_or_else(|| home.join("Pictures")),
            );
            push(
                &mut out,
                PlaceKind::Videos,
                "Movies",
                dirs::video_dir().unwrap_or_else(|| home.join("Movies")),
            );
            push(
                &mut out,
                PlaceKind::Applications,
                "Applications",
                PathBuf::from("/Applications"),
            );
        }
        out
    }

    fn volumes(&self) -> Vec<Place> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/Volumes") else {
            return out;
        };
        for entry in entries.flatten().take(MAX_VOLUMES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if VOLUME_EXCLUSIONS.contains(&name.as_str()) {
                continue;
            }
            // The boot volume appears in /Volumes under its real name; the
            // symlink entry for the legacy name is noise.
            let md = match entry.metadata() {
                Ok(md) => md,
                Err(_) => continue,
            };
            if !md.is_dir() {
                continue;
            }
            if name == "Macintosh HD" && out.iter().any(|p: &Place| p.name == "Macintosh HD") {
                continue;
            }
            out.push(Place {
                name,
                kind: PlaceKind::Volume,
                path: entry.path(),
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Icons (D-18)

use crate::icon::{IconBitmap, IconKey, IconProvider};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Icon size to ask the OS for, in points.
///
/// Larger than any row draws it: one cached bitmap serves the 15 px list
/// glyph and the 48 px icon-view tile, and downscaling a sharp icon beats
/// upscaling a blurry one.
const ICON_POINTS: f64 = 64.0;

/// Finder's own icons, via `NSWorkspace`.
#[derive(Default)]
pub struct MacIcons {
    /// Resolved icons, keyed by type. Bounded by the number of *types* the
    /// user has looked at, not by the number of files.
    ///
    /// `parking_lot`, so there is no poisoning to swallow. `lookup` used to
    /// end in `.lock().ok()?`, which turned a lock poisoned by a panic
    /// anywhere in the process into "this type has no icon" — permanently,
    /// and indistinguishably from a genuine miss.
    cache: Mutex<HashMap<IconKey, Option<IconBitmap>>>,
}

impl IconProvider for MacIcons {
    fn lookup(&self, key: &IconKey) -> Option<IconBitmap> {
        // Cache only — never the OS. This runs once per visible row while a
        // frame is being built.
        self.cache.lock().get(key)?.clone()
    }

    fn resolve(&self, key: &IconKey, sample: &std::path::Path) -> bool {
        if self.cache.lock().contains_key(key) {
            return false; // Already answered, including a cached failure.
        }
        let bitmap = icon_for_path(sample).filter(IconBitmap::is_well_formed);
        let cached_something = bitmap.is_some();
        // A failure is cached too: a type whose icon cannot be read must not
        // be retried once per frame forever.
        self.cache.lock().insert(key.clone(), bitmap);
        cached_something
    }
}

/// Swap the red and blue channels of an RGBA buffer, in place of a copy.
///
/// A `u32` shuffle per pixel rather than a four-byte `extend_from_slice`
/// (A-39) — 4,096 pixels per icon at `ICON_POINTS = 64`. Split out from the
/// `unsafe` AppKit block above so the byte order can be tested without a
/// window server.
fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = vec![0u8; rgba.len()];
    for (src, dst) in rgba.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
        // Native-endian throughout: the bytes go straight back out as bytes,
        // so the only thing that matters is that the shuffle below is the
        // inverse of the load, which it is on either endianness.
        let px = u32::from_ne_bytes([src[0], src[1], src[2], src[3]]);
        let swizzled = if cfg!(target_endian = "little") {
            // Little-endian load puts src[0] (R) in the low byte and src[2]
            // (B) in bits 16..24; swapping those two is the whole job.
            (px & 0xff00_ff00) | ((px >> 16) & 0x0000_00ff) | ((px & 0x0000_00ff) << 16)
        } else {
            (px & 0x00ff_00ff) | ((px >> 16) & 0x0000_ff00) | ((px & 0x0000_ff00) << 16)
        };
        dst.copy_from_slice(&swizzled.to_ne_bytes());
    }
    bgra
}

/// Ask `NSWorkspace` for a path's icon and draw it into a BGRA buffer.
fn icon_for_path(path: &std::path::Path) -> Option<IconBitmap> {
    use objc2::ClassType as _;
    use objc2_app_kit::{NSBitmapImageRep, NSGraphicsContext, NSWorkspace};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    let side = ICON_POINTS as usize;

    // SAFETY: every call below is an ordinary AppKit message on objects this
    // function owns, made on whatever thread the caller is on. `NSWorkspace`
    // icon lookup and `NSBitmapImageRep` drawing are both thread-safe; the
    // graphics context is saved and restored around the draw so no other
    // drawing is disturbed.
    unsafe {
        let workspace = NSWorkspace::sharedWorkspace();
        let ns_path = NSString::from_str(&path.to_string_lossy());
        let image = workspace.iconForFile(&ns_path);
        image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));

        // A 32-bit RGBA bitmap we can draw the icon into and then read back.
        let rep = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            side as isize,
            side as isize,
            8,
            4,
            true,
            false,
            objc2_app_kit::NSDeviceRGBColorSpace,
            (side * 4) as isize,
            32,
        )?;

        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&context));
        image.drawInRect(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(ICON_POINTS, ICON_POINTS),
        ));
        NSGraphicsContext::restoreGraphicsState_class();

        let data = rep.bitmapData();
        if data.is_null() {
            return None;
        }
        let len = side * side * 4;
        let rgba = std::slice::from_raw_parts(data, len);

        // AppKit hands back RGBA; GPUI's `RenderImage` wants BGRA. Swizzled
        // a `u32` at a time rather than a byte at a time (A-39): one shuffle
        // and one store per pixel instead of a four-byte `extend_from_slice`,
        // which is 4,096 of them per icon at `ICON_POINTS = 64`.
        //
        // Asking `NSBitmapImageRep` for BGRA directly via `bitmapFormat`
        // would be better still and is what rev 1 suggested — but the
        // initialiser used here has no format argument, and the one that does
        // wants a set of flags whose behaviour differs across macOS releases.
        // This is the version whose correctness is obvious from the test
        // below.
        let bgra = rgba_to_bgra(rgba);

        Some(IconBitmap {
            width: side as u32,
            height: side as u32,
            bgra,
        })
    }
}

#[cfg(test)]
mod swizzle_tests {
    use super::rgba_to_bgra;

    /// The loop this replaced, kept as the definition of correct.
    fn reference(rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        out
    }

    #[test]
    fn the_fast_swizzle_matches_the_obvious_one() {
        // Every channel distinct, so a wrong lane is visible rather than
        // hidden by two channels happening to agree.
        let rgba: Vec<u8> = (0u8..=255).collect();
        assert_eq!(rgba_to_bgra(&rgba), reference(&rgba));

        // And the shapes that catch an off-by-one: one pixel, and none.
        assert_eq!(rgba_to_bgra(&[1, 2, 3, 4]), vec![3, 2, 1, 4]);
        assert!(rgba_to_bgra(&[]).is_empty());
    }

    #[test]
    fn alpha_and_green_are_left_alone() {
        // The two channels that must not move — a swizzle that disturbed
        // alpha would show up as icons with the wrong transparency.
        let out = rgba_to_bgra(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(out[1], 0xad, "green stays put");
        assert_eq!(out[3], 0xef, "alpha stays put");
        assert_eq!((out[0], out[2]), (0xbe, 0xde), "red and blue swap");
    }
}
