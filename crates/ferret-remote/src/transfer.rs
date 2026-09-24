//! Moving bytes between local disk and a mount (R-6, F-22).
//!
//! The user-facing name is "Send to", and the reference product calls it SCP.
//! The transport here is SFTP on the connection that is already authenticated
//! (R-D05): OpenSSH 9.0 switched `scp` to SFTP itself, the old protocol is
//! deprecated, and a subprocess would authenticate a second time, report
//! progress at best per-file, and be unable to stop cleanly mid-file.
//!
//! Three properties are worth more than throughput:
//!
//! * **Nothing lands under its final name until it is complete.** Every write
//!   goes to `name.ferret-part` and is renamed into place at the end, so a
//!   cancelled or failed transfer leaves an obvious fragment rather than a
//!   truncated file that looks finished.
//! * **A server cannot write outside the destination.** Remote names are
//!   validated before they are ever joined onto a local path.
//! * **Cancellation is by drop**, like every other streaming operation in
//!   Ferret: the receiver goes away, the next progress send fails, and the
//!   transfer stops.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::try_join_all;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::config::{
    MAX_RESPONSE_BYTES, PART_SUFFIX, PLAN_CANCEL_POLL, TRANSFER_CHUNK, TRANSFER_WINDOW,
};
use crate::sftp::SftpVfs;
use crate::{RemoteError, Result};

/// Which way the bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Local disk to the server.
    Upload,
    /// The server to local disk.
    Download,
}

/// What to do when the destination already has something with that name.
///
/// Decided for the whole job before the first byte moves, rather than asked
/// per file halfway through a hundred-file copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Conflict {
    /// Leave the existing file alone and count it as skipped.
    #[default]
    Skip,
    /// Overwrite it.
    Replace,
}

/// What the UI draws while a job runs.
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// A file is starting. `index` is one-based over `total_files`.
    Started {
        name: String,
        index: usize,
        total_files: usize,
    },
    /// Bytes moved so far across the whole job.
    Progress { bytes_done: u64, bytes_total: u64 },
    /// One file failed. The job continues; the summary reports it.
    Failed { name: String, reason: String },
    /// Nothing was copied because something already had that name.
    Skipped { name: String },
    Finished {
        copied: usize,
        skipped: usize,
        failed: usize,
    },
}

/// A running transfer. Dropping it cancels the job.
///
/// The channel behind it stays private, so no third-party type crosses this
/// crate's boundary — the same rule `ferret-terminal` applies to its VT
/// engine (D-23).
pub struct Transfer {
    events: async_channel::Receiver<TransferEvent>,
}

impl Transfer {
    /// The next event, or `None` once the job has ended.
    pub async fn next(&self) -> Option<TransferEvent> {
        self.events.recv().await.ok()
    }
}

/// One file to move, with the size that makes the progress bar meaningful.
#[derive(Clone, Debug)]
struct PlannedFile {
    /// Path on local disk, either the source or the destination.
    local: PathBuf,
    /// Absolute path on the server, the other end of the same pair.
    remote: String,
    /// Shown in progress; the file's own name, not the whole path.
    name: String,
    bytes: u64,
}

static TRANSFER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn transfer_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        TRANSFER_COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

fn remote_part_path(remote: &str) -> String {
    format!("{remote}{PART_SUFFIX}-{}", transfer_suffix())
}

fn local_part_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    parent.join(format!(".{name}{PART_SUFFIX}-{}", transfer_suffix()))
}

impl SftpVfs {
    /// Start a transfer. Returns immediately; progress arrives on the channel.
    ///
    /// `sources` are local paths for an upload and remote paths for a
    /// download. `destination` is the directory the other side.
    pub fn transfer(
        self: Arc<Self>,
        direction: Direction,
        sources: Vec<TransferSource>,
        destination: TransferDestination,
        conflict: Conflict,
    ) -> Transfer {
        // Bounded, so a UI that stops reading parks the transfer instead of
        // letting progress events pile up without limit.
        let (sender, events) = async_channel::bounded(64);
        crate::detach(async move {
            let outcome = run(self, direction, sources, destination, conflict, &sender).await;
            if let Err(error) = outcome {
                let _ = sender
                    .send(TransferEvent::Failed {
                        name: String::new(),
                        reason: error.to_string(),
                    })
                    .await;
                let _ = sender
                    .send(TransferEvent::Finished {
                        copied: 0,
                        skipped: 0,
                        failed: 1,
                    })
                    .await;
            }
        });
        Transfer { events }
    }
}

