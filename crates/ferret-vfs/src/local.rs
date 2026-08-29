use crate::config::{FIRST_LISTING_BATCH, LISTING_BATCH_SIZE, LISTING_BUFFER};
use crate::{
    Capabilities, DirEntryBatch, Entry, EntryKind, Metadata, ReadDirStream, RemoveMode, Result,
    SymlinkTarget, Vfs, VfsError, VfsPath,
};
use async_trait::async_trait;
use futures::stream::StreamExt;
use rayon::prelude::*;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

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
            // True on both platforms now (D-33). On Windows it always was:
            // `FindFirstFileEx` returns size, times and attributes with the
            // name. On Unix it is an extra `stat` per entry, but those stats
            // run in parallel on Ferret's own pool as each batch is read, so
            // the row is complete when it appears instead of filling in for
            // the next two minutes. The flag stays because a remote backend,
            // where a second round trip is genuinely expensive, will want to
            // say `false` and let the file pane chase metadata separately.
            metadata_in_listing: true,
        }
    }

    fn read_dir(&self, dir: &VfsPath) -> ReadDirStream {
        let dir = dir.clone();
        let root = match Self::require_local(&dir) {
            Ok(root) => root,
            Err(err) => return futures::stream::once(async move { Err(err) }).boxed(),
        };

        let (tx, rx) = async_channel::bounded::<Result<DirEntryBatch>>(LISTING_BUFFER);

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

                // Ramped rather than flat, because a batch now carries a
                // `stat` per entry and so decides how long the first row
                // waits (A-35).
                let mut batch_size = FIRST_LISTING_BATCH;
                let mut pending = Vec::with_capacity(batch_size);
                for item in iter {
                    let entry = match item {
                        Ok(entry) => entry,
                        Err(source) => {
                            if !pending.is_empty()
                                && tx
                                    .send_blocking(Ok(resolve(&mut pending, batch_size)))
                                    .is_err()
                            {
                                return;
                            }
                            if tx
                                .send_blocking(Err(VfsError::from_io("read", &dir, source)))
                                .is_err()
                            {
                                return;
                            }
                            continue;
                        }
                    };

                    pending.push(entry);
                    if pending.len() < batch_size {
                        continue;
                    }
                    if tx
                        .send_blocking(Ok(resolve(&mut pending, batch_size)))
                        .is_err()
                    {
                        return; // consumer went away: stop walking.
                    }
                    batch_size = (batch_size * 2).min(LISTING_BATCH_SIZE);
                }

                if !pending.is_empty() {
                    let _ = tx.send_blocking(Ok(resolve(&mut pending, batch_size)));
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

    async fn metadata_batch(&self, paths: Vec<VfsPath>) -> Vec<(VfsPath, Result<Metadata>)> {
        // One blocking-pool dispatch per UI batch. `symlink_metadata` remains
        // serial inside that task, but this avoids one task spawn and wakeup
        // per row while preserving per-path errors for the caller.
        smol::unblock(move || {
            paths
                .into_iter()
                .map(|path| {
                    let result = Self::require_local(&path).and_then(|local| {
                        fs::symlink_metadata(&local)
                            .map(|md| to_metadata(&md, &path))
                            .map_err(|source| VfsError::from_io("read", &path, source))
                    });
                    (path, result)
                })
                .collect()
        })
        .await
    }

    async fn probe(&self, path: &VfsPath) -> Result<Entry> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            let md = fs::symlink_metadata(&local)
                .map_err(|source| VfsError::from_io("read", &owned, source))?;

            let kind = if md.is_symlink() {
                EntryKind::Symlink {
                    target_kind: fs::metadata(&local).ok().map(|target| {
                        if target.is_dir() {
                            SymlinkTarget::Dir
                        } else {
                            SymlinkTarget::File
                        }
                    }),
                }
            } else if md.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };

            let link_target = kind
                .is_broken_symlink()
                .then(|| fs::read_link(&local).ok())
                .flatten()
                .map(std::sync::Arc::from);

            let name = local.file_name().unwrap_or_default().to_string_lossy();
            Ok(Entry::new(owned.clone(), name, kind)
                .with_metadata(Some(to_metadata(&md, &owned)))
                .with_link_target(link_target))
        })
        .await
    }

    async fn canonicalize(&self, path: &VfsPath) -> Result<VfsPath> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::canonicalize(&local)
                .map(VfsPath::local)
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

    async fn create_file(&self, path: &VfsPath) -> Result<()> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::File::create_new(&local)
                .map(|_| ())
                .map_err(|source| VfsError::from_io("create", &owned, source))
        })
        .await
    }

    async fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let src = Self::require_local(from)?;
        let dst = Self::require_local(to)?;
        smol::unblock(move || copy_tree(&src, &dst)).await
    }

    fn watch(&self, dir: &VfsPath) -> Result<crate::Watch> {
        let local = Self::require_local(dir)?;
        crate::Watch::start(local).map_err(|source| {
            VfsError::from_io("watch", dir, std::io::Error::new(source.kind(), source))
        })
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
                    remove_tree(&local)
                        .map_err(|source| VfsError::from_io(source.action, &owned, source.source))
                })
                .await
            }
        }
    }
}

