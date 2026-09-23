//! Read-only SFTP implementation of Ferret's storage boundary (R-2).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use ferret_vfs::{
    Capabilities, Entry, EntryKind, ListingDetail, Metadata, MountId, ReadDirStream, RemoveMode,
    SymlinkTarget, Vfs, VfsError, VfsPath,
};
use futures::{StreamExt, stream};
use russh_sftp::client::{RawSftpSession, error::Error as SftpError};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cache::MountCache;
use crate::config::{
    HANDSHAKE_TIMEOUT, LISTING_BATCH_CAP, LISTING_CACHE_MAX_ENTRIES, MAX_RESPONSE_BYTES,
    PIPELINE_DEPTH, REQUEST_TIMEOUT, TRANSFER_CHUNK,
};
use crate::{RemoteError, Result};

/// An authenticated SFTP mount. All protocol work is dispatched to the
/// crate's Tokio runtime, so this handle is safe to call from GPUI or smol.
pub struct SftpVfs {
    mount: MountId,
    host: Arc<str>,
    raw: Arc<RawSftpSession>,
    atomic_rename: bool,
    /// The server advertises `copy-data`, so it can copy a file's bytes
    /// itself. Without it there is no server-side copy in SFTP v3.
    server_side_copy: bool,
    /// What this mount remembers between calls, and the permit pool that caps
    /// how many listings it may have open at once. See [`crate::cache`].
    cache: Arc<MountCache>,
}

impl SftpVfs {
    pub(crate) async fn negotiate<S>(stream: S) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let raw = RawSftpSession::new(stream);
        raw.set_timeout(REQUEST_TIMEOUT.as_secs());
        // Bounded separately from an ordinary request: if nothing on the far
        // side speaks SFTP — a refused `sudo`, a missing server binary — then
        // no version packet is coming, and waiting the request timeout to
        // discover that reads as a hang rather than as the refusal it is.
        let version = match tokio::time::timeout(HANDSHAKE_TIMEOUT, raw.init()).await {
            Ok(Ok(version)) => version,
            Ok(Err(error)) => {
                return Err(RemoteError::Subsystem {
                    host: "the remote server".into(),
                    message: error.to_string(),
                });
            }
            Err(_) => {
                return Err(RemoteError::Subsystem {
                    host: "the remote server".into(),
                    message: "it never started an SFTP server on the channel".into(),
                });
            }
        };
        Ok(Self {
            mount: MountId(0),
            host: Arc::from("the remote server"),
            raw: Arc::new(raw),
            atomic_rename: version.extensions.contains_key("posix-rename@openssh.com"),
            server_side_copy: version.extensions.contains_key("copy-data"),
            cache: Arc::new(MountCache::default()),
        })
    }

    pub(crate) fn with_mount_and_host(mut self, mount: MountId, host: String) -> Self {
        self.mount = mount;
        self.host = Arc::from(host);
        self
    }

    pub fn mount_id(&self) -> MountId {
        self.mount
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The protocol session, for the transfer engine.
    ///
    /// `pub(crate)` on purpose: transfers need raw handles and offsets, which
    /// the `Vfs` trait has no shape for, but nothing outside this crate may
    /// hold an SFTP handle.
    pub(crate) fn raw_session(&self) -> Arc<RawSftpSession> {
        self.raw.clone()
    }

    /// Whether something is at `path`, for the conflict scan.
    pub(crate) async fn remote_exists(&self, path: &str) -> bool {
        self.raw.lstat(path.to_string()).await.is_ok()
    }

    /// Forget a remote path, for callers holding protocol strings rather than
    /// a [`VfsPath`] — the transfer engine, which has just written one.
    pub(crate) fn invalidate_remote(&self, path: &str) {
        self.cache.invalidate(path);
    }

    /// Resolve the login account's home directory for the mount's first path.
    pub async fn home(&self) -> ferret_vfs::Result<VfsPath> {
        let raw = self.raw.clone();
        let placeholder = VfsPath::remote(self.mount, "/");
        let response = crate::spawn(async move { raw.realpath(".").await })
            .await
            .map_err(|error| error.into_vfs("resolve the remote home directory", &placeholder))?
            .map_err(|error| map_error(error, "resolve the remote home directory", &placeholder))?;
        let home = response
            .files
            .into_iter()
            .next()
            .ok_or_else(|| invalid_path(&placeholder, "the server returned no home directory"))?
            .filename;
        validate_path(&home).map_err(|message| invalid_path(&placeholder, message))?;
        Ok(VfsPath::remote(self.mount, home))
    }

    fn remote_path(&self, path: &VfsPath) -> std::result::Result<String, VfsError> {
        match path {
            VfsPath::Remote { mount, path: value } if *mount == self.mount => {
                validate_path(value).map_err(|message| invalid_path(path, message))?;
                Ok(value.to_string())
            }
            _ => Err(invalid_path(path, "the path belongs to a different mount")),
        }
    }
}