/// One thing the user selected: a file, or a directory to copy whole.
#[derive(Clone, Debug)]
pub enum TransferSource {
    Local(PathBuf),
    Remote(String),
}

/// Where it is going.
#[derive(Clone, Debug)]
pub enum TransferDestination {
    LocalDir(PathBuf),
    RemoteDir(String),
}

async fn run(
    sftp: Arc<SftpVfs>,
    direction: Direction,
    sources: Vec<TransferSource>,
    destination: TransferDestination,
    conflict: Conflict,
    events: &async_channel::Sender<TransferEvent>,
) -> Result<()> {
    // The receiver going away is the cancellation signal for the whole job,
    // including the planning phase that runs before any event is sent.
    let cancelled = || events.is_closed();
    let planned = match direction {
        Direction::Upload => plan_upload(&sftp, &sources, &destination, &cancelled).await,
        Direction::Download => plan_download(&sftp, &sources, &destination, &cancelled).await,
    };
    let planned = match planned {
        Ok(planned) => planned,
        // A job cancelled while planning ends quietly: nobody is listening,
        // and there is nothing to report to.
        Err(RemoteError::Cancelled) => return Ok(()),
        Err(error) => return Err(error),
    };

    let total_files = planned.len();
    let bytes_total: u64 = planned.iter().map(|file| file.bytes).sum();
    let mut bytes_done = 0_u64;
    let (mut copied, mut skipped, mut failed) = (0, 0, 0);

    for (index, file) in planned.into_iter().enumerate() {
        if events
            .send(TransferEvent::Started {
                name: file.name.clone(),
                index: index + 1,
                total_files,
            })
            .await
            .is_err()
        {
            return Ok(()); // Cancelled: the receiver went away.
        }

        let existing = match direction {
            Direction::Upload => sftp.remote_exists(&file.remote).await,
            Direction::Download => file.local.exists(),
        };
        if existing && conflict == Conflict::Skip {
            skipped += 1;
            bytes_done += file.bytes;
            let _ = events
                .send(TransferEvent::Skipped {
                    name: file.name.clone(),
                })
                .await;
            continue;
        }

        let result = match direction {
            Direction::Upload => {
                upload_one(&sftp, &file, events, &mut bytes_done, bytes_total).await
            }
            Direction::Download => {
                download_one(&sftp, &file, events, &mut bytes_done, bytes_total).await
            }
        };
        match result {
            Ok(()) => {
                copied += 1;
                if direction == Direction::Upload {
                    // The destination directory has one more file in it than
                    // whatever was last listed.
                    sftp.invalidate_remote(&file.remote);
                }
            }
            // A cancelled job stops entirely rather than reporting every
            // remaining file as a failure.
            Err(RemoteError::Cancelled) => return Ok(()),
            Err(error) => {
                failed += 1;
                let _ = events
                    .send(TransferEvent::Failed {
                        name: file.name.clone(),
                        reason: error.to_string(),
                    })
                    .await;
            }
        }
    }

    let _ = events
        .send(TransferEvent::Finished {
            copied,
            skipped,
            failed,
        })
        .await;
    Ok(())
}

