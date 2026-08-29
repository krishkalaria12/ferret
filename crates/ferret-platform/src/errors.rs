use std::io;
use std::path::PathBuf;

/// This crate's `Result`, with its error type as the default.
pub type Result<T, E = PlatformError> = std::result::Result<T, E>;

/// Failures the shell integrations can produce.
///
/// Every variant names what it was trying to do and to what, so the message
/// can be shown to a person without editing.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("{path} no longer exists.")]
    NotFound { path: PathBuf },

    #[error("Ferret doesn't have permission to open {path}.")]
    PermissionDenied { path: PathBuf },

    #[error("Ferret can't open a terminal in {path} because it isn't a folder.")]
    NotADirectory { path: PathBuf },

    /// The current platform build does not implement the operation. Linux is
    /// deferred (D-19); a missing backend must say so rather than guess.
    #[error("This platform doesn't support {capability} yet.")]
    Unsupported { capability: &'static str },

    #[error("Couldn't {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl PlatformError {
    pub fn from_io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        let path = path.into();
        match source.kind() {
            io::ErrorKind::NotFound => PlatformError::NotFound { path },
            io::ErrorKind::PermissionDenied => PlatformError::PermissionDenied { path },
            io::ErrorKind::NotADirectory => PlatformError::NotADirectory { path },
            _ => PlatformError::Io {
                action,
                path,
                source,
            },
        }
    }
}
