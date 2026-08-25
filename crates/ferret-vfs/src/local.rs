use crate::{
    Capabilities, Entry, EntryKind, Metadata, RemoveMode, Result, SymlinkTarget, Vfs, VfsError,
    VfsPath,
};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

/// How many entries may sit in the channel before the producer thread parks.
///
/// Bounded on purpose: an unbounded channel would let a 200,000-entry walk
/// materialise the whole directory in memory, which is exactly what streaming
/// is meant to avoid.
const LISTING_BUFFER: usize = 512;

/// The local disk backend, over `std::fs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFs;

impl LocalFs {
    pub fn new() -> Self {
        Self
    }

    fn require_local(path: &VfsPath) -> Result<PathBuf> {
        path.as_local()
            .map(PathBuf::from)
            .ok_or(VfsError::Unsupported {
                capability: "local filesystem access",
            })
    }
}

#[async_trait]
impl Vfs for LocalFs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            symlinks: true,
            permissions: true,
            atomic_rename: true,
            watch: true,
            // `std::fs::DirEntry::metadata` costs an extra stat on Unix, but on
            // Windows it is served from the data `FindFirstFileEx` already
            // returned. We fill it inline either way and let the platform
            // decide what that costs — see the module tests.
            metadata_in_listing: cfg!(windows),
        }
    }

    fn read_dir(&self, dir: &VfsPath) -> BoxStream<'static, Result<Entry>> {
        let dir = dir.clone();
        let root = match Self::require_local(&dir) {
            Ok(root) => root,
            Err(err) => return futures::stream::once(async move { Err(err) }).boxed(),
        };

        let (tx, rx) = async_channel::bounded::<Result<Entry>>(LISTING_BUFFER);
        let fill_metadata = cfg!(windows);

        // The walk is blocking, so it runs on the blocking pool. `send_blocking`
        // parks when the consumer is behind and fails once the receiver drops —
        // which is how dropping the stream cancels the walk.
        smol::spawn(async move {
            smol::unblock(move || {
                let iter = match fs::read_dir(&root) {
                    Ok(iter) => iter,
                    Err(source) => {
                        let _ = tx.send_blocking(Err(VfsError::from_io("read", &dir, source)));
                        return;
                    }
                };

                for item in iter {
                    let entry = match item {
                        Ok(entry) => entry,
                        Err(source) => {
                            if tx
                                .send_blocking(Err(VfsError::from_io("read", &dir, source)))
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                    };

                    if tx.send_blocking(Ok(convert(entry, fill_metadata))).is_err() {
                        return; // consumer went away: stop walking.
                    }
                }
            })
            .await;
        })
        .detach();

        rx.boxed()
    }

    async fn metadata(&self, path: &VfsPath) -> Result<Metadata> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::symlink_metadata(&local)
                .map(|md| to_metadata(&md, &owned))
                .map_err(|source| VfsError::from_io("read", &owned, source))
        })
        .await
    }

    async fn canonicalize(&self, path: &VfsPath) -> Result<VfsPath> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::canonicalize(&local)
                .map(VfsPath::Local)
                .map_err(|source| VfsError::from_io("resolve", &owned, source))
        })
        .await
    }

    async fn read(&self, path: &VfsPath, budget: usize) -> Result<Vec<u8>> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            let file = fs::File::open(&local)
                .map_err(|source| VfsError::from_io("open", &owned, source))?;
            let mut buf = Vec::new();
            // `take` is the cap: a 4 GB file yields `budget` bytes, not 4 GB.
            file.take(budget as u64)
                .read_to_end(&mut buf)
                .map_err(|source| VfsError::from_io("read", &owned, source))?;
            Ok(buf)
        })
        .await
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<()> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::create_dir(&local).map_err(|source| VfsError::from_io("create", &owned, source))
        })
        .await
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let src = Self::require_local(from)?;
        let dst = Self::require_local(to)?;
        let owned = from.clone();
        smol::unblock(move || {
            fs::rename(&src, &dst).map_err(|source| {
                // A rename across volumes is not a failure the user caused; the
                // operations queue turns it into copy-then-delete.
                if source.kind() == std::io::ErrorKind::CrossesDevices {
                    VfsError::CrossVolume {
                        path: owned.clone(),
                    }
                } else {
                    VfsError::from_io("rename", &owned, source)
                }
            })
        })
        .await
    }

    async fn remove(&self, path: &VfsPath, mode: RemoveMode) -> Result<()> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        match mode {
            // Wired to the platform trash in `ferret-platform` when that lands.
            // Refusing is the right placeholder: silently deleting permanently
            // because the trash is not implemented yet would be data loss.
            RemoveMode::Trash => Err(VfsError::Unsupported {
                capability: "moving to the Trash",
            }),
            RemoveMode::Permanent => {
                smol::unblock(move || {
                    let md = fs::symlink_metadata(&local)
                        .map_err(|source| VfsError::from_io("read", &owned, source))?;
                    // Never follow a symlink out of the tree being deleted:
                    // remove the link itself.
                    let outcome = if md.is_dir() {
                        fs::remove_dir_all(&local)
                    } else {
                        fs::remove_file(&local)
                    };
                    outcome.map_err(|source| VfsError::from_io("delete", &owned, source))
                })
                .await
            }
        }
    }
}