/// Expand the selection into files, creating the directories they need.
///
/// The local tree is walked with `jwalk` on Ferret's own pool, which is the
/// walker D-34 assigns to bulk file work like a tree copy — and never rayon's
/// implicit global pool, which would be a second one.
///
/// `cancelled` is checked as the walk proceeds. Planning happens before any
/// progress event exists, so without it a drop could not stop a walk over a
/// large selection, and the transfer would keep creating remote directories
/// for a job nobody is waiting on.
async fn plan_upload(
    sftp: &Arc<SftpVfs>,
    sources: &[TransferSource],
    destination: &TransferDestination,
    cancelled: &(impl Fn() -> bool + Send + Sync),
) -> Result<Vec<PlannedFile>> {
    let TransferDestination::RemoteDir(dest) = destination else {
        return Err(RemoteError::Unsupported {
            capability: "uploading to a local directory",
        });
    };
    crate::sftp::validate_path(dest).map_err(|message| RemoteError::Server {
        path: dest.clone(),
        message: message.into(),
    })?;
    let locals: Vec<PathBuf> = sources
        .iter()
        .filter_map(|source| match source {
            TransferSource::Local(path) => Some(path.clone()),
            TransferSource::Remote(_) => None,
        })
        .collect();
    if locals.len() != sources.len() {
        return Err(RemoteError::Unsupported {
            capability: "uploading remote sources as local files",
        });
    }

    let dest = dest.trim_end_matches('/').to_string();
    let stop = CancelFlag::default();
    let walk_stop = stop.clone();
    // The walk runs on a blocking thread and cannot poll the event channel
    // itself, so the flag is what carries the cancellation across to it.
    let mut walk = Box::pin(tokio::task::spawn_blocking(move || {
        expand_local(&locals, &dest, &walk_stop)
    }));
    let expanded = loop {
        tokio::select! {
            finished = &mut walk => {
                break finished.map_err(|_| RemoteError::Cancelled)??;
            }
            _ = tokio::time::sleep(PLAN_CANCEL_POLL) => {
                if cancelled() {
                    stop.set();
                }
            }
        }
    };
    if cancelled() {
        return Err(RemoteError::Cancelled);
    }

    for directory in &expanded.directories {
        if cancelled() {
            return Err(RemoteError::Cancelled);
        }
        let raw = sftp.raw_session();
        match raw.mkdir(directory.clone(), FileAttributes::empty()).await {
            Ok(_) => {}
            Err(error) => match raw.lstat(directory.clone()).await {
                Ok(attrs) if attrs.attrs.is_dir() => {}
                _ => return Err(server_error(directory, error)),
            },
        }
    }
    Ok(expanded.files)
}

async fn plan_download(
    sftp: &Arc<SftpVfs>,
    sources: &[TransferSource],
    destination: &TransferDestination,
    cancelled: &(impl Fn() -> bool + Send + Sync),
) -> Result<Vec<PlannedFile>> {
    let TransferDestination::LocalDir(dest) = destination else {
        return Err(RemoteError::Unsupported {
            capability: "downloading to a remote directory",
        });
    };
    let mut files = Vec::new();
    for source in sources {
        let TransferSource::Remote(path) = source else {
            return Err(RemoteError::Unsupported {
                capability: "downloading local sources as remote files",
            });
        };
        crate::sftp::validate_path(path).map_err(|message| RemoteError::Server {
            path: path.clone(),
            message: message.into(),
        })?;
        collect_remote(sftp, path, dest, &mut files, cancelled).await?;
    }
    Ok(files)
}

/// A stop flag the blocking walk can see.
#[derive(Clone, Default)]
struct CancelFlag(Arc<std::sync::atomic::AtomicBool>);