/// Turn one batch of directory entries into rows, resolving every syscall
/// they need in parallel (D-33).
///
/// This is where `metadata_in_listing` becomes true on Unix. Each entry needs
/// up to three calls — `stat` for size and dates, a second `stat` to tell a
/// working symlink from a broken one, and `readlink` for the broken ones — and
/// they are independent, so they run across Ferret's pool rather than one
/// after another on this thread.
///
/// Batch-at-a-time rather than whole-listing-at-a-time keeps two promises: the
/// first rows still arrive after `LISTING_BATCH_SIZE` names rather than after
/// the whole directory, and a dropped stream leaves at most one batch of
/// stats in flight rather than a directory's worth.
fn resolve(entries: &mut Vec<fs::DirEntry>, next_capacity: usize) -> DirEntryBatch {
    let batch = std::mem::take(entries);
    entries.reserve(next_capacity);
    crate::pool::install(move || batch.into_par_iter().map(convert).collect())
}

fn convert(entry: fs::DirEntry) -> Entry {
    // `DirEntry::path` joins the parent and file name. Keep that allocation
    // alive for every path-dependent operation below rather than rebuilding it
    // for symlink resolution and broken-link reporting.
    let path = VfsPath::local(entry.path());
    let name = entry.file_name();
    let name = name.to_string_lossy();
    let local = path
        .as_local()
        .expect("a LocalFs directory entry always has a local path");

    // `file_type` comes from the directory read itself and costs nothing.
    let file_type = entry.file_type().ok();
    let kind = match file_type {
        Some(ft) if ft.is_dir() => EntryKind::Dir,
        Some(ft) if ft.is_file() => EntryKind::File,
        Some(ft) if ft.is_symlink() => EntryKind::Symlink {
            // Resolving the target is what tells a working link from a broken
            // one, and the list draws them differently, so it cannot wait.
            target_kind: fs::metadata(local).ok().map(|md| {
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

    // Always, on both platforms (D-33). On Windows this is served from what
    // `FindFirstFileEx` already returned; on Unix it is a `stat`, running here
    // on a pool thread rather than on the file pane's second pass.
    let metadata = entry.metadata().ok().map(|md| to_metadata(&md, &path));

    // One `readlink`, and only for links that resolve to nothing — the row
    // has to name what it is pointing at.
    let link_target = kind
        .is_broken_symlink()
        .then(|| fs::read_link(local).ok())
        .flatten()
        .map(std::sync::Arc::from);

    Entry::new(path.clone(), name, kind)
        .with_metadata(metadata)
        .with_link_target(link_target)
}

fn to_metadata(md: &fs::Metadata, path: &VfsPath) -> Metadata {
    Metadata {
        len: md.len(),
        modified: md.modified().ok(),
        hidden: path.is_hidden_by_name() || platform_hidden(md),
    }
}

/// An `io::Error` that remembers which verb was being attempted.
///
/// The parallel paths below cross a `rayon` boundary where the `VfsPath` is
/// not in hand, so the action travels with the error and the caller attaches
/// the path it already knows.
struct FileOpError {
    action: &'static str,
    source: std::io::Error,
}

impl FileOpError {
    fn new(action: &'static str, source: std::io::Error) -> Self {
        Self { action, source }
    }
}

/// Delete a file, a link, or a whole tree.
///
/// Parallel across the top level only, and every subtree beneath it goes
/// through `std::fs::remove_dir_all` (D-34). That split is deliberate. std's
/// implementation walks with `openat` and refuses to descend a directory that
/// was swapped for a symlink while it was working; a path-based parallel walk
/// would be faster still and would give that up. On the one operation where
/// being wrong destroys data outside the tree the user pointed at, that is not
/// a trade worth making — so the parallelism goes where it is free, which is
/// across independent children.
///
/// It is not a micro-optimisation: the common shape is a directory of many
/// files, and those unlinks run across the pool rather than one at a time.
fn remove_tree(path: &std::path::Path) -> std::result::Result<(), FileOpError> {
    let md = fs::symlink_metadata(path).map_err(|e| FileOpError::new("read", e))?;
    if !md.is_dir() {
        // Never follow a symlink out of the tree being deleted: remove the
        // link itself.
        return fs::remove_file(path).map_err(|e| FileOpError::new("delete", e));
    }

    let children: Vec<PathBuf> = fs::read_dir(path)
        .map_err(|e| FileOpError::new("read", e))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();

    // Collected rather than short-circuited: a delete that hits one
    // permission-denied child should still remove everything it can, and then
    // say what failed. `remove_dir` below fails anyway if anything is left.
    let failures: Vec<FileOpError> = crate::pool::install(move || {
        children
            .into_par_iter()
            .filter_map(|child| {
                let md = fs::symlink_metadata(&child).ok()?;
                let outcome = if md.is_dir() {
                    fs::remove_dir_all(&child)
                } else {
                    // A symlink is unlinked, never followed — including one
                    // that points outside the tree being deleted.
                    fs::remove_file(&child)
                };
                outcome.err().map(|e| FileOpError::new("delete", e))
            })
            .collect()
    });
    if let Some(failure) = failures.into_iter().next() {
        return Err(failure);
    }

    fs::remove_dir(path).map_err(|e| FileOpError::new("delete", e))
}

/// Copy one file, or reproduce one symlink as a link.
fn copy_one(
    src: &std::path::Path,
    dst: &std::path::Path,
    is_symlink: bool,
) -> std::result::Result<(), FileOpError> {
    if !is_symlink {
        return fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| FileOpError::new("copy", e));
    }
    #[cfg(unix)]
    {
        let target = fs::read_link(src).map_err(|e| FileOpError::new("read", e))?;
        std::os::unix::fs::symlink(&target, dst).map_err(|e| FileOpError::new("create", e))
    }
    #[cfg(windows)]
    {
        let _ = dst;
        Err(FileOpError::new(
            "copy",
            std::io::Error::other("copying symlinks is not supported on Windows"),
        ))
    }
}

/// Recursive copy used by paste and cross-volume moves.
///
/// Symlinks are copied as links, not followed — copying a tree that happens
/// to contain a link back to `/` must not become a walk of the universe. That
/// is `follow_links(false)` plus [`copy_one`], and it is the reason the link
/// tests exist.
///
/// Two phases (D-34). `jwalk` enumerates the tree across Ferret's pool, which
/// is where it earns its place: a deep tree is many directories, and
/// `fs::read_dir` recursion reads them strictly one after another. Directories
/// are created as they are yielded — the walk yields a parent before its
/// children, which is exactly the order `create_dir` needs — and the file
/// copies, which are the expensive part, then run in parallel.
fn copy_tree(src: &PathBuf, dst: &PathBuf) -> Result<()> {
    fn at(path: &std::path::Path, err: FileOpError) -> VfsError {
        VfsError::from_io(err.action, &VfsPath::local(path), err.source)
    }

    let md = fs::symlink_metadata(src)
        .map_err(|source| VfsError::from_io("read", &VfsPath::local(src.as_path()), source))?;

    if md.is_symlink() || !md.is_dir() {
        return copy_one(src, dst, md.is_symlink()).map_err(|e| at(src, e));
    }

    // Refuses to overwrite: conflict policy is the caller's decision, and this
    // signature makes "silently replaced something" impossible to write by
    // accident.
    fs::create_dir(dst)
        .map_err(|source| VfsError::from_io("create", &VfsPath::local(dst.as_path()), source))?;

    let mut files = Vec::new();
    for entry in jwalk::WalkDir::new(src)
        .min_depth(1)
        // Ferret's hidden logic is richer than a leading dot and is a user
        // toggle besides; a copy that silently skipped dotfiles would be a
        // data-loss bug wearing a feature's clothes.
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(crate::pool::parallelism())
    {
        let entry = entry.map_err(|source| {
            VfsError::from_io("read", &VfsPath::local(src.as_path()), source.into())
        })?;
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(src) else {
            continue;
        };
        let target = dst.join(relative);

        // A symlink to a directory is a link to reproduce, never a directory
        // to descend — `follow_links(false)` already stops the walk, and this
        // keeps it out of the `create_dir` branch.
        if entry.file_type().is_dir() {
            fs::create_dir(&target).map_err(|source| {
                VfsError::from_io("create", &VfsPath::local(target.as_path()), source)
            })?;
        } else {
            files.push((path, target, entry.file_type().is_symlink()));
        }
    }

    let failure = crate::pool::install(move || {
        files
            .into_par_iter()
            .filter_map(|(from, to, is_symlink)| {
                copy_one(&from, &to, is_symlink).err().map(|e| (from, e))
            })
            .find_any(|_| true)
    });
    match failure {
        Some((path, err)) => Err(at(&path, err)),
        None => Ok(()),
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
    use crate::config::FIRST_LISTING_BATCH;
    use std::collections::BTreeMap;

    fn collect(fs: &LocalFs, dir: &VfsPath) -> BTreeMap<String, Entry> {
        smol::block_on(async {
            let mut stream = fs.read_dir(dir);
            let mut entries = BTreeMap::new();
            while let Some(item) = stream.next().await {
                if let Ok(batch) = item {
                    for entry in batch {
                        entries.insert(entry.name().to_string(), entry);
                    }
                }
            }
            entries
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

        let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
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

        let got = smol::block_on(LocalFs::new().read(&VfsPath::local(file), 128)).unwrap();
        assert_eq!(got.len(), 128, "read must stop at the budget");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn trash_is_refused_rather_than_deleting_permanently() {
        let root = tempdir("trash");
        let file = root.join("keep.txt");
        fs::write(&file, b"x").unwrap();

        let err =
            smol::block_on(LocalFs::new().remove(&VfsPath::local(file.clone()), RemoveMode::Trash))
                .unwrap_err();
        assert!(matches!(err, VfsError::Unsupported { .. }));
        assert!(file.exists(), "refusing must not have deleted the file");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dropping_the_stream_stops_the_walk() {
        let root = tempdir("cancel");
        // More batches than the bounded channel can accept ensures the
        // producer reaches a send after the consumer has gone away.
        for i in 0..(LISTING_BATCH_SIZE * (LISTING_BUFFER + 2)) {
            fs::write(root.join(format!("f{i:05}")), b"").unwrap();
        }

        smol::block_on(async {
            let mut stream = LocalFs::new().read_dir(&VfsPath::local(root.clone()));
            assert!(stream.next().await.is_some());
            drop(stream); // the producer's next send fails and it returns
        });

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn listings_arrive_in_bounded_batches() {
        let root = tempdir("batches");
        // Enough to get past the ramp and into the steady state.
        let expected = LISTING_BATCH_SIZE * 2 + 1;
        for i in 0..expected {
            fs::write(root.join(format!("f{i:05}")), b"").unwrap();
        }

        let batches: Vec<DirEntryBatch> = smol::block_on(async {
            LocalFs::new()
                .read_dir(&VfsPath::local(root.clone()))
                .filter_map(|item| async move { item.ok() })
                .collect()
                .await
        });

        // The first batch is small, so the first rows appear without waiting
        // on a `stat` for four thousand of them (A-35).
        assert_eq!(batches[0].len(), FIRST_LISTING_BATCH);
        // Then it doubles, up to the ceiling and no further.
        assert_eq!(batches[1].len(), FIRST_LISTING_BATCH * 2);
        assert!(
            batches.iter().all(|b| b.len() <= LISTING_BATCH_SIZE),
            "no batch may exceed the ceiling: {:?}",
            batches.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert!(
            batches.iter().any(|b| b.len() == LISTING_BATCH_SIZE),
            "a listing this size must reach the steady state"
        );

        // And nothing is lost or duplicated on the way.
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), expected);
        assert!(batches.iter().all(|batch| !batch.is_empty()));

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlinks_are_listed_and_flagged() {
        let root = tempdir("symlink");
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();

        let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
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

        let entries = collect(&LocalFs::new(), &VfsPath::local(root.clone()));
        assert!(entries[".secret"].is_hidden());
        assert!(!entries["plain"].is_hidden());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn copy_copies_files_and_trees_without_following_links() {
        let root = tempdir("copy");
        let src = root.join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("nested/b.txt"), b"beta").unwrap();

        let fs_ = LocalFs::new();
        smol::block_on(async {
            fs_.copy(
                &VfsPath::local(src.clone()),
                &VfsPath::local(root.join("dst")),
            )
            .await
            .unwrap();
            // Refuse to overwrite.
            assert!(
                fs_.copy(
                    &VfsPath::local(src.clone()),
                    &VfsPath::local(root.join("dst"))
                )
                .await
                .is_err()
            );
        });

        assert_eq!(fs::read(root.join("dst/a.txt")).unwrap(), b"alpha");
        assert_eq!(fs::read(root.join("dst/nested/b.txt")).unwrap(), b"beta");

        fs::remove_dir_all(&root).ok();
    }

    /// `LocalFs::watch` reaches a real backend watch.
    ///
    /// Polled with a deadline and retried rather than awaited: the registry
    /// shares one backend stream, and registering a directory restarts it, so
    /// a write landing inside another test's restart is genuinely lost (see
    /// `watch.rs`). Blocking on `recv()` would report that as a hung test
    /// suite rather than as the rare miss it is.
    #[cfg(unix)]
    #[test]
    fn watch_reports_changes() {
        use super::super::watch::WatchEvent;
        let root = tempdir("watch");
        let watch = LocalFs::new().watch(&VfsPath::local(root.clone())).unwrap();

        for attempt in 0..20 {
            let name = format!("x{attempt}");
            fs::write(root.join(&name), b"").unwrap();

            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                while let Ok(event) = watch.events.try_recv() {
                    let WatchEvent::Changed(paths) = event else {
                        continue;
                    };
                    if paths.iter().any(|p| p.ends_with(&name)) {
                        fs::remove_dir_all(&root).ok();
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        fs::remove_dir_all(&root).ok();
        panic!("LocalFs::watch never reported a write");
    }
}

#[cfg(test)]
mod link_target_tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn only_broken_links_carry_their_target() {
        let root = std::env::temp_dir().join(format!(
            "ferret-vfs-links-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("real.txt", root.join("works")).unwrap();
        std::os::unix::fs::symlink("/nowhere/at/all", root.join("dead")).unwrap();

        let fs_impl = LocalFs::new();
        let entries: Vec<Entry> = smol::block_on(async {
            fs_impl
                .read_dir(&VfsPath::local(root.clone()))
                .filter_map(|r| async move { r.ok() })
                .flat_map(futures::stream::iter)
                .collect()
                .await
        });
        let by_name = |name: &str| {
            entries
                .iter()
                .find(|e| e.name() == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .clone()
        };

        // The dead one names what it points at — a broken link that will not
        // say to what is not something a person can act on.
        let dead = by_name("dead");
        assert!(dead.kind.is_broken_symlink());
        assert_eq!(
            dead.link_target.as_deref(),
            Some(std::path::Path::new("/nowhere/at/all"))
        );

        // A working link costs no `readlink` at all.
        let works = by_name("works");
        assert!(!works.kind.is_broken_symlink());
        assert_eq!(works.link_target, None);

        // And a plain file is untouched by any of this.
        assert_eq!(by_name("real.txt").link_target, None);

        fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferret-vfs-tree-{tag}-{}-{:?}",
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
    fn a_copied_tree_keeps_its_shape_at_depth() {
        // The parallel copy creates directories as the walk yields them, which
        // only works because a parent is always yielded before its children.
        let root = tempdir("deep-copy");
        let src = root.join("src");
        let mut here = src.clone();
        for level in 0..12 {
            here = here.join(format!("level-{level}"));
            fs::create_dir_all(&here).unwrap();
            for i in 0..5 {
                fs::write(here.join(format!("f{i}.txt")), format!("{level}:{i}")).unwrap();
            }
        }

        let dst = root.join("dst");
        smol::block_on(LocalFs::new().copy(&VfsPath::local(&src), &VfsPath::local(&dst))).unwrap();

        let mut check = dst.clone();
        for level in 0..12 {
            check = check.join(format!("level-{level}"));
            assert!(check.is_dir(), "{} must exist", check.display());
            for i in 0..5 {
                assert_eq!(
                    fs::read_to_string(check.join(format!("f{i}.txt"))).unwrap(),
                    format!("{level}:{i}")
                );
            }
        }

        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_tree_unlinks_symlinks_rather_than_following_them() {
        // The one that matters: a link inside the tree pointing *outside* it
        // must be unlinked, and whatever it points at must survive.
        let root = tempdir("delete-links");
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious.txt"), b"do not delete").unwrap();

        let doomed = root.join("doomed");
        fs::create_dir_all(doomed.join("nested")).unwrap();
        fs::write(doomed.join("nested/a.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, doomed.join("link-to-dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("precious.txt"),
            doomed.join("nested/link-to-file"),
        )
        .unwrap();

        smol::block_on(LocalFs::new().remove(&VfsPath::local(&doomed), RemoveMode::Permanent))
            .unwrap();

        assert!(!doomed.exists(), "the tree is gone");
        assert!(outside.is_dir(), "the link's target directory survives");
        assert_eq!(
            fs::read_to_string(outside.join("precious.txt")).unwrap(),
            "do not delete",
            "nothing outside the tree was followed and deleted"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn deleting_a_wide_directory_removes_every_child() {
        // The shape the parallel top level is for: many children, no depth.
        let root = tempdir("delete-wide");
        let doomed = root.join("doomed");
        fs::create_dir(&doomed).unwrap();
        for i in 0..500 {
            fs::write(doomed.join(format!("f{i:04}.txt")), b"x").unwrap();
        }
        fs::create_dir(doomed.join("sub")).unwrap();
        fs::write(doomed.join("sub/inner.txt"), b"x").unwrap();

        smol::block_on(LocalFs::new().remove(&VfsPath::local(&doomed), RemoveMode::Permanent))
            .unwrap();
        assert!(!doomed.exists());

        fs::remove_dir_all(&root).ok();
    }
}
