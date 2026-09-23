use crate::config::{FIRST_LISTING_BATCH, LISTING_BATCH_SIZE, LISTING_BUFFER};
use crate::{
    Capabilities, DirEntryBatch, Entry, EntryKind, FileIdentity, ListingDetail, Metadata,
    ReadDirStream, RemoveMode, Result, SymlinkTarget, Vfs, VfsError, VfsPath, content_hash,
};
use async_trait::async_trait;
use futures::stream::StreamExt;
use rayon::prelude::*;
use std::fs;
use std::io::{Read, Write};
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
            // Same-directory temp plus rename: atomic replacement with the
            // identity check `write_atomic` performs. A future remote backend
            // answers false here unless it can offer the same conflict checks —
            // a generic writable or rename capability is insufficient.
            atomic_write: true,
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
            // `UF_HIDDEN` lives in `st_flags` on Unix, so it costs the same
            // `stat` as size and dates; `FindFirstFileEx` hands the Windows
            // attributes over with the name.
            hidden_in_listing: cfg!(windows),
        }
    }

    fn read_dir(&self, dir: &VfsPath, detail: ListingDetail) -> ReadDirStream {
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
                                    .send_blocking(Ok(resolve(&mut pending, batch_size, detail)))
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
                        .send_blocking(Ok(resolve(&mut pending, batch_size, detail)))
                        .is_err()
                    {
                        return; // consumer went away: stop walking.
                    }
                    batch_size = (batch_size * 2).min(LISTING_BATCH_SIZE);
                }

                if !pending.is_empty() {
                    let _ = tx.send_blocking(Ok(resolve(&mut pending, batch_size, detail)));
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

    async fn write_atomic(
        &self,
        path: &VfsPath,
        contents: &[u8],
        expect: FileIdentity,
    ) -> Result<FileIdentity> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        let contents = contents.to_vec();
        smol::unblock(move || write_atomic_blocking(&local, &owned, &contents, expect)).await
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
fn resolve(
    entries: &mut Vec<fs::DirEntry>,
    next_capacity: usize,
    detail: ListingDetail,
) -> DirEntryBatch {
    let batch = std::mem::take(entries);
    entries.reserve(next_capacity);
    crate::pool::install(move || {
        batch
            .into_par_iter()
            .map(|entry| convert(entry, detail))
            .collect()
    })
}

fn convert(entry: fs::DirEntry, detail: ListingDetail) -> Entry {
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

    // On both platforms when asked for (D-33). On Windows this is served from
    // what `FindFirstFileEx` already returned; on Unix it is a `stat`, running
    // here on a pool thread rather than on the file pane's second pass — and
    // it is the one call a `NamesOnly` listing exists to skip.
    let metadata = detail
        .wants_metadata()
        .then(|| entry.metadata().ok().map(|md| to_metadata(&md, &path)))
        .flatten();

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

/// Replace one file's contents without ever exposing a half-written file.
///
/// All blocking, for `smol::unblock` only. The identity check (length, mtime,
/// content hash) runs first so an external change — including one that kept
/// the size and the mtime — refuses with [`VfsError::Conflict`] instead of
/// destroying someone's edit. Then the new bytes go to a temp file in the
/// *same directory* (a different filesystem would make the final step a copy)
/// and the temp file is moved over the target.
///
/// Symlinks are refused outright: v1 editing never follows a link, so a link
/// to a 4 GB file cannot be swapped for a small save, and a link whose target
/// moved cannot be "saved" into a new regular file.
fn write_atomic_blocking(
    local: &std::path::Path,
    owned: &VfsPath,
    contents: &[u8],
    expect: FileIdentity,
) -> Result<FileIdentity> {
    if fs::symlink_metadata(local)
        .map_err(|source| VfsError::from_io("read", owned, source))?
        .is_symlink()
    {
        return Err(VfsError::Unsupported {
            capability: "editing linked files",
        });
    }
    // Length and mtime first: cheap, and the common conflict shape.
    let current = fs::metadata(local).map_err(|source| VfsError::from_io("read", owned, source))?;
    // Saving over a directory (or anything non-file) is a programmer error
    // through the UI gate — refuse it with words, not an `open`/`read` Io.
    if !current.is_file() {
        return Err(VfsError::NotADirectory {
            path: owned.clone(),
        });
    }
    // A read-only file must refuse, not silently replace: on Unix a rename
    // over a 0444 file succeeds when the directory is writable, while on
    // Windows `ReplaceFileW` fails — same action, opposite outcomes. Refusing
    // up front makes both platforms say the same thing.
    if current.permissions().readonly() {
        return Err(VfsError::PermissionDenied {
            path: owned.clone(),
        });
    }
    let current_len = current.len();
    let (current_secs, current_nanos) = unix_secs_nanos(current.modified().ok());
    if current_len != expect.len
        || current_secs != expect.mtime_secs
        || current_nanos != expect.mtime_nanos
    {
        return Err(VfsError::Conflict {
            path: owned.clone(),
        });
    }
    // Then the bytes themselves: bounded by the length just observed, plus
    // one byte to notice a growth that raced the stat.
    let mut current_bytes = Vec::new();
    fs::File::open(local)
        .map_err(|source| VfsError::from_io("open", owned, source))?
        .take(current_len.saturating_add(1))
        .read_to_end(&mut current_bytes)
        .map_err(|source| VfsError::from_io("read", owned, source))?;
    if current_bytes.len() as u64 != expect.len
        || content_hash(&current_bytes) != expect.content_hash
    {
        return Err(VfsError::Conflict {
            path: owned.clone(),
        });
    }

    let dir = local.parent().ok_or(VfsError::Unsupported {
        capability: "editing files without a parent directory",
    })?;
    let temp = dir.join(format!(
        ".ferret-save-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0),
    ));
    let cleanup = TempCleanup {
        path: Some(temp.clone()),
    };
    // Re-verify the symlink check immediately before the rename: a swap
    // between the check above and the use here would otherwise redirect the
    // read-check onto a link target while the rename replaces the link.
    // The window is narrow (a racy restore tool, not a remote attacker), but
    // the second stat is one syscall.
    if fs::symlink_metadata(local)
        .map_err(|source| VfsError::from_io("read", owned, source))?
        .is_symlink()
    {
        return Err(VfsError::Unsupported {
            capability: "editing linked files",
        });
    }
    let result = write_temp_and_replace(local, owned, &temp, contents, &current);
    // A crash mid-write must leave the original intact *and* no litter: the
    // temp file goes on every path where the rename did not consume it.
    if result.is_err() {
        drop(cleanup);
    } else {
        std::mem::forget(cleanup);
    }
    result
}

/// A temp file that deletes itself unless disarmed.
///
/// The rename consumes the temp on success; every other exit — conflict,
/// full disk, permission — must not leave `.ferret-save-*.tmp` litter behind
/// the next listing.
struct TempCleanup {
    path: Option<PathBuf>,
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn write_temp_and_replace(
    local: &std::path::Path,
    owned: &VfsPath,
    temp: &std::path::Path,
    contents: &[u8],
    current: &fs::Metadata,
) -> Result<FileIdentity> {
    {
        let mut file = fs::File::create_new(temp)
            .map_err(|source| VfsError::from_io("create", owned, source))?;
        file.write_all(contents)
            .map_err(|source| VfsError::from_io("write", owned, source))?;
        file.flush()
            .map_err(|source| VfsError::from_io("write", owned, source))?;
        // Mode before sync: a crash must not leave new bytes with old mode
        // unsynced in either order — syncing last covers both.
        preserve_mode(&file, current);
        #[cfg(unix)]
        file.sync_all()
            .map_err(|source| VfsError::from_io("write", owned, source))?;
        // Windows durability, not just Rust-buffer flushing: without
        // `FlushFileBuffers` a power loss after "saved" can lose the file on
        // Windows while surviving on Unix. Best-effort like the dir sync —
        // the rename already committed, so retrying is worse than reporting.
        #[cfg(windows)]
        flush_file_buffers(&file, owned)?;
    }
    replace_file(temp, local, owned)?;
    // Crash durability for the rename itself, where the platform offers it.
    // Best-effort by design: the rename already committed, so a failure here
    // is reported by syncing nothing — retrying a replacement that may have
    // committed is how a file gets written twice.
    #[cfg(unix)]
    let _ = fs::File::open(local.parent().unwrap_or(local)).and_then(|dir| dir.sync_all());
    let fresh = fs::metadata(local).map_err(|source| VfsError::from_io("read", owned, source))?;
    Ok(FileIdentity::new(
        fresh.len(),
        fresh.modified().ok(),
        contents,
    ))
}

/// Carry the destination's permission bits onto the temp file before the
/// rename, so a save never widens or narrows who can read the file.
///
/// Ownership follows for free: the temp file is created in the same directory
/// by the same user. macOS extended attributes and Windows ACLs beyond the
/// readonly bit are *not* carried by this path — `ReplaceFileW` preserves the
/// destination's ACLs on Windows, while Unix callers with xattrs beyond mode
/// bits should treat that as a known v1 limitation, not as silent fidelity.
///
/// On Windows the temp itself is created with inherited (default) DACLs and
/// holds the full new bytes until the replace: a file locked down to one user
/// sits briefly readable by whoever inherits the directory's DACLs. The
/// committed file keeps the destination's ACLs; the window is the temp's
/// lifetime only. Creating the temp with a current-user-only DACL first is
/// the hardening follow-up.
#[cfg(unix)]
fn preserve_mode(temp: &fs::File, current: &fs::Metadata) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = current.mode() & 0o7777;
    let _ = temp.set_permissions(fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn preserve_mode(_temp: &fs::File, _current: &fs::Metadata) {}

/// Flush OS buffers for a Windows handle: `File::sync_all` does not exist
/// there, and `flush()` only empties Rust's buffers into the OS.
#[cfg(windows)]
fn flush_file_buffers(file: &fs::File, owned: &VfsPath) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

    // SAFETY: the handle is a live open file for this call; the return is
    // checked and a failure becomes a typed save error, never a silent loss.
    let ok = unsafe { FlushFileBuffers(file.as_raw_handle() as _) };
    if ok == 0 {
        return Err(VfsError::from_io(
            "write",
            owned,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Move the flushed temp file over the destination.
///
/// Unix `rename` is atomic on one filesystem, which the same-directory temp
/// guarantees. Windows goes through `ReplaceFileW`: unlike delete-then-move
/// it preserves the destination's ACLs and attributes, and it never leaves a
/// window where the path names nothing.
fn replace_file(temp: &std::path::Path, dest: &std::path::Path, owned: &VfsPath) -> Result<()> {
    #[cfg(windows)]
    {
        replace_file_windows(temp, dest, owned)
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, dest).map_err(|source| VfsError::from_io("save", owned, source))
    }
}

/// Verified `ReplaceFileW` semantics: never delete the destination first.
///
/// Alternate data streams ride the destination, not the temp: the temp holds
/// only the default stream, so `dest:Zone.Identifier` (the download-mark of
/// the web) and custom ADSs are silently discarded by a save. Low blast
/// radius for code files, real for downloaded ones — a known v1 limitation
/// alongside the xattr note above, not silent fidelity.
///
/// Long paths go extended-length: `std::fs` operations succeed past
/// `MAX_PATH` while a verbatim `ReplaceFileW` fails, so over-long absolute
/// paths are prefixed with `\\?\` (and `\\?\UNC\` for UNC) before widening.
#[cfg(windows)]
fn replace_file_windows(
    temp: &std::path::Path,
    dest: &std::path::Path,
    owned: &VfsPath,
) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    fn wide(path: &std::path::Path) -> Vec<u16> {
        let extended = extended_length(path);
        extended.as_os_str().encode_wide().chain([0]).collect()
    }

    /// Prefix over-long absolute paths for the extended-length API.
    fn extended_length(path: &std::path::Path) -> std::path::PathBuf {
        use std::path::{Component, Prefix};
        let text = path.as_os_str().to_string_lossy();
        if text.len() < 240 || text.starts_with(r"\\?\") {
            return path.to_path_buf();
        }
        let mut out = String::with_capacity(text.len() + 8);
        // UNC (`\\host\share`) and device (`\\.\`) paths need their own
        // prefix; anything else absolute takes the plain one.
        let mut components = path.components();
        match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::UNC(host, share) => {
                    out.push_str(r"\\?\UNC\");
                    out.push_str(&host.to_string_lossy());
                    out.push('\\');
                    out.push_str(&share.to_string_lossy());
                    for rest in components {
                        out.push('\\');
                        out.push_str(&rest.as_os_str().to_string_lossy());
                    }
                    return std::path::PathBuf::from(out);
                }
                _ => return path.to_path_buf(),
            },
            Some(Component::RootDir) => {
                // `C:\…` absolute: strip the leading `\` handling below by
                // rebuilding from components under the prefix.
                out.push_str(r"\\?\");
                out.push_str(&text.trim_start_matches('\\'));
                return std::path::PathBuf::from(out);
            }
            _ => {}
        }
        // Relative or oddly-prefixed: leave alone rather than corrupt.
        path.to_path_buf()
    }

    let temp_wide = wide(temp);
    let dest_wide = wide(dest);
    // SAFETY: both pointers are valid NUL-terminated UTF-16 for the call, no
    // backup is requested, and no flags change the default replace behavior.
    let ok = unsafe {
        ReplaceFileW(
            dest_wide.as_ptr(),
            temp_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(VfsError::from_io(
            "save",
            owned,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn unix_secs_nanos(modified: Option<std::time::SystemTime>) -> (u64, u32) {
    crate::vfs::mtime_parts(modified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FIRST_LISTING_BATCH;
    use std::collections::BTreeMap;

    fn collect(fs: &LocalFs, dir: &VfsPath) -> BTreeMap<String, Entry> {
        collect_with(fs, dir, ListingDetail::Full)
    }

    fn collect_with(fs: &LocalFs, dir: &VfsPath, detail: ListingDetail) -> BTreeMap<String, Entry> {
        smol::block_on(async {
            let mut stream = fs.read_dir(dir, detail);
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
        let results: Vec<_> = smol::block_on(async {
            LocalFs::new()
                .read_dir(&missing, ListingDetail::Full)
                .collect()
                .await
        });
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
            let mut stream =
                LocalFs::new().read_dir(&VfsPath::local(root.clone()), ListingDetail::Full);
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
                .read_dir(&VfsPath::local(root.clone()), ListingDetail::Full)
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
    fn names_only_skips_the_stat_that_fills_metadata() {
        let root = tempdir("detail");
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();

        let fs_ = LocalFs::new();
        let dir = VfsPath::local(root.clone());

        // Full is the contract every column in the file pane is written
        // against: the row is complete when it appears (D-33).
        let full = collect_with(&fs_, &dir, ListingDetail::Full);
        assert_eq!(full["a.txt"].metadata.map(|m| m.len), Some(5));
        assert!(full["sub"].metadata.is_some());

        // NamesOnly still classifies every entry — the chevron, the icon and
        // the sort by name all come from the directory read itself — but the
        // per-entry `stat` is gone.
        let names = collect_with(&fs_, &dir, ListingDetail::NamesOnly);
        assert_eq!(names.len(), 2, "the same entries are listed");
        assert_eq!(names["a.txt"].kind, EntryKind::File);
        assert_eq!(names["sub"].kind, EntryKind::Dir);
        assert!(names["sub"].is_dir_like());
        assert!(
            names["a.txt"].metadata.is_none(),
            "NamesOnly must not pay for size and dates"
        );
        assert_eq!(
            names["a.txt"].display_len(),
            None,
            "and a row must report no size rather than a wrong one"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn names_only_still_hides_dotfiles_by_name() {
        // The platform's own hidden flag rides the `stat` NamesOnly skips, so
        // on Unix only the name-based half survives. That is exactly why
        // `ListingProfile::detail` refuses NamesOnly while hidden entries are
        // being filtered — see `hidden_in_listing`.
        let root = tempdir("detail-hidden");
        fs::write(root.join(".secret"), b"").unwrap();
        fs::write(root.join("plain"), b"").unwrap();

        let names = collect_with(
            &LocalFs::new(),
            &VfsPath::local(root.clone()),
            ListingDetail::NamesOnly,
        );
        assert!(names[".secret"].is_hidden());
        assert!(!names["plain"].is_hidden());

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
                .read_dir(&VfsPath::local(root.clone()), ListingDetail::Full)
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

    fn identity_of(path: &VfsPath, bytes: &[u8]) -> FileIdentity {
        let md = fs::metadata(path.as_local().unwrap()).unwrap();
        FileIdentity::new(md.len(), md.modified().ok(), bytes)
    }

    #[test]
    fn an_unedited_save_round_trips_byte_identically() {
        // Opened and saved without being edited: the bytes must not move at
        // all, not even a newline or a mode bit.
        let root = tempdir("atomic-noop");
        let file = root.join("config.conf");
        fs::write(&file, b"key = value\nsecond = 2").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let path = VfsPath::local(&file);
        let before = fs::read(&file).unwrap();
        let expect = identity_of(&path, &before);

        let after = smol::block_on(LocalFs::new().write_atomic(&path, &before, expect)).unwrap();
        assert_eq!(fs::read(&file).unwrap(), before, "bytes untouched");
        assert_eq!(after.len, before.len() as u64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&file).unwrap().mode() & 0o777,
                0o640,
                "permission bits preserved"
            );
        }

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn crlf_without_trailing_newline_survives_a_save() {
        // The fidelity the editor promises: line endings and the missing
        // final newline are the caller's bytes to keep, and the atomic layer
        // must not touch them.
        let root = tempdir("atomic-crlf");
        let file = root.join("win.ini");
        let before = b"[section]\r\nkey=value";
        fs::write(&file, before).unwrap();
        let path = VfsPath::local(&file);
        let expect = identity_of(&path, before);

        let changed = b"[section]\r\nkey=other";
        smol::block_on(LocalFs::new().write_atomic(&path, changed, expect)).unwrap();
        assert_eq!(fs::read(&file).unwrap(), changed);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_bom_is_opaque_bytes_to_the_atomic_layer() {
        let root = tempdir("atomic-bom");
        let file = root.join("utf16.txt");
        let mut before = vec![0xFF, 0xFE];
        before.extend_from_slice(&[b'h', 0, b'i', 0]);
        fs::write(&file, &before).unwrap();
        let path = VfsPath::local(&file);
        let expect = identity_of(&path, &before);

        let mut changed = before.clone();
        changed.extend_from_slice(&[b'!', 0]);
        smol::block_on(LocalFs::new().write_atomic(&path, &changed, expect)).unwrap();
        assert_eq!(fs::read(&file).unwrap(), changed);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_external_edit_refuses_rather_than_winning() {
        let root = tempdir("atomic-conflict");
        let file = root.join("shared.txt");
        fs::write(&file, b"version one").unwrap();
        let path = VfsPath::local(&file);
        let stale = identity_of(&path, b"version one");

        // Someone else saves first: longer, so the length check fires.
        fs::write(&file, b"version two, longer").unwrap();
        let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", stale))
            .expect_err("a changed file must refuse");
        assert!(
            matches!(err, VfsError::Conflict { .. }),
            "refusal, not resolution: {err:?}"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "version two, longer",
            "the refused save changed nothing"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_same_size_same_mtime_edit_still_conflicts() {
        // The case a stat alone cannot see: identical length and identical
        // mtime, different bytes. Only the content hash catches it, so forge
        // an identity with the current stat but a stale hash.
        let root = tempdir("atomic-sneaky");
        let file = root.join("sneaky.txt");
        fs::write(&file, b"aaaaaaaa").unwrap();
        let path = VfsPath::local(&file);

        // An external edit that keeps the size; the forged identity carries
        // the *new* stat (as if the clock never moved) with the *old* hash.
        fs::write(&file, b"bbbbbbbb").unwrap();
        let current = fs::read(&file).unwrap();
        let md = fs::metadata(&file).unwrap();
        let mut stale = FileIdentity::new(md.len(), md.modified().ok(), &current);
        stale.content_hash = content_hash(b"aaaaaaaa");

        let err = smol::block_on(LocalFs::new().write_atomic(&path, b"cccccccc", stale))
            .expect_err("same-size edits must conflict");
        assert!(matches!(err, VfsError::Conflict { .. }), "{err:?}");
        assert_eq!(fs::read(&file).unwrap(), b"bbbbbbbb");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn conflict_messages_name_the_file() {
        let err = VfsError::Conflict {
            path: VfsPath::local("/docs/notes.md"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/docs/notes.md"), "{msg}");
        assert!(msg.contains("changed"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_not_followed() {
        let root = tempdir("atomic-link");
        let target = root.join("real.txt");
        fs::write(&target, b"real").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let path = VfsPath::local(&link);
        let expect = FileIdentity::new(4, None, b"real");

        let err = smol::block_on(LocalFs::new().write_atomic(&path, b"swapped", expect))
            .expect_err("a symlink must never be saved through");
        assert!(matches!(err, VfsError::Unsupported { .. }), "{err:?}");
        assert_eq!(fs::read(&target).unwrap(), b"real");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_save_leaves_no_litter() {
        let root = tempdir("atomic-litter");
        let file = root.join("note.txt");
        fs::write(&file, b"original").unwrap();
        let path = VfsPath::local(&file);
        let stale = FileIdentity::new(999, None, b"wrong");

        let _ = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", stale));
        let litter: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ferret-save-")
            })
            .collect();
        assert!(litter.is_empty(), "temp files must not survive: {litter:?}");
        assert_eq!(fs::read(&file).unwrap(), b"original");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_successful_save_leaves_no_litter_either() {
        let root = tempdir("atomic-litter-ok");
        let file = root.join("note.txt");
        fs::write(&file, b"original").unwrap();
        let path = VfsPath::local(&file);
        let expect = identity_of(&path, b"original");

        smol::block_on(LocalFs::new().write_atomic(&path, b"updated", expect))
            .expect("a clean save");
        let litter: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ferret-save-")
            })
            .collect();
        // On Windows the replacement consumes the temp by contract; on Unix
        // the rename does. Either way nothing may survive beside the file.
        assert!(litter.is_empty(), "temp files must not survive: {litter:?}");
        assert_eq!(fs::read(&file).unwrap(), b"updated");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn saving_a_directory_is_a_named_refusal() {
        let root = tempdir("atomic-is-dir");
        let path = VfsPath::local(&root);
        let expect = FileIdentity::new(0, None, b"");

        let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", expect))
            .expect_err("a directory is not a file");
        assert!(
            matches!(err, VfsError::NotADirectory { .. }),
            "words, not an `open` Io: {err:?}"
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn saving_a_read_only_file_refuses_on_every_platform() {
        let root = tempdir("atomic-readonly");
        let file = root.join("locked.txt");
        fs::write(&file, b"original").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let path = VfsPath::local(&file);
        let expect = identity_of(&path, b"original");

        let err = smol::block_on(LocalFs::new().write_atomic(&path, b"mine", expect))
            .expect_err("a read-only file must refuse");
        assert!(
            matches!(err, VfsError::PermissionDenied { .. }),
            "same answer on Unix and Windows: {err:?}"
        );
        assert_eq!(fs::read(&file).unwrap(), b"original");

        // Test cleanup only: restore owner writability without the
        // world-writable side effect `set_readonly(false)` has on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o644));
        }
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(&file).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&file, permissions).unwrap();
        }
        fs::remove_dir_all(&root).ok();
    }
}
