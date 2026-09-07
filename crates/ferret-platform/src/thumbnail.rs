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
/// Every backend currently declines; the macOS (`QLThumbnailGenerator`) and
/// Windows (`IShellItemImageFactory` in a supervised helper) implementations
/// arrive with step 08. The constructor exists now so the pane wires one
/// call path from the start instead of growing a second one later.
pub fn thumbnail_provider() -> Arc<dyn ThumbnailProvider> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(super::macos::ThumbnailDeclined)
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
