//! This crate's error type and its `Result` alias.

use ferret_vfs::{VfsError, VfsPath};
use std::path::PathBuf;

/// This crate's `Result`, with its error type as the default parameter — so
/// every signature reads `Result<T>` and the error type is never spelled out
/// or accidentally varied.
pub type Result<T, E = RemoteError> = std::result::Result<T, E>;

/// What can go wrong between here and a server.
///
/// Every variant names its subject, and the `Display` text is written to be
/// shown to a person unedited: "connection failed" with no host is the failure
/// this type exists to prevent.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("Couldn't reach {host} on port {port}: {message}")]
    Connect {
        host: String,
        port: u16,
        message: String,
    },

    /// The server's key is not in `known_hosts`. The caller shows the
    /// fingerprint and asks; it must never be accepted silently.
    #[error("{host} isn't in your known_hosts file yet.")]
    UnknownHostKey {
        host: String,
        port: u16,
        fingerprint: String,
        algorithm: String,
    },

    /// The server presented a key that does not match the recorded one. This
    /// is either a rekeyed server or an interception, and Ferret cannot tell
    /// the difference — so it refuses and points at the file to inspect.
    #[error(
        "The host key for {host} has changed. If this wasn't expected, do not connect — \
         someone may be impersonating the server. The recorded key is in {file}."
    )]
    HostKeyMismatch {
        host: String,
        port: u16,
        fingerprint: String,
        file: PathBuf,
    },

    #[error("Couldn't read the known_hosts file at {file}: {message}")]
    KnownHosts { file: PathBuf, message: String },

    #[error("{host} rejected every way Ferret offered to sign in as {user}: {tried}")]
    AuthFailed {
        host: String,
        user: String,
        tried: String,
    },

    /// Auth needs something the caller has to ask a person for. Carrying the
    /// prompt means the dialog can name the host and the key it is for, which
    /// is what stops a credential prompt from being a phishing surface.
    #[error("{host} needs {}", .prompt)]
    AuthPrompt {
        host: String,
        user: String,
        prompt: String,
        kind: PromptKind,
    },

    #[error("Couldn't read the SSH key at {path}: {message}")]
    KeyFile { path: PathBuf, message: String },

    #[error("Couldn't open an SFTP session on {host}: {message}")]
    Subsystem { host: String, message: String },

    /// A root mount was asked for and the host would not give one.
    ///
    /// Its own variant because the fix is specific and worth naming: the
    /// account needs passwordless `sudo` for the SFTP server binary. F-23 says
    /// to document that requirement rather than paper over it, and an error
    /// that says "handshake failed" would be papering over it.
    #[error("{host} wouldn't start an SFTP server as root: {message}")]
    RootUnavailable { host: String, message: String },

    /// The link went away mid-operation. Distinct from an I/O error on a path,
    /// because the recovery is different: reconnect, not retry.
    #[error("The connection to {host} was lost.")]
    Disconnected { host: String },

    #[error("{path} — the server said: {message}")]
    Server { path: String, message: String },

    /// A server sent a directory entry whose name cannot be part of a path.
    /// Treated as hostile input rather than as a quirk: joining it onto a
    /// local path is how a download escapes its destination.
    #[error("{host} sent an entry named {name:?}, which isn't a usable file name.")]
    MalformedName { host: String, name: String },

    #[error("Couldn't read the connection list at {path}: {message}")]
    Store { path: PathBuf, message: String },

    #[error("Couldn't reach the system keychain for {label}: {message}")]
    Keychain { label: String, message: String },

    #[error("Ferret isn't connected to {name}.")]
    NotConnected { name: String },

    #[error("This server doesn't support {capability}.")]
    Unsupported { capability: &'static str },

    #[error("Cancelled.")]
    Cancelled,
}

/// What a credential prompt is asking for, so the dialog can pick its field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// An encrypted private key needs its passphrase.
    KeyPassphrase,
    /// The account password.
    Password,
    /// A keyboard-interactive challenge, typically a second factor.
    Challenge,
}

