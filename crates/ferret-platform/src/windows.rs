//! The Windows backend.
//!
//! Opening files with their default application goes through `explorer.exe`,
//! which resolves the shell association exactly like a double-click on the
//! desktop; revealing uses its `/select` form. The Recycle Bin goes through
//! the `trash` crate's `SHFileOperationW` path, which is what Explorer itself
//! uses and therefore records Put-Back metadata correctly.

use crate::config::MAX_VOLUMES;
use crate::errors::{PlatformError, Result};
use crate::{Place, PlaceKind, Platform};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// The Windows implementation of [`Platform`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Windows;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

    async fn move_to_trash(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        if !owned.exists() {
            return Err(PlatformError::NotFound { path: owned });
        }
        smol::unblock(move || trash::delete(&owned))
            .await
            .map_err(|source| match source {
                trash::Error::CouldNotAccess { .. } => {
                    PlatformError::PermissionDenied { path: owned }
                }
                other => PlatformError::Io {
                    action: "move to the Recycle Bin",
                    path: owned,
                    source: std::io::Error::other(other.to_string()),
                },
            })
    }

    async fn remove_permanently(&self, path: &Path) -> Result<()> {
        let owned = path.to_path_buf();
        smol::unblock(move || {
            let md = std::fs::symlink_metadata(&owned)
                .map_err(|source| PlatformError::from_io("read", &owned, source))?;
            let outcome = if md.is_dir() {
                std::fs::remove_dir_all(&owned)
            } else {
                std::fs::remove_file(&owned)
            };
            outcome.map_err(|source| PlatformError::from_io("delete", &owned, source))
        })
        .await
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