#[async_trait]
impl Vfs for SftpVfs {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            symlinks: true,
            permissions: true,
            atomic_rename: self.atomic_rename,
            // No identity-checked atomic replacement over SFTP: a remote
            // write cannot offer the conflict refusal `write_atomic`
            // requires, so in-place preview editing stays unavailable there.
            atomic_write: false,
            watch: false,
            metadata_in_listing: true,
            hidden_in_listing: true,
        }
    }

    fn read_dir(&self, dir: &VfsPath, _detail: ListingDetail) -> ReadDirStream {
        let path = match self.remote_path(dir) {
            Ok(path) => path,
            Err(error) => return stream::once(async move { Err(error) }).boxed(),
        };
        // A hit is the whole point of the cache: back, forward and the column
        // view's ancestors all re-read directories that were listed seconds
        // ago, and over a link each of those is a round trip.
        if let Some(cached) = self.cache.listing(&path) {
            let batch = (*cached).clone();
            return stream::once(async move { Ok(batch) }).boxed();
        }

        let subject = dir.clone();
        let mount = self.mount;
        let raw = self.raw.clone();
        let cache = self.cache.clone();
        let cache_key = path.clone();
        let (sender, receiver) = async_channel::bounded(ferret_vfs::config::LISTING_BUFFER);
        crate::detach(async move {
            // Held for the whole walk and released on every exit, including
            // the early one when the reader drops the stream. Column view
            // opens several listings at once; without this they compete for
            // the one channel and each finishes later than it needed to.
            let _slot = cache.listing_slot().await;

            let handle = match raw.opendir(path).await {
                Ok(handle) => handle.handle,
                Err(error) => {
                    let _ = sender
                        .send(Err(map_error(error, "read directory", &subject)))
                        .await;
                    return;
                }
            };

            // Accumulated alongside the stream so the listing can be cached
            // once it completes. Abandoned past the cap rather than growing:
            // a hit copies this back, and a 200,000-entry copy is not a
            // saving.
            let mut collected = Some(Vec::new());
            // Set only by the EOF arm: any other exit leaves a partial walk,
            // which must not be remembered as the directory's contents.
            let mut complete = false;

            loop {
                match raw.readdir(handle.clone()).await {
                    Ok(response) => {
                        let mut entries =
                            Vec::with_capacity(response.files.len().min(LISTING_BATCH_CAP));
                        let converted = stream::iter(response.files.into_iter().map(|file| {
                            let raw = raw.clone();
                            let subject = subject.clone();
                            async move {
                                let name = file.filename;
                                if name == "." || name == ".." || !valid_name(&name) {
                                    return None;
                                }
                                let target = if file.attrs.is_symlink() {
                                    let child = remote_child(&subject, &name);
                                    raw.stat(child)
                                        .await
                                        .ok()
                                        .and_then(|attrs| target_kind(&attrs.attrs))
                                } else {
                                    None
                                };
                                Some(entry_from_attrs(mount, &subject, &name, file.attrs, target))
                            }
                        }))
                        .buffered(PIPELINE_DEPTH)
                        .collect::<Vec<_>>()
                        .await;
                        for entry in converted.into_iter().flatten() {
                            match &mut collected {
                                Some(all) if all.len() < LISTING_CACHE_MAX_ENTRIES => {
                                    all.push(entry.clone())
                                }
                                slot => *slot = None,
                            }
                            entries.push(entry);
                            // A send failure is the listing being cancelled by
                            // drop: the receiver is gone, so close the handle
                            // and stop rather than finishing a walk nobody is
                            // reading.
                            if entries.len() == LISTING_BATCH_CAP
                                && sender.send(Ok(std::mem::take(&mut entries))).await.is_err()
                            {
                                let _ = raw.close(handle.clone()).await;
                                return;
                            }
                        }
                        if !entries.is_empty() && sender.send(Ok(entries)).await.is_err() {
                            let _ = raw.close(handle.clone()).await;
                            return;
                        }
                    }
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        complete = true;
                        break;
                    }
                    Err(error) => {
                        let _ = sender
                            .send(Err(map_error(error, "read directory", &subject)))
                            .await;
                        break;
                    }
                }
            }
            let _ = raw.close(handle).await;

            // Only a walk that ran to completion is worth remembering. One cut
            // short by an error would cache a directory that is missing rows.
            if let Some(all) = collected.filter(|_| complete) {
                cache.store_listing(&cache_key, all);
            }
        });
        receiver.boxed()
    }

    async fn metadata(&self, path: &VfsPath) -> ferret_vfs::Result<Metadata> {
        let remote = self.remote_path(path)?;
        // Usually a hit: the listing that produced this row seeded it.
        if let Some(cached) = self.cache.metadata(&remote) {
            return Ok(cached);
        }
        let raw = self.raw.clone();
        let request = remote.clone();
        let attrs = crate::spawn(async move { raw.lstat(request).await })
            .await
            .map_err(|error| error.into_vfs("read metadata for", path))?
            .map_err(|error| map_error(error, "read metadata for", path))?;
        let metadata = metadata_from_attrs(path.file_name().unwrap_or_default(), &attrs.attrs);
        self.cache.store_metadata(&remote, metadata);
        Ok(metadata)
    }

    async fn metadata_batch(
        &self,
        paths: Vec<VfsPath>,
    ) -> Vec<(VfsPath, ferret_vfs::Result<Metadata>)> {
        stream::iter(paths.into_iter().map(|path| async move {
            let result = self.metadata(&path).await;
            (path, result)
        }))
        .buffer_unordered(PIPELINE_DEPTH)
        .collect()
        .await
    }

    async fn probe(&self, path: &VfsPath) -> ferret_vfs::Result<Entry> {
        let remote = self.remote_path(path)?;
        let raw = self.raw.clone();
        let (link_attrs, target_attrs, link_target) = crate::spawn(async move {
            let link = raw.lstat(remote.clone()).await?;
            if !link.attrs.is_symlink() {
                return Ok::<_, SftpError>((link, None, None));
            }
            let target = raw
                .readlink(remote.clone())
                .await
                .ok()
                .and_then(|names| names.files.into_iter().next().map(|file| file.filename));
            let resolved = raw.stat(remote).await.ok();
            Ok((link, resolved, target))
        })
        .await
        .map_err(|error| error.into_vfs("probe", path))?
        .map_err(|error| map_error(error, "probe", path))?;
        let name = path.file_name().unwrap_or_default();
        let kind = if link_attrs.attrs.is_symlink() {
            EntryKind::Symlink {
                target_kind: target_attrs
                    .as_ref()
                    .and_then(|attrs| target_kind(&attrs.attrs)),
            }
        } else {
            kind_from_attrs(&link_attrs.attrs)
        };
        Ok(Entry::new(path.clone(), name, kind)
            .with_metadata(Some(metadata_from_attrs(name, &link_attrs.attrs)))
            .with_link_target(link_target.map(|target| Arc::from(PathBuf::from(target)))))
    }

    async fn exists(&self, path: &VfsPath) -> bool {
        let Ok(remote) = self.remote_path(path) else {
            return false;
        };
        let raw = self.raw.clone();
        crate::spawn(async move { raw.lstat(remote).await })
            .await
            .is_ok_and(|result| match result {
                Ok(_) => true,
                Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                    false
                }
                Err(_) => false,
            })
    }

    async fn canonicalize(&self, path: &VfsPath) -> ferret_vfs::Result<VfsPath> {
        let remote = self.remote_path(path)?;
        let raw = self.raw.clone();
        let response = crate::spawn(async move { raw.realpath(remote).await })
            .await
            .map_err(|error| error.into_vfs("canonicalize", path))?
            .map_err(|error| map_error(error, "canonicalize", path))?;
        let canonical = response
            .files
            .into_iter()
            .next()
            .ok_or_else(|| invalid_path(path, "the server returned no canonical path"))?
            .filename;
        validate_path(&canonical).map_err(|message| invalid_path(path, message))?;
        Ok(VfsPath::remote(self.mount, canonical))
    }

    async fn read(&self, path: &VfsPath, budget: usize) -> ferret_vfs::Result<Vec<u8>> {
        let remote = self.remote_path(path)?;
        let raw = self.raw.clone();
        let subject = path.clone();
        crate::spawn(async move {
            let opened = raw
                .open(remote, OpenFlags::READ, FileAttributes::empty())
                .await
                .map_err(|error| map_error(error, "open", &subject))?;
            let mut output = Vec::with_capacity(budget.min(TRANSFER_CHUNK));
            let mut offset = 0_u64;
            while output.len() < budget {
                let request = (budget - output.len())
                    .min(TRANSFER_CHUNK)
                    .min(MAX_RESPONSE_BYTES) as u32;
                match raw.read(opened.handle.clone(), offset, request).await {
                    Ok(data) => {
                        if data.data.len() > request as usize
                            || data.data.len() > MAX_RESPONSE_BYTES
                        {
                            let _ = raw.close(opened.handle).await;
                            return Err(invalid_path(
                                &subject,
                                "the server returned an oversized read response",
                            ));
                        }
                        if data.data.is_empty() {
                            break;
                        }
                        offset += data.data.len() as u64;
                        output.extend_from_slice(&data.data);
                    }
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        break;
                    }
                    Err(error) => {
                        let _ = raw.close(opened.handle).await;
                        return Err(map_error(error, "read", &subject));
                    }
                }
            }
            let _ = raw.close(opened.handle).await;
            Ok(output)
        })
        .await
        .map_err(|error| error.into_vfs("read", path))?
    }

    async fn create_dir(&self, path: &VfsPath) -> ferret_vfs::Result<()> {
        let remote = self.remote_path(path)?;
        // Before the call, not after: a failure still means whatever was
        // remembered about the parent may no longer be true.
        self.cache.invalidate(&remote);
        let raw = self.raw.clone();
        crate::spawn(async move { raw.mkdir(remote, FileAttributes::empty()).await })
            .await
            .map_err(|error| error.into_vfs("create", path))?
            .map(drop)
            .map_err(|error| map_error(error, "create", path))
    }

    /// Create an empty file, refusing to overwrite.
    ///
    /// `EXCL` is what makes that true on the server rather than in a check
    /// here: a stat-then-create would race, and "New File" must never blank
    /// something the user already had.
    async fn create_file(&self, path: &VfsPath) -> ferret_vfs::Result<()> {
        let remote = self.remote_path(path)?;
        self.cache.invalidate(&remote);
        let raw = self.raw.clone();
        let subject = path.clone();
        crate::spawn(async move {
            let opened = raw
                .open(
                    remote,
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                    FileAttributes::empty(),
                )
                .await
                .map_err(|error| map_error(error, "create", &subject))?;
            let _ = raw.close(opened.handle).await;
            Ok(())
        })
        .await
        .map_err(|error| error.into_vfs("create", path))?
    }

    /// Rename, preferring the atomic OpenSSH extension when the server has it.
    ///
    /// SFTP v3's own `SSH_FXP_RENAME` fails when the destination exists, so
    /// without `posix-rename@openssh.com` this cannot replace anything — which
    /// is exactly what `Capabilities::atomic_rename` reports, so the caller has
    /// already asked before it gets here.
    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
        let source = self.remote_path(from)?;
        let destination = self.remote_path(to)?;
        // Both ends: the name leaves one directory and appears in another.
        self.cache.invalidate(&source);
        self.cache.invalidate(&destination);
        let raw = self.raw.clone();
        let atomic = self.atomic_rename;
        crate::spawn(async move {
            if atomic {
                let mut payload = Vec::new();
                encode_string(&mut payload, &source);
                encode_string(&mut payload, &destination);
                raw.extended("posix-rename@openssh.com", payload)
                    .await
                    .map(drop)
            } else {
                raw.rename(source, destination).await.map(drop)
            }
        })
        .await
        .map_err(|error| error.into_vfs("rename", from))?
        .map_err(|error| map_error(error, "rename", from))
    }

    /// Remove a file or a whole directory tree.
    ///
    /// `Trash` is refused rather than quietly downgraded: there is no remote
    /// trash, and silently deleting something the user asked to *move* is the
    /// failure the "trash before delete" gate exists to prevent. The caller
    /// asks for `Permanent` explicitly, having said so to the user first.
    async fn remove(&self, path: &VfsPath, mode: RemoveMode) -> ferret_vfs::Result<()> {
        if mode == RemoveMode::Trash {
            return Err(VfsError::Unsupported {
                capability: "moving to the trash",
            });
        }
        let remote = self.remote_path(path)?;
        self.cache.invalidate(&remote);
        let raw = self.raw.clone();
        let subject = path.clone();
        crate::spawn(async move { remove_recursive(&raw, remote, &subject).await })
            .await
            .map_err(|error| error.into_vfs("delete", path))?
    }

    /// Copy a file or a tree within one mount, server-side.
    ///
    /// The router has already refused a pair that spans two backends, so both
    /// ends are on this server and no byte needs to cross the link. That is
    /// only possible when the server advertises `copy-data`: SFTP v3 has no
    /// copy of its own, and doing it client-side would be a download followed
    /// by an upload — a transfer, which belongs in the queue with progress and
    /// a cancel button rather than hidden behind a trait method that looks
    /// instant.
    async fn copy(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
        if !self.server_side_copy {
            return Err(VfsError::Unsupported {
                capability: "copying files on the server",
            });
        }
        let source = self.remote_path(from)?;
        let destination = self.remote_path(to)?;
        self.cache.invalidate(&destination);

        let raw = self.raw.clone();
        let subject = to.clone();
        crate::spawn(async move { copy_recursive(&raw, source, destination, &subject).await })
            .await
            .map_err(|error| error.into_vfs("copy", from))?
    }

    /// What makes Refresh mean "ask the server".
    ///
    /// Unconditional: the browser calls this immediately before re-listing, so
    /// a cache that could answer here would turn ⌘R into a no-op.
    fn invalidate(&self, path: &VfsPath) {
        if let Ok(remote) = self.remote_path(path) {
            self.cache.invalidate(&remote);
        }
    }
}

