//! The boundary where every OS-specific behaviour lives.
//!
//! Ferret targets macOS and Windows (D-19 defers Linux). Anything that touches
//! the Trash, the default-application registry, Finder/Explorer, or the set of
//! user places goes through [`Platform`], so the UI never writes a
//! platform-specific line and re-adding Linux is an implementation, not a
//! rewrite.
//!
//! Methods are `async` for consistency with `ferret-vfs`: moving to the Trash
//! can touch the network on some setups, and a high-latency call must be a
//! slow call rather than a frozen frame. Implementations run blocking work on
//! a pool, exactly like `LocalFs` does.

pub mod config;
mod errors;
mod icon;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub use errors::{PlatformError, Result};
pub use icon::{IconBitmap, IconKey, IconProvider, icon_provider};

use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// The backend for the platform Ferret was built for.
///
/// Linux is deferred (D-19): rather than a stub that lies, the constructor
/// fails the build target explicitly so nobody ships a half-platform.
pub fn platform() -> std::sync::Arc<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        std::sync::Arc::new(macos::MacOs)
    }
    #[cfg(target_os = "windows")]
    {
        std::sync::Arc::new(windows::Windows)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        compile_error!("Ferret currently targets macOS and Windows only (D-19).")
    }
}

/// A sidebar entry: somewhere the user can go in one click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub name: String,
    pub kind: PlaceKind,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceKind {
    /// The user's home directory.
    Home,
    Desktop,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    /// macOS `/Applications`; Windows Start-menu programs.
    Applications,
    /// A mounted volume. Windows drive roots land here too.
    Volume,
}

impl PlaceKind {
    /// The bundled icon name for this kind of place.
    ///
    /// Icon names refer to `ferret-ui`'s asset paths; the mapping lives here
    /// so every backend agrees and the UI stays icon-agnostic.
    pub fn icon_name(self) -> &'static str {
        match self {
            PlaceKind::Home => "icons/file/folder-home.svg",
            PlaceKind::Desktop => "icons/file/folder-desktop.svg",
            PlaceKind::Documents => "icons/file/folder-docs.svg",
            PlaceKind::Downloads => "icons/file/folder-downloads.svg",
            PlaceKind::Music => "icons/file/folder-music.svg",
            PlaceKind::Pictures => "icons/file/folder-images.svg",
            PlaceKind::Videos => "icons/file/folder-videos.svg",
            PlaceKind::Applications => "icons/file/apps.svg",
            PlaceKind::Volume => "icons/file/drive.svg",
        }
    }
}

/// Everything Ferret needs from the operating system's shell.
#[async_trait]
pub trait Platform: Send + Sync + 'static {
    /// Open a path with whatever application the OS associates with it.
    ///
    /// This is double-click. Directories open in Ferret itself before this is
    /// ever called; everything else is delegated to the shell.
    async fn open_with_default(&self, path: &Path) -> Result<()>;

    /// Show the path in the platform file manager with it selected.
    ///
    /// Used by "Reveal in Finder" / "Show in Explorer".
    async fn reveal_in_file_manager(&self, path: &Path) -> Result<()>;

    /// Move a path to the platform Trash / Recycle Bin.
    ///
    /// Gate 5: no delete path may exist that does not come through here, or
    /// through `Vfs::remove(RemoveMode::Permanent)` behind its confirmation.
    ///
    /// Deleting irreversibly is deliberately *not* on this trait. It was, and
    /// both backends implemented it identically with `std::fs` — nothing about
    /// it is OS-specific, and `RemoveMode::Permanent` in `ferret-vfs` already
    /// existed for the same job. The Trash is the part that needs a platform.
    async fn move_to_trash(&self, path: &Path) -> Result<()>;

    /// The standard Places section: home and the well-known folders that
    /// exist on this machine. Missing folders are omitted, not shown greyed.
    fn places(&self) -> Vec<Place>;

    /// Mounted volumes, ready to browse.
    fn volumes(&self) -> Vec<Place>;
}
