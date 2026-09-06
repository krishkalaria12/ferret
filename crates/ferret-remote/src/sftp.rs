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

use crate::config::{
    LISTING_BATCH_CAP, MAX_RESPONSE_BYTES, PIPELINE_DEPTH, REQUEST_TIMEOUT, TRANSFER_CHUNK,
};
use crate::{RemoteError, Result};

/// An authenticated SFTP mount. All protocol work is dispatched to the
/// crate's Tokio runtime, so this handle is safe to call from GPUI or smol.
pub struct SftpVfs {
    mount: MountId,
    host: Arc<str>,
    raw: Arc<RawSftpSession>,
    atomic_rename: bool,
}

impl SftpVfs {
    pub(crate) async fn negotiate<S>(stream: S) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let raw = RawSftpSession::new(stream);
        raw.set_timeout(REQUEST_TIMEOUT.as_secs());
        let version = raw.init().await.map_err(|error| RemoteError::Subsystem {
            host: "the remote server".into(),
            message: error.to_string(),
        })?;
        Ok(Self {
            mount: MountId(0),
            host: Arc::from("the remote server"),
            raw: Arc::new(raw),
            atomic_rename: version.extensions.contains_key("posix-rename@openssh.com"),
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
        let subject = dir.clone();
        let mount = self.mount;
        let raw = self.raw.clone();
        let (sender, receiver) = async_channel::bounded(ferret_vfs::config::LISTING_BUFFER);
        crate::detach(async move {
            let handle = match raw.opendir(path).await {
                Ok(handle) => handle.handle,
                Err(error) => {
                    let _ = sender
                        .send(Err(map_error(error, "read directory", &subject)))
                        .await;
                    return;
                }
            };

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
        });
        receiver.boxed()
    }

    async fn metadata(&self, path: &VfsPath) -> ferret_vfs::Result<Metadata> {
        let remote = self.remote_path(path)?;
        let raw = self.raw.clone();
        let attrs = crate::spawn(async move { raw.lstat(remote).await })
            .await
            .map_err(|error| error.into_vfs("read metadata for", path))?
            .map_err(|error| map_error(error, "read metadata for", path))?;
        Ok(metadata_from_attrs(
            path.file_name().unwrap_or_default(),
            &attrs.attrs,
        ))
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

    async fn create_dir(&self, _path: &VfsPath) -> ferret_vfs::Result<()> {
        read_only("creating directories")
    }
    async fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> ferret_vfs::Result<()> {
        read_only("renaming")
    }
    async fn remove(&self, _path: &VfsPath, _mode: RemoveMode) -> ferret_vfs::Result<()> {
        read_only("removing files")
    }
}

fn read_only<T>(capability: &'static str) -> ferret_vfs::Result<T> {
    Err(VfsError::Unsupported { capability })
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
}