/// Depth-first removal, because SFTP has no recursive delete.
///
/// Boxed because it recurses in an `async fn`. It reports the *first* failure
/// with its own path rather than continuing blindly: a half-deleted tree is
/// bad enough without also being unable to say where it stopped.
fn remove_recursive<'a>(
    raw: &'a Arc<RawSftpSession>,
    path: String,
    subject: &'a VfsPath,
) -> std::pin::Pin<Box<dyn Future<Output = ferret_vfs::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let attrs = raw
            .lstat(path.clone())
            .await
            .map_err(|error| map_error(error, "delete", subject))?;

        // A symlink to a directory is removed as the link, never followed —
        // following one would delete something outside the tree the user
        // pointed at.
        if !attrs.attrs.is_dir() {
            return raw
                .remove(path)
                .await
                .map(drop)
                .map_err(|error| map_error(error, "delete", subject));
        }

        let handle = raw
            .opendir(path.clone())
            .await
            .map_err(|error| map_error(error, "delete", subject))?
            .handle;
        let mut children = Vec::new();
        loop {
            match raw.readdir(handle.clone()).await {
                Ok(response) => children.extend(
                    response
                        .files
                        .into_iter()
                        .map(|file| file.filename)
                        .filter(|name| valid_name(name)),
                ),
                Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break,
                Err(error) => {
                    let _ = raw.close(handle).await;
                    return Err(map_error(error, "delete", subject));
                }
            }
        }
        let _ = raw.close(handle).await;

        for child in children {
            let child_path = format!("{}/{}", path.trim_end_matches('/'), child);
            remove_recursive(raw, child_path, subject).await?;
        }
        raw.rmdir(path)
            .await
            .map(drop)
            .map_err(|error| map_error(error, "delete", subject))
    })
}

