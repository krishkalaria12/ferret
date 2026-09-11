//! The only module in Ferret that touches a secret.
//!
//! Passwords and key passphrases live in the OS credential store — Keychain on
//! macOS, Credential Manager on Windows — and never in a file Ferret writes.
//! The connection record holds a *flag* saying a secret exists, never the
//! secret itself.

use crate::config::KEYCHAIN_SERVICE;
use crate::record::ConnectionId;
use crate::{RemoteError, Result};

/// A secret in transit, which refuses to print itself.
///
/// `Debug` is implemented by hand precisely so a panic, a log line or a
/// `dbg!` left in by accident cannot put a password in a backtrace.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// What a stored secret is for. One connection can remember both a key
/// passphrase and an account password without them colliding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    Password,
    KeyPassphrase,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::KeyPassphrase => "passphrase",
        }
    }
}

fn entry(id: &ConnectionId, kind: SecretKind) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("{id}:{}", kind.suffix())).map_err(|err| {
        RemoteError::Keychain {
            label: format!("{id}"),
            message: err.to_string(),
        }
    })
}

/// Read a remembered secret, if there is one.
///
/// A missing entry is `Ok(None)`, not an error: "nothing remembered" is the
/// ordinary case and must not look like a failure.
pub fn load(id: &ConnectionId, kind: SecretKind) -> Result<Option<Secret>> {
    match entry(id, kind)?.get_password() {
        Ok(value) => Ok(Some(Secret::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(RemoteError::Keychain {
            label: format!("{id}"),
            message: err.to_string(),
        }),
    }
}

/// Remember a secret. Only ever called after an explicit opt-in.
pub fn store(id: &ConnectionId, kind: SecretKind, secret: &Secret) -> Result<()> {
    entry(id, kind)?
        .set_password(secret.expose())
        .map_err(|err| RemoteError::Keychain {
            label: format!("{id}"),
            message: err.to_string(),
        })
}

/// Forget a secret. Deleting a connection calls this for both kinds, so no
/// credential outlives the record that explained what it was for.
pub fn forget(id: &ConnectionId, kind: SecretKind) -> Result<()> {
    match entry(id, kind)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(RemoteError::Keychain {
            label: format!("{id}"),
            message: err.to_string(),
        }),
    }
}

/// Forget everything stored for a connection.
pub fn forget_all(id: &ConnectionId) {
    // Best effort by design: failing to clear a keychain entry must not stop
    // the user from deleting a connection they no longer want.
    let _ = forget(id, SecretKind::Password);
    let _ = forget(id, SecretKind::KeyPassphrase);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new("hunter2");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");

        // And it must stay redacted inside a container, which is how it would
        // actually reach a backtrace.
        let wrapped = format!("{:?}", Some(secret.clone()));
        assert!(!wrapped.contains("hunter2"), "{wrapped}");
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn the_two_kinds_do_not_collide() {
        let id = ConnectionId("abc".into());
        // The account name is what keeps a password and a passphrase apart in
        // the same keychain service.
        assert_ne!(
            format!("{id}:{}", SecretKind::Password.suffix()),
            format!("{id}:{}", SecretKind::KeyPassphrase.suffix())
        );
    }
}