impl CancelFlag {
    fn set(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn is_set(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

struct ExpandedUpload {
    files: Vec<PlannedFile>,
    directories: Vec<String>,
}

fn expand_local(sources: &[PathBuf], dest: &str, stop: &CancelFlag) -> Result<ExpandedUpload> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    for source in sources {
        if stop.is_set() {
            return Err(RemoteError::Cancelled);
        }
        let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let target = crate::sftp::join_remote(dest, name);
        walk_local(source, &target, &mut files, &mut directories, stop)?;
    }
    Ok(ExpandedUpload { files, directories })
}

/// One selected item, expanded into the files and directories it implies.
///
/// `jwalk` on Ferret's shared pool (D-34): this is bulk file work with no
/// ignore semantics, which is exactly the job that walker owns, and
/// `pool::parallelism()` is what keeps it off rayon's implicit global pool.
///
/// Symlinks are not followed. Following one turns a link loop into an infinite
/// transfer and copies data the user never pointed at.
fn walk_local(
    source: &Path,
    target: &str,
    files: &mut Vec<PlannedFile>,
    directories: &mut Vec<String>,
    stop: &CancelFlag,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source).map_err(|error| RemoteError::Server {
        path: source.display().to_string(),
        message: error.to_string(),
    })?;

    if metadata.is_symlink() {
        return Ok(());
    }

    if !metadata.is_dir() {
        files.push(PlannedFile {
            name: source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            local: source.to_path_buf(),
            remote: target.to_string(),
            bytes: metadata.len(),
        });
        return Ok(());
    }

    directories.push(target.to_string());
    for entry in jwalk::WalkDir::new(source)
        .parallelism(ferret_vfs::pool::parallelism())
        .skip_hidden(false)
        .follow_links(false)
    {
        if stop.is_set() {
            return Err(RemoteError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        // The root of the walk is the directory itself, already recorded.
        let Ok(relative) = entry.path().strip_prefix(source).map(Path::to_path_buf) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        // A remote path is `/`-separated whatever this platform uses locally,
        // and a component that is not usable text has no name to send.
        let mut suffix = String::new();
        for component in relative.components() {
            let Some(part) = component.as_os_str().to_str() else {
                suffix.clear();
                break;
            };
            if !crate::sftp::valid_name(part) {
                suffix.clear();
                break;
            }
            suffix.push('/');
            suffix.push_str(part);
        }
        if suffix.is_empty() {
            continue;
        }
        let remote = format!("{target}{suffix}");

        if entry.path_is_symlink() {
            continue;
        }
        if entry.file_type().is_dir() {
            directories.push(remote);
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push(PlannedFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            local: entry.path(),
            remote,
            bytes,
        });
    }
    // Parents before children, so a file never lands before its directory
    // exists. `jwalk` yields depth-first but not in a guaranteed order across
    // threads, and sorting by depth is cheaper than reasoning about it.
    directories.sort_by_key(|path| path.matches('/').count());
    Ok(())
}

/// Walk a remote path into planned files, refusing any name that could escape
/// the destination directory.
fn collect_remote<'a>(
    sftp: &'a Arc<SftpVfs>,
    path: &'a str,
    dest: &'a Path,
    files: &'a mut Vec<PlannedFile>,
    cancelled: &'a (impl Fn() -> bool + Send + Sync),
) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        if cancelled() {
            return Err(RemoteError::Cancelled);
        }
        crate::sftp::validate_path(path).map_err(|message| RemoteError::Server {
            path: path.to_string(),
            message: message.into(),
        })?;
        let raw = sftp.raw_session();
        let attrs = raw
            .lstat(path.to_string())
            .await
            .map_err(|error| server_error(path, error))?;

        let name = path.rsplit('/').next().unwrap_or_default().to_string();
        // The check that makes a hostile server harmless: a name that is not
        // a single safe path component never becomes part of a local path.
        crate::sftp::safe_local_name(&name).map_err(|message| RemoteError::MalformedName {
            host: sftp.host().to_string(),
            name: format!("{name} ({message})"),
        })?;
        let local = dest.join(&name);

        if attrs.attrs.is_symlink() {
            return Ok(());
        }
        if attrs.attrs.is_dir() {
            std::fs::create_dir_all(&local).map_err(|error| RemoteError::Server {
                path: local.display().to_string(),
                message: error.to_string(),
            })?;
            let handle = raw
                .opendir(path.to_string())
                .await
                .map_err(|error| server_error(path, error))?
                .handle;
            let mut children = Vec::new();
            loop {
                match raw.readdir(handle.clone()).await {
                    Ok(response) => children.extend(
                        response
                            .files
                            .into_iter()
                            .map(|file| file.filename)
                            .filter(|name| name != "." && name != ".."),
                    ),
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        break;
                    }
                    Err(error) => {
                        let _ = raw.close(handle).await;
                        return Err(server_error(path, error));
                    }
                }
            }
            let _ = raw.close(handle).await;
            for child in children {
                if !crate::sftp::valid_name(&child) {
                    return Err(RemoteError::MalformedName {
                        host: sftp.host().to_string(),
                        name: child,
                    });
                }
                crate::sftp::safe_local_name(&child).map_err(|message| {
                    RemoteError::MalformedName {
                        host: sftp.host().to_string(),
                        name: format!("{child} ({message})"),
                    }
                })?;
                let child_path = crate::sftp::join_remote(path, &child);
                collect_remote(sftp, &child_path, &local, files, cancelled).await?;
            }
            return Ok(());
        }

