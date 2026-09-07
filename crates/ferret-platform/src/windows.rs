//! The Windows backend.
//!
//! Opening files with their default application goes through `explorer.exe`,
//! which resolves the shell association exactly like a double-click on the
//! desktop; revealing uses its `/select` form. The Recycle Bin goes through
//! the `trash` crate's `SHFileOperationW` path, which is what Explorer itself
//! uses and therefore records Put-Back metadata correctly.

use crate::config::MAX_VOLUMES;
use crate::errors::{PlatformError, Result};
use crate::{Place, PlaceKind, Platform, validate_terminal_directory};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// The Windows implementation of [`Platform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Windows;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

fn spawn_explorer(args: &[String], action: &'static str, path: PathBuf) -> Result<()> {
    use std::os::windows::process::CommandExt;

    let outcome = std::process::Command::new("explorer.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status();

    match outcome {
        // `explorer /select` returns non-zero on some systems even when it
        // succeeded; its exit code is not a contract anyone honours.
        Ok(_) => Ok(()),
        Err(source) => Err(PlatformError::from_io(action, path, source)),
    }
}

fn spawn_terminal(path: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;

    let path_text = path.to_string_lossy().into_owned();
    let windows_terminal = std::process::Command::new("wt.exe")
        .args(["-d", &path_text])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    if windows_terminal.is_ok() {
        return Ok(());
    }

    let escaped = path_text.replace('\'', "''");
    let command = format!("Set-Location -LiteralPath '{escaped}'");
    std::process::Command::new("powershell.exe")
        .args(["-NoExit", "-Command", &command])
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(|_| ())
        .map_err(|source| PlatformError::from_io("open a terminal in", path, source))
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
impl Platform for Windows {
    async fn open_with_default(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        let arg = owned.to_string_lossy().into_owned();
        smol::unblock(move || spawn_explorer(&[arg], "open", owned)).await
    }

    async fn reveal_in_file_manager(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        let arg = format!("/select,{}", owned.display());
        smol::unblock(move || spawn_explorer(&[arg], "reveal", owned)).await
    }

    async fn open_in_terminal(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        validate_terminal_directory(&owned)?;
        smol::unblock(move || spawn_terminal(&owned)).await
    }

    async fn move_to_trash(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        if !owned.exists() {
            return Err(PlatformError::NotFound { path: owned });
        }
        smol::unblock(move || trash::delete(&owned))
            .await
            .map_err(|source| match source {
                trash::Error::CouldNotAccess { .. } => PlatformError::PermissionDenied {
                    path: path.to_path_buf(),
                },
                other => PlatformError::Io {
                    action: "move to the Recycle Bin",
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
                "Videos",
                dirs::video_dir().unwrap_or_else(|| home.join("Videos")),
            );
        }
        out
    }

    fn volumes(&self) -> Vec<Place> {
        let mut out = Vec::new();
        for letter in b'A'..=b'Z' {
            if out.len() >= MAX_VOLUMES {
                break;
            }
            let root = PathBuf::from(format!("{}:\\", letter as char));
            if root.is_dir() {
                out.push(Place {
                    name: format!("{}:", letter as char),
                    kind: PlaceKind::Volume,
                    path: root,
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Thumbnails (PV-D07)
//
// `IShellItemImageFactory::GetImage` arrives with PDF page rendering
// (preview-plan step 08), behind typed `windows` COM bindings in a
// supervised helper with its own COM apartment — never on the UI thread,
// never with the pane blocked on it. Until then this declines every request
// and the pane shows metadata with *Open with…*.

/// The Windows thumbnail backend, until the supervised helper is wired up.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThumbnailDeclined;

#[async_trait]
impl crate::thumbnail::ThumbnailProvider for ThumbnailDeclined {
    async fn thumbnail(&self, _path: &Path, _longest_edge_px: u32) -> Result<Option<IconBitmap>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Icons (D-18)

use crate::icon::{IconBitmap, IconKey, IconProvider};
use parking_lot::Mutex;
use std::collections::HashMap;

/// Icon size to ask the shell for, in pixels.
///
/// `SHGetFileInfo` returns the shell's large icon, which is 32×32 on the
/// default DPI. One cached bitmap serves both the 15 px list glyph and the
/// 48 px tile: downscaling a sharp icon beats upscaling a blurry one, and
/// `IShellItemImageFactory` (which would give arbitrary sizes) needs COM
/// initialised on the calling thread, which this backend does not own.
const ICON_PIXELS: i32 = 32;

/// Explorer's own icons, via `SHGetFileInfo`.
#[derive(Default)]
pub struct WindowsIcons {
    /// Resolved icons, keyed by type — bounded by the number of *types* seen,
    /// never by the number of files.
    ///
    /// `parking_lot`, so there is no poisoning to swallow: `lookup` used to
    /// turn a lock poisoned anywhere in the process into a permanent, silent
    /// "this type has no icon".
    cache: Mutex<HashMap<IconKey, Option<IconBitmap>>>,
}

impl IconProvider for WindowsIcons {
    fn lookup(&self, key: &IconKey) -> Option<IconBitmap> {
        // Cache only: this runs once per visible row while a frame is built.
        self.cache.lock().get(key)?.clone()
    }

    fn resolve(&self, key: &IconKey, sample: &std::path::Path) -> bool {
        if self.cache.lock().contains_key(key) {
            return false;
        }
        let bitmap = icon_for_path(sample, matches!(key, IconKey::Directory))
            .filter(IconBitmap::is_well_formed);
        let cached_something = bitmap.is_some();
        // Failures are cached too, so an unreadable type is not retried once
        // per frame forever.
        self.cache.lock().insert(key.clone(), bitmap);
        cached_something
    }
}

/// Ask the shell for a path's icon and copy it into a BGRA buffer.
fn icon_for_path(path: &std::path::Path, is_dir: bool) -> Option<IconBitmap> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
        ReleaseDC,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    };
    use windows_sys::Win32::UI::Shell::{
        SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: every call below is a documented Win32 call on handles this
    // function creates and destroys. `SHGFI_USEFILEATTRIBUTES` keeps the
    // shell from touching the disk, so a slow or offline path answers from
    // the type association alone rather than blocking.
    unsafe {
        let mut info: SHFILEINFOW = std::mem::zeroed();
        let attributes = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        let ok = SHGetFileInfoW(
            wide.as_ptr(),
            attributes,
            &mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
        );
        if ok == 0 || info.hIcon.is_null() {
            return None;
        }
        // The icon is ours from here; every early return must free it.
        let icon = info.hIcon;

        let mut icon_info: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut icon_info) == 0 {
            DestroyIcon(icon);
            return None;
        }
        let colour_bitmap = icon_info.hbmColor;
        let mask_bitmap = icon_info.hbmMask;

        let side = ICON_PIXELS;
        let mut header: BITMAPINFO = std::mem::zeroed();
        header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        header.bmiHeader.biWidth = side;
        // Negative height asks for a top-down buffer, which is the row order
        // the rest of the pipeline expects.
        header.bmiHeader.biHeight = -side;
        header.bmiHeader.biPlanes = 1;
        header.bmiHeader.biBitCount = 32;
        header.bmiHeader.biCompression = BI_RGB;

        let len = (side * side * 4) as usize;
        let mut bgra = vec![0u8; len];
        let dc = GetDC(std::ptr::null_mut());
        let copied = GetDIBits(
            dc,
            colour_bitmap,
            0,
            side as u32,
            bgra.as_mut_ptr().cast(),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(std::ptr::null_mut(), dc);

        if !colour_bitmap.is_null() {
            DeleteObject(colour_bitmap.cast());
        }
        if !mask_bitmap.is_null() {
            DeleteObject(mask_bitmap.cast());
        }
        DestroyIcon(icon);

        if copied == 0 {
            return None;
        }

        // A 32-bit icon whose alpha channel is entirely zero is opaque with
        // the alpha left unset — drawn as-is it would be invisible.
        if bgra.iter().skip(3).step_by(4).all(|a| *a == 0) {
            for alpha in bgra.iter_mut().skip(3).step_by(4) {
                *alpha = 0xFF;
            }
        }

        Some(IconBitmap {
            width: side as u32,
            height: side as u32,
            bgra,
        })
    }
}
