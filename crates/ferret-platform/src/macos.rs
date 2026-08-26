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