        files.push(PlannedFile {
            name,
            local,
            remote: path.to_string(),
            bytes: attrs.attrs.size.unwrap_or(0),
        });
        Ok(())
    })
}

/// Copy one file to the server through a part file.
async fn upload_one(
    sftp: &Arc<SftpVfs>,
    file: &PlannedFile,
    events: &async_channel::Sender<TransferEvent>,
    bytes_done: &mut u64,
    bytes_total: u64,
) -> Result<()> {
    let raw = sftp.raw_session();
    let part = remote_part_path(&file.remote);

    let mut source =
        tokio::fs::File::open(&file.local)
            .await
            .map_err(|error| RemoteError::Server {
                path: file.local.display().to_string(),
                message: error.to_string(),
            })?;

    let handle = raw
        .open(
            part.clone(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|error| server_error(&part, error))?
        .handle;

    let mut offset = 0_u64;
    loop {
        // One window's worth of chunks is read, then written together: at
        // 80 ms round trip, sixteen sequential writes would cost sixteen
        // round trips and this costs one.
        let mut window = Vec::with_capacity(TRANSFER_WINDOW);
        for _ in 0..TRANSFER_WINDOW {
            let mut chunk = vec![0_u8; TRANSFER_CHUNK];
            let read = read_full(&mut source, &mut chunk).await?;
            if read == 0 {
                break;
            }
            chunk.truncate(read);
            window.push((offset, chunk));
            offset += read as u64;
        }
        if window.is_empty() {
            break;
        }

        let moved: u64 = window.iter().map(|(_, chunk)| chunk.len() as u64).sum();
        let writes = window.into_iter().map(|(at, chunk)| {
            let raw = raw.clone();
            let handle = handle.clone();
            async move { raw.write(handle, at, chunk).await }
        });
        if let Err(error) = try_join_all(writes).await {
            let _ = raw.close(handle).await;
            let _ = raw.remove(part).await;
            return Err(server_error(&file.remote, error));
        }

        *bytes_done += moved;
        if events
            .send(TransferEvent::Progress {
                bytes_done: *bytes_done,
                bytes_total,
            })
            .await
            .is_err()
        {
            // Cancelled mid-file: close and remove the fragment rather than
            // leaving a part file the user has to find and delete.
            let _ = raw.close(handle).await;
            let _ = raw.remove(part).await;
            return Err(RemoteError::Cancelled);
        }
    }
    let _ = raw.close(handle).await;

    // Preserve the modification time. Ownership is deliberately not attempted:
    // it usually needs privileges Ferret does not have, and failing the whole
    // transfer over it would be worse than a fresh timestamp.
    if let Ok(metadata) = std::fs::metadata(&file.local)
        && let Ok(modified) = metadata.modified()
        && let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        let seconds = since_epoch.as_secs().min(u32::MAX as u64) as u32;
        let _ = raw
            .setstat(
                part.clone(),
                FileAttributes {
                    atime: Some(seconds),
                    mtime: Some(seconds),
                    ..Default::default()
                },
            )
            .await;
    }

    let from = ferret_vfs::VfsPath::remote(sftp.mount_id(), part.clone());
    let to = ferret_vfs::VfsPath::remote(sftp.mount_id(), file.remote.clone());
    sftp.replace_remote(&from, &to)
        .await
        .map_err(|error| RemoteError::Server {
            path: file.remote.clone(),
            message: error.to_string(),
        })
}