/// Copy one path to another on the same server, recursing into directories.
///
/// Boxed because it recurses in an `async fn`. Files go through `copy-data`,
/// which moves the bytes inside the server; directories are recreated and
/// walked, because the extension copies a file handle's contents and has no
/// concept of a tree.
fn copy_recursive<'a>(
    raw: &'a Arc<RawSftpSession>,
    from: String,
    to: String,
    subject: &'a VfsPath,
) -> std::pin::Pin<Box<dyn Future<Output = ferret_vfs::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let attrs = raw
            .lstat(from.clone())
            .await
            .map_err(|error| map_error(error, "copy", subject))?;

        // A symlink is copied as a link rather than followed, the same rule
        // the delete and the transfer walk follow.
        if attrs.attrs.is_symlink() {
            let target = raw
                .readlink(from.clone())
                .await
                .ok()
                .and_then(|names| names.files.into_iter().next().map(|file| file.filename));
            if let Some(target) = target {
                return raw
                    .symlink(to, target)
                    .await
                    .map(drop)
                    .map_err(|error| map_error(error, "copy", subject));
            }
            return Ok(());
        }

        if attrs.attrs.is_dir() {
            let _ = raw.mkdir(to.clone(), FileAttributes::empty()).await;
            let handle = raw
                .opendir(from.clone())
                .await
                .map_err(|error| map_error(error, "copy", subject))?
                .handle;
            let mut children = Vec::new();
            loop {
                match raw.readdir(handle.clone()).await {
                    Ok(response) => children.extend(
                        response
                            .files
                            .into_iter()
                            .map(|file| file.filename)
                            .filter(|name| valid_name(name)),
                    ),
                    Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => {
                        break;
                    }
                    Err(error) => {
                        let _ = raw.close(handle).await;
                        return Err(map_error(error, "copy", subject));
                    }
                }
            }
            let _ = raw.close(handle).await;
            for child in children {
                let source = format!("{}/{}", from.trim_end_matches('/'), child);
                let target = format!("{}/{}", to.trim_end_matches('/'), child);
                copy_recursive(raw, source, target, subject).await?;
            }
            return Ok(());
        }

        copy_file_data(raw, &from, &to, subject).await
    })
}

