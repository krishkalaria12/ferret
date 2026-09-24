//! Host-key verification (R-D08).
//!
//! Three outcomes, and the difference between them is the whole of SSH's
//! security model:
//!
//! * **Known** — the offered key matches `known_hosts`. Connect.
//! * **Unknown** — no entry for this host. Show the fingerprint and ask. The
//!   answer is never assumed.
//! * **Changed** — an entry exists and does not match. Refuse. This is either
//!   a rekeyed server or an interception, and nothing here can tell the
//!   difference, so it does not try to.
//!
//! Verification happens *before* any authentication attempt, so a password or
//! an agent signature is never offered to a host that has not been identified.

use crate::{RemoteError, Result};
use russh::keys::{PublicKey, known_hosts};
use std::path::PathBuf;

/// What `known_hosts` says about a key the server just offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyVerdict {
    Known,
    Unknown {
        fingerprint: String,
        algorithm: String,
    },
    Changed {
        fingerprint: String,
        file: PathBuf,
    },
}

/// The user's `known_hosts`, on both targets.
pub fn known_hosts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}

/// The SHA256 fingerprint in the form OpenSSH prints, so it can be compared
/// against what an administrator published without transcribing between
/// formats.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(Default::default()).to_string()
}

/// Check an offered key against the user's `known_hosts`.
///
/// A host with a non-standard port is recorded by OpenSSH as `[host]:port`,
/// and `russh` handles that spelling itself — passing the bare host for a
/// non-22 port would silently look up the wrong entry.
pub fn verify(host: &str, port: u16, key: &PublicKey) -> Result<HostKeyVerdict> {
    verify_at(host, port, key, known_hosts_path().as_deref())
}

pub(crate) fn verify_at(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: Option<&std::path::Path>,
) -> Result<HostKeyVerdict> {
    let checked = match path {
        Some(path) => known_hosts::check_known_hosts_path(host, port, key, path),
        None => known_hosts::check_known_hosts(host, port, key),
    };
    match checked {
        Ok(true) => Ok(HostKeyVerdict::Known),
        // `false` means no key with the offered algorithm is recorded for the
        // host. A recorded key of that algorithm with different bytes is the
        // distinct `KeyChanged` error below.
        Ok(false) => Ok(HostKeyVerdict::Unknown {
            fingerprint: fingerprint(key),
            algorithm: key.algorithm().to_string(),
        }),
        Err(russh::keys::Error::KeyChanged { .. }) => Ok(HostKeyVerdict::Changed {
            fingerprint: fingerprint(key),
            file: path
                .map(std::path::Path::to_path_buf)
                .or_else(known_hosts_path)
                .unwrap_or_else(|| PathBuf::from("~/.ssh/known_hosts")),
        }),
        Err(err) => Err(RemoteError::KnownHosts {
            file: path
                .map(std::path::Path::to_path_buf)
                .or_else(known_hosts_path)
                .unwrap_or_default(),
            message: err.to_string(),
        }),
    }
}

/// Record a key the user explicitly accepted.
///
/// Only ever called from the accept button of the fingerprint dialog. There is
/// no path that learns a key without a person having read it.
pub fn learn(host: &str, port: u16, key: &PublicKey) -> Result<()> {
    learn_at(host, port, key, known_hosts_path().as_deref())
}

pub(crate) fn learn_at(
    host: &str,
    port: u16,
    key: &PublicKey,
    path: Option<&std::path::Path>,
) -> Result<()> {
    let learned = match path {
        Some(path) => known_hosts::learn_known_hosts_path(host, port, key, path),
        None => known_hosts::learn_known_hosts(host, port, key),
    };
    learned.map_err(|err| RemoteError::KnownHosts {
        file: path
            .map(std::path::Path::to_path_buf)
            .or_else(known_hosts_path)
            .unwrap_or_default(),
        message: err.to_string(),
    })
}

/// Turn a verdict into the error the session reports, or `Ok(())` to proceed.
pub fn into_result(verdict: HostKeyVerdict, host: &str, port: u16) -> Result<()> {
    match verdict {
        HostKeyVerdict::Known => Ok(()),
        HostKeyVerdict::Unknown {
            fingerprint,
            algorithm,
        } => Err(RemoteError::UnknownHostKey {
            host: host.to_string(),
            port,
            fingerprint,
            algorithm,
        }),
        HostKeyVerdict::Changed { fingerprint, file } => Err(RemoteError::HostKeyMismatch {
            host: host.to_string(),
            port,
            fingerprint,
            file,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_host_is_a_question_and_a_changed_key_is_a_refusal() {
        // The distinction the whole module exists for: one of these is
        // answerable by a person, the other is not offered as a choice at all.
        let unknown = HostKeyVerdict::Unknown {
            fingerprint: "SHA256:aaa".into(),
            algorithm: "ssh-ed25519".into(),
        };
        let err = into_result(unknown, "build-box", 22).unwrap_err();
        assert!(matches!(err, RemoteError::UnknownHostKey { .. }), "{err}");
        assert!(!err.is_fatal_to_session(), "the user can still say yes");

        let changed = HostKeyVerdict::Changed {
            fingerprint: "SHA256:bbb".into(),
            file: PathBuf::from("/home/k/.ssh/known_hosts"),
        };
        let err = into_result(changed, "build-box", 22).unwrap_err();
        assert!(matches!(err, RemoteError::HostKeyMismatch { .. }), "{err}");
        assert!(
            err.is_fatal_to_session(),
            "there is no continue-anyway path"
        );
        // The message has to say where to look, or it is unactionable.
        assert!(err.to_string().contains("known_hosts"), "{err}");
    }

    #[test]
    fn a_known_host_proceeds() {
        assert!(into_result(HostKeyVerdict::Known, "h", 22).is_ok());
    }
}