/// Copy one file from the server through a part file.
async fn download_one(
    sftp: &Arc<SftpVfs>,
    file: &PlannedFile,
    events: &async_channel::Sender<TransferEvent>,
    bytes_done: &mut u64,
    bytes_total: u64,
) -> Result<()> {
    let raw = sftp.raw_session();
    let part = local_part_path(&file.local);

    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).map_err(|error| RemoteError::Server {
            path: parent.display().to_string(),
            message: error.to_string(),
        })?;
    }
    let mut sink = create_private_file(&part)
        .await
        .map_err(|error| RemoteError::Server {
            path: part.display().to_string(),
            message: error.to_string(),
        })?;

    let handle = raw
        .open(
            file.remote.clone(),
            OpenFlags::READ,
            FileAttributes::empty(),
        )
        .await
        .map_err(|error| server_error(&file.remote, error))?
        .handle;

    let mut offset = 0_u64;
    loop {
        // Every slot is requested at a fixed stride from `base`, which is only
        // where the next byte actually is while every preceding block came
        // back full length.
        let base = offset;
        let reads = (0..TRANSFER_WINDOW).map(|slot| {
            let raw = raw.clone();
            let handle = handle.clone();
            let at = base + (slot * TRANSFER_CHUNK) as u64;
            async move {
                match raw.read(handle, at, TRANSFER_CHUNK as u32).await {
                    Ok(data) => Ok(data.data),
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        Ok(Vec::new())
                    }
                    Err(error) => Err(error),
                }
            }
        });
        let chunks = match try_join_all(reads).await {
            Ok(chunks) => chunks,
            Err(error) => {
                let _ = raw.close(handle).await;
                let _ = std::fs::remove_file(&part);
                return Err(server_error(&file.remote, error));
            }
        };

        let mut moved = 0_u64;
        let mut hit_end = false;
        for chunk in chunks {
            if chunk.is_empty() {
                hit_end = true;
                break;
            }
            // A server may answer with fewer bytes than asked for at any
            // point, not only at the end of the file — the protocol allows it
            // and some implementations do it. Every later slot in this window
            // was requested at a stride that assumed this block was full, so
            // its data begins *after* a gap. Writing it here would drop the
            // bytes in between and silently corrupt the file, which the
            // part-file rename would then present as a completed download.
            //
            // So the short block is written, the rest of the window is thrown
            // away, and the next window starts from the true next byte. The
            // cost is one wasted round trip in a case that is rare; the
            // alternative is a download that is quietly wrong.
            let short = chunk.len() < TRANSFER_CHUNK;
            if chunk.len() > TRANSFER_CHUNK || chunk.len() > MAX_RESPONSE_BYTES {
                let _ = raw.close(handle).await;
                let _ = std::fs::remove_file(&part);
                return Err(RemoteError::Server {
                    path: file.remote.clone(),
                    message: "the server returned more bytes than were asked for".into(),
                });
            }
            moved += chunk.len() as u64;
            if let Err(error) = sink.write_all(&chunk).await {
                let _ = raw.close(handle).await;
                let _ = std::fs::remove_file(&part);
                return Err(RemoteError::Server {
                    path: part.display().to_string(),
                    message: error.to_string(),
                });
            }
            if short {
                break;
            }
        }
        offset = base + moved;
        *bytes_done += moved;

        if moved > 0
            && events
                .send(TransferEvent::Progress {
                    bytes_done: *bytes_done,
                    bytes_total,
                })
                .await
                .is_err()
        {
            let _ = raw.close(handle).await;
            let _ = std::fs::remove_file(&part);
            return Err(RemoteError::Cancelled);
        }
        if hit_end || moved == 0 {
            break;
        }
    }

    let _ = raw.close(handle).await;
    if let Err(error) = sink.flush().await {
        drop(sink);
        let _ = std::fs::remove_file(&part);
        return Err(RemoteError::Server {
            path: part.display().to_string(),
            message: error.to_string(),
        });
    }
    drop(sink);

    replace_file(&part, &file.local).map_err(|error| {
        let _ = std::fs::remove_file(&part);
        RemoteError::Server {
            path: file.local.display().to_string(),
            message: error.to_string(),
        }
    })
}