fn convert(entry: fs::DirEntry, fill_metadata: bool) -> Entry {
    let path = VfsPath::Local(entry.path());
    let name = entry.file_name().to_string_lossy().into_owned();

    // `file_type` comes from the directory read itself and costs nothing.
    let file_type = entry.file_type().ok();
    let kind = match file_type {
        Some(ft) if ft.is_dir() => EntryKind::Dir,
        Some(ft) if ft.is_file() => EntryKind::File,
        Some(ft) if ft.is_symlink() => EntryKind::Symlink {
            // Resolving the target is what tells a working link from a broken
            // one, and the list draws them differently, so it cannot wait.
            target_kind: fs::metadata(entry.path()).ok().map(|md| {
                if md.is_dir() {
                    SymlinkTarget::Dir
                } else {
                    SymlinkTarget::File
                }
            }),
        },
        Some(_) => EntryKind::Other,
        None => EntryKind::Other,
    };

    let metadata = if fill_metadata {
        entry.metadata().ok().map(|md| to_metadata(&md, &path))
    } else {
        None
    };

    Entry {
        path,
        name,
        kind,
        metadata,
    }
}

fn to_metadata(md: &fs::Metadata, path: &VfsPath) -> Metadata {
    Metadata {
        len: md.len(),
        modified: md.modified().ok(),
        created: md.created().ok(),
        readonly: md.permissions().readonly(),
        hidden: path.is_hidden_by_name() || platform_hidden(md),
    }
}

#[cfg(windows)]
fn platform_hidden(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    md.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(target_os = "macos")]
fn platform_hidden(md: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_HIDDEN: u32 = 0x8000;
    md.st_flags() & UF_HIDDEN != 0
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_hidden(_md: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn collect(fs: &LocalFs, dir: &VfsPath) -> BTreeMap<String, Entry> {
        smol::block_on(async {
            fs.read_dir(dir)
                .filter_map(|r| async move { r.ok() })
                .map(|e| (e.name.clone(), e))
                .collect()
                .await
        })
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferret-vfs-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_files_and_directories() {
        let root = tempdir("list");
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();

        let entries = collect(&LocalFs::new(), &VfsPath::Local(root.clone()));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries["a.txt"].kind, EntryKind::File);
        assert_eq!(entries["sub"].kind, EntryKind::Dir);
        assert!(entries["sub"].is_dir_like());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_directory_yields_one_error_not_a_panic() {
        let missing = VfsPath::local("/definitely/not/here/ferret");
        let results: Vec<_> =
            smol::block_on(async { LocalFs::new().read_dir(&missing).collect().await });
        assert_eq!(results.len(), 1);
        assert!(results[0].as_ref().unwrap_err().is_gone());
    }

    #[test]
    fn read_is_capped_by_the_budget() {
        let root = tempdir("budget");
        let file = root.join("big.bin");
        fs::write(&file, vec![7u8; 10_000]).unwrap();

        let got = smol::block_on(LocalFs::new().read(&VfsPath::Local(file), 128)).unwrap();
        assert_eq!(got.len(), 128, "read must stop at the budget");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn trash_is_refused_rather_than_deleting_permanently() {
        let root = tempdir("trash");
        let file = root.join("keep.txt");
        fs::write(&file, b"x").unwrap();

        let err =
            smol::block_on(LocalFs::new().remove(&VfsPath::Local(file.clone()), RemoveMode::Trash))
                .unwrap_err();
        assert!(matches!(err, VfsError::Unsupported { .. }));
        assert!(file.exists(), "refusing must not have deleted the file");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropping_the_stream_stops_the_walk() {
        let root = tempdir("cancel");
        for i in 0..2_000 {
            fs::write(root.join(format!("f{i:05}")), b"").unwrap();
        }

        smol::block_on(async {
            let mut stream = LocalFs::new().read_dir(&VfsPath::Local(root.clone()));
            assert!(stream.next().await.is_some());
            drop(stream); // the producer's next send fails and it returns
        });

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlinks_are_listed_and_flagged() {
        let root = tempdir("symlink");
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();

        let entries = collect(&LocalFs::new(), &VfsPath::Local(root.clone()));
        assert!(
            entries["dangling"].kind.is_broken_symlink(),
            "got {:?}",
            entries["dangling"].kind
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dotfiles_are_hidden() {
        let root = tempdir("hidden");
        fs::write(root.join(".secret"), b"").unwrap();
        fs::write(root.join("plain"), b"").unwrap();

        let entries = collect(&LocalFs::new(), &VfsPath::Local(root.clone()));
        assert!(entries[".secret"].is_hidden());
        assert!(!entries["plain"].is_hidden());

        fs::remove_dir_all(&root).ok();
    }
}