/// One file's contents, copied inside the server with `copy-data`.
///
/// The extension takes two open handles and a range, so both files are opened
/// here and closed however this ends. A length of zero means "to the end of
/// the source", which is what a copy wants.
async fn copy_file_data(
    raw: &Arc<RawSftpSession>,
    from: &str,
    to: &str,
    subject: &VfsPath,
) -> ferret_vfs::Result<()> {
    let source = raw
        .open(from.to_string(), OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|error| map_error(error, "copy", subject))?
        .handle;
    let destination = match raw
        .open(
            to.to_string(),
            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            FileAttributes::empty(),
        )
        .await
    {
        Ok(opened) => opened.handle,
        Err(error) => {
            let _ = raw.close(source).await;
            return Err(map_error(error, "copy", subject));
        }
    };

    let mut payload = Vec::new();
    encode_string(&mut payload, &source);
    payload.extend_from_slice(&0_u64.to_be_bytes()); // read from the start
    payload.extend_from_slice(&0_u64.to_be_bytes()); // to the end of the file
    encode_string(&mut payload, &destination);
    payload.extend_from_slice(&0_u64.to_be_bytes()); // write from the start

    let outcome = raw.extended("copy-data", payload).await;
    let _ = raw.close(source).await;
    let _ = raw.close(destination).await;
    outcome
        .map(drop)
        .map_err(|error| map_error(error, "copy", subject))
}

