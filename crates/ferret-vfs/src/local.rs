use crate::config::{FIRST_LISTING_BATCH, LISTING_BATCH_SIZE, LISTING_BUFFER};
use crate::{
    Capabilities, DirEntryBatch, Entry, EntryKind, FileIdentity, ListingDetail, Metadata,
    ReadDirStream, RemoveMode, Result, SymlinkKind, SymlinkTarget, Vfs, VfsError, VfsPath,
    content_hash,
};
use async_trait::async_trait;
use futures::stream::StreamExt;
use rayon::prelude::*;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
use std::{ffi::CString, os::unix::ffi::OsStrExt, ptr};

/// The local disk backend, over `std::fs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFs;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
            create_symlinks: true,
            permissions: true,
            atomic_rename: true,
            rename_noreplace: cfg!(any(target_os = "macos", windows)),
            atomic_rename_noreplace: cfg!(any(target_os = "macos", windows)),
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
                .is_symlink()
                .then(|| fs::read_link(&local).ok())
                .flatten()
                .map(|target| Arc::from(target.to_string_lossy().into_owned()));

            let name = local.file_name().unwrap_or_default().to_string_lossy();
            Ok(Entry::new(owned.clone(), name, kind)
                .with_metadata(Some(to_metadata(&md, &owned)))
                .with_link_target(link_target))
        })
        .await
    }

    async fn read_link(&self, path: &VfsPath) -> Result<PathBuf> {
        let local = Self::require_local(path)?;
        let owned = path.clone();
        smol::unblock(move || {
            fs::read_link(&local)
                .map_err(|source| VfsError::from_io("read symbolic link", &owned, source))
        })
        .await
    }

    async fn create_symlink(&self, link: &VfsPath, target: &str, kind: SymlinkKind) -> Result<()> {
        let local = Self::require_local(link)?;
        if target.is_empty() || target.as_bytes().contains(&0) {
            return Err(VfsError::Io {
                action: "create symbolic link",
                path: link.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the symbolic-link target is empty or contains NUL",
                ),
            });
        }
        let owned = link.clone();
        let target = target.to_owned();
        smol::unblock(move || {
            let target = PathBuf::from(target);
            #[cfg(unix)]
            {
                let _ = kind;
                std::os::unix::fs::symlink(&target, &local)
                    .map_err(|source| symlink_error(&owned, source))
            }
            #[cfg(windows)]
            {
                match kind {
                    SymlinkKind::File => std::os::windows::fs::symlink_file(&target, &local)
                        .map_err(|source| symlink_error(&owned, source)),
                    SymlinkKind::Directory => std::os::windows::fs::symlink_dir(&target, &local)
                        .map_err(|source| symlink_error(&owned, source)),
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = (target, kind);
                Err(VfsError::Unsupported {
                    capability: "creating symbolic links",
                })
            }
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

    async fn rename_noreplace(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let source = Self::require_local(from)?;
        let destination = Self::require_local(to)?;
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = (source, destination);
            return Err(VfsError::Unsupported {
                capability: "rename without replacing",
            });
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            let from_path = from.clone();
            let to_path = to.clone();
            smol::unblock(move || {
                rename_noreplace_local(&source, &destination)
                    .map_err(|error| VfsError::from_rename_io(&from_path, &to_path, error))
            })
            .await
        }
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

fn symlink_error(path: &VfsPath, source: std::io::Error) -> VfsError {
    #[cfg(windows)]
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        return VfsError::SymlinkPermissionRequired { path: path.clone() };
    }
    VfsError::from_io("create symbolic link", path, source)
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

    let link_target = kind
        .is_symlink()
        .then(|| fs::read_link(local).ok())
        .flatten()
        .map(|target| Arc::from(target.to_string_lossy().into_owned()));

    Entry::new(path.clone(), name, kind)
        .with_metadata(metadata)
        .with_link_target(link_target)
}

fn to_metadata(md: &fs::Metadata, path: &VfsPath) -> Metadata {
    Metadata {
        len: md.len(),
        modified: md.modified().ok(),
        hidden: path.is_hidden_by_name() || platform_hidden(md),
        name_hidden: path.is_hidden_by_name(),
        platform_hidden: platform_hidden(md),
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
        let metadata = fs::symlink_metadata(src).map_err(|e| FileOpError::new("read", e))?;
        let mut source = fs::File::open(src).map_err(|e| FileOpError::new("read", e))?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut destination = options
            .open(dst)
            .map_err(|e| FileOpError::new("create", e))?;
        let result = std::io::copy(&mut source, &mut destination)
            .map_err(|e| FileOpError::new("copy", e))
            .and_then(|_| {
                destination
                    .set_permissions(metadata.permissions())
                    .map_err(|e| FileOpError::new("preserve metadata", e))
            });
        if let Err(error) = result {
            drop(destination);
            let _ = fs::remove_file(dst);
            return Err(error);
        }
        return Ok(());
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

#[cfg(unix)]
fn is_multiply_linked(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    md.nlink() > 1
}

#[cfg(target_os = "macos")]
fn has_extended_metadata(path: &std::path::Path) -> std::io::Result<bool> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("the path contains a NUL byte"))?;
    let size = unsafe { libc::listxattr(path.as_ptr(), ptr::null_mut(), 0, 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if size == 0 {
        return Ok(false);
    }
    let mut names = vec![0_u8; size as usize];
    let size = unsafe { libc::listxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut start = 0;
    while start < names.len() {
        let end = names[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map_or(names.len(), |offset| start + offset);
        if end > start && names[start..end] != b"com.apple.provenance"[..] {
            return Ok(true);
        }
        start = end + 1;
    }
    Ok(false)
}

#[cfg(windows)]
fn is_multiply_linked(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    md.number_of_links().is_some_and(|links| links > 1)
}

#[cfg(not(any(unix, windows)))]
fn is_multiply_linked(_md: &fs::Metadata) -> bool {
    false
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
    if is_multiply_linked(&current) {
        return Err(VfsError::Unsupported {
            capability: "editing multiply linked files",
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
    #[cfg(target_os = "macos")]
    if has_extended_metadata(local)
        .map_err(|source| VfsError::from_io("read metadata", owned, source))?
    {
        return Err(VfsError::Unsupported {
            capability: "editing files with extended metadata",
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
        ".ferret-save-{}-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
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
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(temp)
            .map_err(|source| VfsError::from_io("create", owned, source))?;
        file.write_all(contents)
            .map_err(|source| VfsError::from_io("write", owned, source))?;
        file.flush()
            .map_err(|source| VfsError::from_io("write", owned, source))?;
        // Mode before sync: a crash must not leave new bytes with old mode
        // unsynced in either order — syncing last covers both.
        preserve_mode(&file, current, owned)?;
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
fn preserve_mode(temp: &fs::File, current: &fs::Metadata, owned: &VfsPath) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = current.mode() & 0o7777;
    temp.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|source| VfsError::from_io("preserve metadata", owned, source))
}

#[cfg(not(unix))]
fn preserve_mode(_temp: &fs::File, _current: &fs::Metadata, _owned: &VfsPath) -> Result<()> {
    Ok(())
}

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

#[cfg(target_os = "macos")]
fn rename_noreplace_local(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the source contains NUL")
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the destination contains NUL",
        )
    })?;
    let renamed =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if renamed == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_noreplace_local(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = windows_extended_length(source);
    let destination = windows_extended_length(destination);
    let renamed = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if renamed == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_extended_length(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    let mut path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    for unit in &mut path {
        if *unit == 47 {
            *unit = 92;
        }
    }
    let is_device = path.starts_with(&[92, 92, 63, 92]) || path.starts_with(&[92, 92, 46, 92]);
    let mut extended = if is_device || path.len() < 240 {
        path
    } else if path.starts_with(&[92, 92]) {
        let mut result = vec![92, 92, 63, 92, 85, 78, 67, 92];
        result.extend_from_slice(&path[2..]);
        result
    } else if path.first() == Some(&92) {
        let mut result = vec![92, 92, 63, 92];
        result.extend_from_slice(&path[1..]);
        result
    } else if path.len() >= 3 && path[1] == 58 && path[2] == 92 {
        let mut result = vec![92, 92, 63, 92];
        result.extend_from_slice(&path);
        result
    } else {
        path
    };
    extended.push(0);
    extended
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
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    fn wide(path: &std::path::Path) -> Vec<u16> {
        windows_extended_length(path)
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
mod link_target_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tree_tests;
