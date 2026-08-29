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

fn validate_terminal_directory(path: &Path) -> Result<()> {
    if path.is_dir() {
        Ok(())
    } else if path.exists() {
        Err(PlatformError::NotADirectory {
            path: path.to_path_buf(),
        })
    } else {
        Err(PlatformError::NotFound {
            path: path.to_path_buf(),
        })
    }
}

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

    /// Open a system terminal with `path` as its working directory (D-29).
    ///
    /// This is independent of the embedded terminal and remains useful as an
    /// escape hatch when a command needs the user's full terminal application.
    async fn open_in_terminal(&self, path: &Path) -> Result<()>;

    /// Move a path to the platform Trash / Recycle Bin.
    ///
    /// Gate 5: no delete path may exist that does not come through here
    /// (or through [`Platform::remove_permanently`] behind its confirmation).
    async fn move_to_trash(&self, path: &Path) -> Result<()>;

    /// Delete irreversibly. Only ever reachable behind an explicit
    /// confirmation — see `settings.browser.confirm_permanent_delete`.
    async fn remove_permanently(&self, path: &Path) -> Result<()>;

    /// The standard Places section: home and the well-known folders that
    /// exist on this machine. Missing folders are omitted, not shown greyed.
    fn places(&self) -> Vec<Place>;

    /// Mounted volumes, ready to browse.
    fn volumes(&self) -> Vec<Place>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_directory_rejects_files_and_missing_paths() {
        let unique = format!(
            "ferret-terminal-directory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let file = std::env::temp_dir().join(unique);
        std::fs::write(&file, b"not a directory").unwrap();

        assert!(matches!(
            validate_terminal_directory(&file),
            Err(PlatformError::NotADirectory { .. })
        ));
        std::fs::remove_file(&file).unwrap();
        assert!(matches!(
            validate_terminal_directory(&file),
            Err(PlatformError::NotFound { .. })
        ));
    }

    /// Launches a real application, so this is an explicit manual gate rather
    /// than part of the ordinary unit suite.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "opens Terminal.app"]
    fn open_in_terminal_smoke() {
        let dir = std::env::var_os("FERRET_TERMINAL_SMOKE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        smol::block_on(platform().open_in_terminal(&dir)).unwrap();
    }
}