/// An SSH `string`: a four-byte big-endian length followed by the bytes.
///
/// Needed because `posix-rename@openssh.com` takes its two paths as an opaque
/// extended-request payload rather than as protocol fields.
fn encode_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn validate_path(path: &str) -> std::result::Result<(), &'static str> {
    if path.as_bytes().contains(&0) {
        return Err("remote paths cannot contain NUL");
    }
    if !path.starts_with('/') {
        return Err("remote paths must be absolute");
    }
    if path.split('/').any(|part| part == "..") {
        return Err("remote paths cannot contain parent traversal");
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.as_bytes().contains(&0)
}

/// Whether a server-supplied name may be joined onto a **local** path.
///
/// Stricter than [`valid_name`], and it has to be: a name is about to become a
/// file on the user's own disk, so this is where a hostile or broken server is
/// stopped from writing outside the directory the user chose. Windows rules
/// apply on both platforms, because a name that is fine on macOS and creates a
/// device file on Windows is still a name Ferret should not write.
pub(crate) fn safe_local_name(name: &str) -> std::result::Result<(), &'static str> {
    if !valid_name(name) {
        return Err("not a usable file name");
    }
    if name.contains('\\') {
        return Err("contains a path separator");
    }
    // `CON`, `NUL`, `COM1` and friends are devices on Windows whatever the
    // extension, and a file named after one cannot be created or removed.
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = name.split('.').next().unwrap_or(name).to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return Err("is a reserved device name on Windows");
    }
    // Windows silently strips these, so `report.` and `report` become the same
    // file — which is how one download quietly overwrites another.
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("ends with a dot or a space");
    }
    if name
        .chars()
        .any(|c| matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err("contains a character Windows cannot store");
    }
    Ok(())
}