async fn create_private_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    options.open(path).await
}

/// Move a finished part file over its destination.
///
/// `std::fs::rename` replaces the destination on Unix and *fails* on Windows
/// when one exists, so a plain rename would report every completed
/// replace-download on Windows as a failure and then delete the part file,
/// leaving the stale destination in place. Both platforms are supported, so
/// both have to work.
///
/// The Windows path uses a backup rename and rolls it back if the final rename
/// fails; that is not atomic, but it does not discard the previous destination
/// on an ordinary rename failure.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        let backup = to.with_file_name(format!(
            ".{}ferret-replace-{}",
            to.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("download"),
            transfer_suffix(),
        ));
        let existing = match std::fs::symlink_metadata(to) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(std::io::Error::other("the destination is a directory"));
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if existing {
            std::fs::rename(to, &backup)?;
        }
        match std::fs::rename(from, to) {
            Ok(()) => {
                if existing {
                    let _ = std::fs::remove_file(&backup);
                }
                Ok(())
            }
            Err(error) => {
                if existing {
                    if let Err(rollback) = std::fs::rename(&backup, to) {
                        return Err(std::io::Error::other(format!(
                            "replacement failed: {error}; rollback failed: {rollback}"
                        )));
                    }
                }
                Err(error)
            }
        }
    }
}

/// Read until the buffer is full or the file ends.
///
/// `read` is allowed to return fewer bytes than asked for at any time, and a
/// short read that was treated as end-of-file would silently truncate an
/// upload — which the part-file rename would then make look successful.
async fn read_full(file: &mut tokio::fs::File, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .await
            .map_err(|error| RemoteError::Server {
                path: "the local file".into(),
                message: error.to_string(),
            })?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn server_error(path: &str, error: SftpError) -> RemoteError {
    let message = match &error {
        SftpError::Status(status) => status.error_message.clone(),
        _ => error.to_string(),
    };
    RemoteError::Server {
        path: path.to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_directory_expands_to_its_files_and_their_parents() {
        let root = std::env::temp_dir().join(format!("ferret-transfer-{}", std::process::id()));
        let nested = root.join("tree").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("tree").join("b.txt"), b"hi").unwrap();

        let expanded =
            expand_local(&[root.join("tree")], "/srv/dest", &CancelFlag::default()).unwrap();
        std::fs::remove_dir_all(&root).ok();

        let mut remotes: Vec<&str> = expanded.files.iter().map(|f| f.remote.as_str()).collect();
        remotes.sort_unstable();
        assert_eq!(
            remotes,
            ["/srv/dest/tree/b.txt", "/srv/dest/tree/inner/a.txt"]
        );
        // The parents have to be created before the files land in them.
        assert!(expanded.directories.contains(&"/srv/dest/tree".to_string()));
        assert!(
            expanded
                .directories
                .contains(&"/srv/dest/tree/inner".to_string())
        );
        // Sizes drive the progress bar, so they have to be real.
        let a = expanded
            .files
            .iter()
            .find(|f| f.name == "a.txt")
            .expect("a.txt");
        assert_eq!(a.bytes, 5);
    }

    #[test]
    fn a_symlink_is_not_followed_into() {
        let root = std::env::temp_dir().join(format!("ferret-transfer-l-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("real.txt"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let expanded =
            expand_local(&[root.join("link.txt")], "/srv", &CancelFlag::default()).unwrap();
        std::fs::remove_dir_all(&root).ok();

        // Following a link copies data the user never pointed at, and a loop
        // would never finish.
        #[cfg(unix)]
        assert!(expanded.files.is_empty(), "{:?}", expanded.files);
        #[cfg(not(unix))]
        let _ = expanded;
    }
}