impl RemoteError {
    /// Whether the mount should be torn down rather than the operation retried.
    pub fn is_fatal_to_session(&self) -> bool {
        matches!(
            self,
            RemoteError::Disconnected { .. }
                | RemoteError::Connect { .. }
                | RemoteError::AuthFailed { .. }
                | RemoteError::HostKeyMismatch { .. }
        )
    }

    /// Translate into the storage layer's error, for the `Vfs` boundary.
    ///
    /// `path` is the operation's subject and `action` is the verb phrase
    /// completing "Couldn't …" — both are carried through, because the file
    /// pane renders these and a message that does not say what failed on what
    /// is not worth showing.
    pub fn into_vfs(self, action: &'static str, path: &VfsPath) -> VfsError {
        use std::io;

        let kind = match &self {
            RemoteError::Server { message, .. } => classify(message),
            // The one variant with an exact counterpart: the file pane draws
            // "this location doesn't support X" without an I/O wrapper.
            RemoteError::Unsupported { capability } => {
                return VfsError::Unsupported { capability };
            }
            _ => io::ErrorKind::Other,
        };
        match kind {
            io::ErrorKind::NotFound => VfsError::NotFound { path: path.clone() },
            io::ErrorKind::PermissionDenied => VfsError::PermissionDenied { path: path.clone() },
            io::ErrorKind::AlreadyExists => VfsError::AlreadyExists { path: path.clone() },
            _ => VfsError::Io {
                action,
                path: path.clone(),
                source: io::Error::other(self.to_string()),
            },
        }
    }
}

/// Map the server's own words onto the three failures the file pane draws
/// differently. SFTP status codes arrive as text through `russh-sftp`, so this
/// is deliberately conservative: anything unrecognised stays an I/O error with
/// the server's message intact rather than being guessed at.
fn classify(message: &str) -> std::io::ErrorKind {
    use std::io::ErrorKind;
    let lower = message.to_ascii_lowercase();
    if lower.contains("no such file") || lower.contains("nosuchfile") {
        ErrorKind::NotFound
    } else if lower.contains("permission denied") || lower.contains("permissiondenied") {
        ErrorKind::PermissionDenied
    } else if lower.contains("already exists")
        || lower.contains("failure") && lower.contains("exist")
    {
        ErrorKind::AlreadyExists
    } else {
        ErrorKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_error_keeps_its_path_and_its_words() {
        let err = RemoteError::Server {
            path: "/srv/data".into(),
            message: "Permission denied".into(),
        };
        let vfs = err.into_vfs(
            "read",
            &VfsPath::remote(ferret_vfs::MountId(1), "/srv/data"),
        );
        let message = vfs.to_string();
        assert!(message.contains("/srv/data"), "{message}");
        assert!(
            matches!(vfs, VfsError::PermissionDenied { .. }),
            "{message}"
        );
    }

    #[test]
    fn an_unrecognised_server_message_survives_intact() {
        let err = RemoteError::Server {
            path: "/srv/x".into(),
            message: "quota exceeded on volume 3".into(),
        };
        let vfs = err.into_vfs("write", &VfsPath::remote(ferret_vfs::MountId(1), "/srv/x"));
        let message = vfs.to_string();
        // The verb, the path and the server's own words all reach the user.
        assert!(message.contains("write"), "{message}");
        assert!(message.contains("/srv/x"), "{message}");
        assert!(message.contains("quota exceeded"), "{message}");
    }

    #[test]
    fn a_changed_host_key_names_the_file_to_inspect() {
        let err = RemoteError::HostKeyMismatch {
            host: "build-box".into(),
            port: 22,
            fingerprint: "SHA256:abc".into(),
            file: PathBuf::from("/home/k/.ssh/known_hosts"),
        };
        let message = err.to_string();
        assert!(message.contains("build-box"), "{message}");
        assert!(message.contains("known_hosts"), "{message}");
        assert!(err.is_fatal_to_session());
    }
}