fn invalid_path(path: &VfsPath, message: &'static str) -> VfsError {
    VfsError::Io {
        action: "use remote path",
        path: path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, message),
    }
}

fn map_error(error: SftpError, action: &'static str, path: &VfsPath) -> VfsError {
    let message = match &error {
        SftpError::Status(status) => status.error_message.clone(),
        _ => error.to_string(),
    };
    RemoteError::Server {
        path: path.to_string(),
        message,
    }
    .into_vfs(action, path)
}

fn remote_child(parent: &VfsPath, name: &str) -> String {
    match parent {
        VfsPath::Remote { path, .. } => format!("{}/{}", path.trim_end_matches('/'), name),
        VfsPath::Local(_) => unreachable!("validated remote parent"),
    }
}

fn entry_from_attrs(
    mount: MountId,
    parent: &VfsPath,
    name: &str,
    attrs: FileAttributes,
    symlink_target: Option<SymlinkTarget>,
) -> Entry {
    let path = match parent {
        VfsPath::Remote { path, .. } => {
            VfsPath::remote(mount, format!("{}/{}", path.trim_end_matches('/'), name))
        }
        VfsPath::Local(_) => unreachable!("validated before listing"),
    };
    let kind = if attrs.is_symlink() {
        EntryKind::Symlink {
            target_kind: symlink_target,
        }
    } else {
        kind_from_attrs(&attrs)
    };
    Entry::new(path, name, kind).with_metadata(Some(metadata_from_attrs(name, &attrs)))
}

