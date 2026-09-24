//! Best-effort OS thumbnails, behind one trait (PV-D07).
//!
//! Quick Look on macOS and `IShellItemImageFactory` on Windows provide
//! thumbnails for installed document and media handlers — HEIC, RAW, Office,
//! video posters. Coverage is capability-dependent, not universal, and the
//! same rules as the icon work apply:
//!
//! 1. **The OS is never called on the render path.** Resolution happens off
//!    the render path (and for Windows, in a supervised helper with its own
//!    COM apartment — never on the UI thread, never on the shared CPU pool).
//! 2. **Keyed by path, not by type**, so the cache is the on-disk thumbnail
//!    cache from F-33 rather than a small in-memory map.
//!
//! The native implementations land with PDF page rendering (preview-plan
//! step 08). Until then every backend declines and the pane shows metadata
//! with *Open with…* — a finished state, not a failure state.
//!
//! **Native exception to byte budgets.** Native thumbnail services receive
//! paths and may read the whole file internally. Requests are limited to
//! local regular files within
//! [`NATIVE_THUMBNAIL_INPUT_LIMIT`](super::config::NATIVE_THUMBNAIL_INPUT_LIMIT);
//! network shares and known cloud placeholders are excluded. These checks
//! reduce exposure but cannot prove zero provider I/O — strict bounded-I/O
//! mode disables native providers entirely.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use super::{IconBitmap, Result};

pub fn native_thumbnail_path_allowed(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if text.starts_with("\\\\")
        || text.starts_with("//")
        || text.starts_with("\\\\?\\UNC")
        || path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("icloud"))
    {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        macos_local_volume(path)
    }
    #[cfg(target_os = "windows")]
    {
        return windows_fixed_drive(path);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn macos_local_volume(path: &Path) -> bool {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt as _;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return false;
    }
    let stats = unsafe { stats.assume_init() };
    stats.f_flags & libc::MNT_LOCAL as u32 != 0
}

#[cfg(target_os = "windows")]
fn windows_fixed_drive(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

    let root: Vec<u16> = path.components().next().map_or_else(Vec::new, |component| {
        component.as_os_str().encode_wide().chain([0]).collect()
    });
    !root.is_empty() && GetDriveTypeW(root.as_ptr()) == DRIVE_FIXED
}

/// A best-effort OS thumbnail for one file.
///
/// Returns `Ok(None)` when the service declines — no handler installed, file
/// over the input limit, remote path — and the caller falls through to
/// `Unsupported`. An icon dressed as a preview is a lie, so providers must
/// fail rather than return a generic document icon.
#[async_trait]
pub trait ThumbnailProvider: Send + Sync + 'static {
    async fn thumbnail(&self, path: &Path, longest_edge_px: u32) -> Result<Option<IconBitmap>>;
}

/// The backend for the platform Ferret was built for.
///
/// macOS serves Quick Look thumbnails today. Windows still declines: its
/// path runs third-party COM in a supervised helper (PV-R05), and until that
/// isolation exists a metadata fallback ships instead of an uncontained
/// native call. The constructor exists so the pane wires one call path from
/// the start instead of growing a second one later.
pub fn thumbnail_provider() -> Arc<dyn ThumbnailProvider> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(super::macos::MacThumbnails)
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(super::windows::ThumbnailDeclined)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Ferret currently targets macOS and Windows only (D-19).")
    }
}

/// Whether the platform backend can answer at all.
///
/// Callers check this *before* paying canonicalization, disk lookup and an
/// async hop for an OS-covered format: on Windows every HEIC/PDF/MP4 tile
/// would otherwise cost all three just to learn the answer is `None`.
/// macOS answers today; Windows declines until the PV-R05 helper lands.
pub fn thumbnail_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(target_os = "windows")]
    {
        false
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Ferret currently targets macOS and Windows only (D-19).")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_paths_reject_cloud_and_unc_inputs() {
        assert!(!native_thumbnail_path_allowed(Path::new(
            "/tmp/photo.icloud"
        )));
        assert!(!native_thumbnail_path_allowed(Path::new(
            r"\\server\share\photo.heic"
        )));
        assert!(!native_thumbnail_path_allowed(Path::new(
            r"\\?\UNC\server\share\photo.heic"
        )));
    }
}
