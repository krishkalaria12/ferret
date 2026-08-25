use crate::VfsPath;
use std::io;

pub type Result<T> = std::result::Result<T, VfsError>;

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

    #[error("This location doesn't support {capability}.")]
    Unsupported { capability: &'static str },

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

    /// Whether the file pane should show an empty listing rather than an error
    /// banner. A folder that vanished mid-navigation is handled by navigating,
    /// not by shouting.
    pub fn is_gone(&self) -> bool {
        matches!(self, VfsError::NotFound { .. })
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