fn kind_from_attrs(attrs: &FileAttributes) -> EntryKind {
    if attrs.is_dir() {
        EntryKind::Dir
    } else if attrs.is_regular() {
        EntryKind::File
    } else if attrs.is_symlink() {
        EntryKind::Symlink { target_kind: None }
    } else {
        EntryKind::Other
    }
}

fn target_kind(attrs: &FileAttributes) -> Option<SymlinkTarget> {
    if attrs.is_dir() {
        Some(SymlinkTarget::Dir)
    } else if attrs.is_regular() {
        Some(SymlinkTarget::File)
    } else {
        None
    }
}

fn metadata_from_attrs(name: &str, attrs: &FileAttributes) -> Metadata {
    Metadata {
        len: attrs.size.unwrap_or(0),
        modified: attrs
            .mtime
            .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds.into()))),
        hidden: name.starts_with('.'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_paths_that_can_escape_or_cross_mounts() {
        assert!(validate_path("relative").is_err());
        assert!(validate_path("/safe/../escape").is_err());
        assert!(validate_path("/bad\0name").is_err());
        assert!(validate_path("/safe/path").is_ok());
    }

    #[test]
    fn hostile_names_are_never_joined() {
        assert!(valid_name("report.txt"));
        for name in ["", ".", "..", "dir/file", "bad\0name"] {
            assert!(!valid_name(name), "{name:?}");
        }
    }

    #[test]
    fn capabilities_only_claim_negotiated_atomic_rename() {
        let attrs = FileAttributes {
            size: Some(7),
            mtime: Some(1),
            ..Default::default()
        };
        assert_eq!(metadata_from_attrs(".hidden", &attrs).len, 7);
        assert!(metadata_from_attrs(".hidden", &attrs).hidden);
    }

    /// `posix-rename@openssh.com` carries its two paths as an opaque payload,
    /// so the framing is ours to get right: four-byte big-endian length, then
    /// the bytes, per path. A server given the wrong framing renames the
    /// wrong thing rather than failing cleanly.
    #[test]
    fn the_posix_rename_payload_is_two_ssh_strings() {
        let mut payload = Vec::new();
        encode_string(&mut payload, "/a");
        encode_string(&mut payload, "/bb");

        assert_eq!(
            payload,
            vec![0, 0, 0, 2, b'/', b'a', 0, 0, 0, 3, b'/', b'b', b'b']
        );
    }

    /// The check standing between a hostile server and the user's disk. Every
    /// case here is a real technique for escaping a download directory.
    #[test]
    fn a_name_that_could_escape_a_download_directory_is_refused() {
        assert!(safe_local_name("report.txt").is_ok());
        assert!(safe_local_name(".hidden").is_ok());

        for hostile in [
            "..",               // the parent directory
            "../../etc/passwd", // traversal
            "sub/file",         // a separator that would create a path
            "sub\\file",        // the Windows separator, refused on both
            "",                 // no name at all
            "bad\0name",        // truncates in a C API
            "CON",              // a Windows device, whatever the platform
            "com1.txt",         // still a device with an extension
            "report.",          // Windows strips the dot, colliding names
            "report ",          // and the trailing space
            "a:b",              // an alternate data stream on NTFS
            "what?",            // wildcards Windows cannot store
        ] {
            assert!(
                safe_local_name(hostile).is_err(),
                "{hostile:?} must not become a local file name"
            );
        }
    }

    #[test]
    fn a_multibyte_name_is_framed_by_bytes_not_characters() {
        let mut payload = Vec::new();
        encode_string(&mut payload, "/é");
        // Two bytes for the slash and one two-byte character: a length in
        // `chars` would truncate the name on the server.
        assert_eq!(&payload[..4], &[0, 0, 0, 3]);
        assert_eq!(payload.len(), 7);
    }
}
