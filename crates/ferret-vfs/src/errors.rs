use crate::VfsPath;
use std::io;

/// This crate's `Result`, with its error type as the default.
///
/// The default parameter is what ties the two together: `Result<T>` means
/// `Result<T, VfsError>` at every call site in the crate, so the error type is
/// never spelled out and never accidentally varies. The second parameter stays
/// open so a caller that needs a different error — a test, or a caller layering
/// `anyhow` on top — can still write `Result<T, OtherError>` without importing
/// `std::result::Result` and shadowing this one.
pub type Result<T, E = VfsError> = std::result::Result<T, E>;

/// Failures the file list has to render.
///
/// Every variant carries the path it concerns, because "Operation not
/// permitted" with no subject is the failure mode this type exists to prevent.
/// The `Display` text is written to be shown to a person as-is.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("{path} no longer exists.")]
    NotFound { path: VfsPath },

    #[error("Ferret can't read {path} — you don't have permission to open it.")]
    PermissionDenied { path: VfsPath },

    #[error("{path} already exists.")]
    AlreadyExists { path: VfsPath },

    #[error("{path} isn't a folder.")]
    NotADirectory { path: VfsPath },

    #[error("{path} is on a different volume, so it has to be copied rather than moved.")]
    CrossVolume { path: VfsPath },

    #[error(
        "Ferret can't save {path} — it changed on disk since you started editing. Discard your edits to reload it, or copy your text elsewhere first."
    )]
    Conflict { path: VfsPath },

    #[error("This location doesn't support {capability}.")]
    Unsupported { capability: &'static str },

    /// Somebody stopped the operation part-way through.
    ///
    /// Not a failure and never shown as one: the operations queue (F-60) reads
    /// it as "this job ended early", which is a different thing from "this job
    /// broke". It names its path anyway, because the message a person sees next
    /// to it — "Cancelled" beside a half-finished copy — is more useful when
    /// the copy's path is already on screen.
    #[error("{path} was cancelled before the operation finished.")]
    Cancelled { path: VfsPath },

    /// A remote path whose mount is not connected — a tab restored from a
    /// previous session, a server that dropped, or a record the user deleted.
    #[error("Ferret isn't connected to the server that holds {path}.")]
    NotMounted { path: VfsPath },

    /// The two ends of a `rename` or `copy` live on different backends.
    ///
    /// Deliberately *not* [`VfsError::CrossVolume`], whose text promises a
    /// copy: two local volumes copy in seconds, and a server is a network
    /// transfer that needs progress and a cancel button. The caller routes
    /// this to the transfer queue rather than retrying.
    #[error(
        "{from} and {to} are on different filesystems, so this has to be a transfer rather than a move."
    )]
    CrossBackend { from: VfsPath, to: VfsPath },

    #[error("Couldn't {action} {from} to {to}: {source}")]
    RenameFailed {
        action: &'static str,
        from: VfsPath,
        to: VfsPath,
        #[source]
        source: io::Error,
    },

    #[error("Couldn't {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: VfsPath,
        #[source]
        source: io::Error,
    },
}

impl VfsError {
    /// Classify an [`io::Error`] into the variants worth showing differently.
    ///
    /// `action` is a verb phrase completing "Couldn't …" — `"read"`,
    /// `"rename"`, `"create"`.
    pub fn from_io(action: &'static str, path: &VfsPath, source: io::Error) -> Self {
        match source.kind() {
            io::ErrorKind::NotFound => VfsError::NotFound { path: path.clone() },
            io::ErrorKind::PermissionDenied => VfsError::PermissionDenied { path: path.clone() },
            io::ErrorKind::AlreadyExists => VfsError::AlreadyExists { path: path.clone() },
            io::ErrorKind::NotADirectory => VfsError::NotADirectory { path: path.clone() },
            _ => VfsError::Io {
                action,
                path: path.clone(),
                source,
            },
        }
    }

    pub fn from_rename_io(from: &VfsPath, to: &VfsPath, source: io::Error) -> Self {
        match source.kind() {
            io::ErrorKind::NotFound => VfsError::NotFound { path: from.clone() },
            io::ErrorKind::AlreadyExists => VfsError::AlreadyExists { path: to.clone() },
            _ => VfsError::RenameFailed {
                action: "rename",
                from: from.clone(),
                to: to.clone(),
                source,
            },
        }
    }

    /// Whether the file pane should show an empty listing rather than an error
    /// banner. A folder that vanished mid-navigation is handled by navigating,
    /// not by shouting.
    pub fn is_gone(&self) -> bool {
        matches!(self, VfsError::NotFound { .. })
    }

    /// The path this error is about, when it names one.
    ///
    /// For the two variants that do not — [`VfsError::Unsupported`], which is
    /// about a backend rather than a file, and [`VfsError::CrossBackend`],
    /// which is about a pair — this returns the side that matters: the source
    /// for a crossing, and nothing for an unsupported capability. A caller
    /// that has to name the subject in a sentence should not have to match on
    /// every variant to find it.
    pub fn path(&self) -> Option<&VfsPath> {
        match self {
            VfsError::NotFound { path }
            | VfsError::PermissionDenied { path }
            | VfsError::AlreadyExists { path }
            | VfsError::NotADirectory { path }
            | VfsError::CrossVolume { path }
            | VfsError::Conflict { path }
            | VfsError::Cancelled { path }
            | VfsError::NotMounted { path }
            | VfsError::Io { path, .. } => Some(path),
            VfsError::RenameFailed { from, .. } => Some(from),
            VfsError::CrossBackend { from, .. } => Some(from),
            VfsError::Unsupported { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denied_names_the_path() {
        let err = VfsError::from_io(
            "read",
            &VfsPath::local("/private/var/root"),
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        let msg = err.to_string();
        assert!(msg.contains("/private/var/root"), "{msg}");
        assert!(msg.contains("permission"), "{msg}");
    }

    #[test]
    fn unclassified_errors_keep_the_verb_and_the_path() {
        let err = VfsError::from_io(
            "rename",
            &VfsPath::local("/x/y"),
            io::Error::other("disk on fire"),
        );
        let msg = err.to_string();
        assert!(msg.contains("rename"), "{msg}");
        assert!(msg.contains("/x/y"), "{msg}");
        assert!(msg.contains("disk on fire"), "{msg}");
    }

    #[test]
    fn rename_failures_name_both_paths() {
        let err = VfsError::from_rename_io(
            &VfsPath::local("/x/old"),
            &VfsPath::local("/x/new"),
            io::Error::other("rename failed"),
        );
        let message = err.to_string();
        assert!(message.contains("/x/old"), "{message}");
        assert!(message.contains("/x/new"), "{message}");
    }

    #[test]
    fn only_not_found_counts_as_gone() {
        assert!(
            VfsError::NotFound {
                path: VfsPath::local("/a")
            }
            .is_gone()
        );
        assert!(
            !VfsError::PermissionDenied {
                path: VfsPath::local("/a")
            }
            .is_gone()
        );
    }
}
