//! One saved connection.
//!
//! The record holds everything needed to dial a server and *nothing* that is
//! secret: a password or a key passphrase lives in the OS keychain and is
//! referenced from here by id (see [`crate::secrets`]).

use ferret_vfs::MountId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// A stable identifier for one saved connection.
///
/// Not the display name — names are edited, and a rename must not orphan the
/// record's keychain entry or its remembered destinations.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectionId(pub String);

impl ConnectionId {
    /// Generate one from the clock and the process id.
    ///
    /// Uniqueness within one user's connection list is all that is required —
    /// these are never exchanged between machines — so this avoids a UUID
    /// dependency for a job that does not need one. Collisions are checked
    /// against the store on insert regardless.
    pub fn generate() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self(format!("{nanos:x}-{:x}", std::process::id()))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A saved SSH connection, as it appears in the sidebar and in
/// `connections.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Connection {
    pub id: ConnectionId,

    /// The mount id every path into this server carries.
    ///
    /// Persisted rather than handed out per launch, and this is not a
    /// convenience: `VfsPath::preference_key()` renders a remote path as
    /// `{mount}:{path}`, so a counter that restarted with the process would
    /// hand one server the per-folder view preferences recorded for another.
    /// Allocated as `max + 1` by the store and never reused.
    pub mount: MountId,

    /// The label shown in the sidebar. Free text; may be empty, in which case
    /// the UI falls back to `user@host`.
    pub name: String,

    /// Where the in-process SFTP client dials: a host name or IP address.
    ///
    /// Resolved at import time, because that client opens a TCP connection and
    /// cannot look an alias up in the user's config.
    pub host: String,

    /// The `~/.ssh/config` alias this record came from, if it came from one.
    ///
    /// Kept because `host` is the *resolved* address, and handing that to
    /// `ssh` would not match the `Host` block it came from — so `ProxyJump`,
    /// `Match`, certificates and every other setting in that block would be
    /// silently bypassed by the interactive terminal, which is the one half of
    /// this feature that promises to honour them (D-07).
    ///
    /// `None` for a connection typed into the dialog, which has no block to
    /// match and nothing to lose.
    pub ssh_alias: Option<String>,

    pub user: String,

    /// `None` means 22, and is stored as absent so the common case does not
    /// clutter the file.
    pub port: Option<u16>,

    /// An explicit private key. `None` means agent first, then the standard
    /// identity files — which is what "leave blank for default" promises.
    pub key_path: Option<PathBuf>,

    /// Free text, shown in the row's tooltip. Never sent anywhere.
    pub notes: String,

    /// Where on the server to land. `None` means the account's home
    /// directory, resolved with `realpath(".")` at mount time.
    pub initial_path: Option<String>,

    /// Whether a password or passphrase for this connection is kept in the OS
    /// keychain.
    ///
    /// A *flag*, never the secret. Its only job is letting the UI show
    /// "remembered" without a keychain round trip on every render, and letting
    /// record deletion know there is something to clean up.
    pub remember_secret: bool,

    /// F-23. Never persisted as enabled — see [`Connection::sanitised`].
    pub mount_as_root: bool,

    /// Keys written by a newer Ferret, kept verbatim so an older build cannot
    /// silently strip them. Same rule the settings file follows (D-21).
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            id: ConnectionId(String::new()),
            mount: MountId(0),
            name: String::new(),
            host: String::new(),
            ssh_alias: None,
            user: String::new(),
            port: None,
            key_path: None,
            notes: String::new(),
            initial_path: None,
            remember_secret: false,
            mount_as_root: false,
            unknown: Map::new(),
        }
    }
}

impl Connection {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(22)
    }

    /// What the sidebar row is labelled.
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            self.endpoint()
        } else {
            self.name.clone()
        }
    }

    /// `user@host`, or just the host when no user is set.
    pub fn endpoint(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// The `ssh` command line this connection corresponds to.
    ///
    /// Shown live in the dialog with a copy button, and — this is the point —
    /// it is the same string F-13 spawns. If the two could diverge, the copy
    /// button would be lying about what Ferret does.
    pub fn ssh_command(&self) -> String {
        let mut parts = vec!["ssh".to_string()];
        for arg in self.ssh_args() {
            parts.push(shell_quote(&arg));
        }
        parts.join(" ")
    }

    /// The argument vector behind [`Connection::ssh_command`], for spawning.
    ///
    /// Derived from the same fields in the same order, so what the terminal
    /// runs and what the copy button shows cannot drift apart.
    pub fn ssh_args(&self) -> Vec<String> {
        // An alias-backed record hands `ssh` the alias and nothing else: the
        // user's own `Host` block owns the user, port, key, jump host and
        // everything else in it, and that is the entire reason the alias was
        // kept. Passing Ferret's copy of those as flags would override the
        // block with values that go stale the moment the user edits their
        // config.
        if let Some(alias) = &self.ssh_alias {
            return vec![alias.clone()];
        }

        let mut args = Vec::new();
        if let Some(port) = self.port
            && port != 22
        {
            args.push("-p".into());
            args.push(port.to_string());
        }
        if let Some(key) = &self.key_path {
            args.push("-i".into());
            args.push(key.to_string_lossy().into_owned());
        }
        args.push(self.endpoint());
        args
    }

    /// Why this record cannot be saved, if it cannot.
    ///
    /// Validation on Save rather than on every keystroke: a field that turns
    /// red while it is still being typed teaches people to ignore it.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.host.trim().is_empty() {
            return Err("A connection needs a host.");
        }
        if self.host.contains(char::is_whitespace) {
            return Err("A host name can't contain spaces.");
        }
        if self.user.contains(char::is_whitespace) {
            return Err("A user name can't contain spaces.");
        }
        if self.port == Some(0) {
            return Err("Port 0 isn't a port Ferret can connect to.");
        }
        Ok(())
    }

    /// The form of this record that is safe to write to disk.
    ///
    /// Root mounts are never persisted as enabled: F-23 requires that the
    /// switch cannot come back on from restored state, and the cheapest way to
    /// guarantee that is for the file to be incapable of expressing it.
    pub fn sanitised(&self) -> Self {
        Self {
            mount_as_root: false,
            ..self.clone()
        }
    }
}

/// Single-quote a value for a POSIX shell, the same way the terminal panel
/// does when it hands a path to a shell.
fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Connection {
        Connection {
            id: ConnectionId("abc".into()),
            mount: MountId(1),
            name: "build-box".into(),
            host: "myserver".into(),
            user: "dorien".into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_command_matches_the_arguments_that_will_be_spawned() {
        let mut record = sample();
        record.port = Some(2222);
        record.key_path = Some(PathBuf::from("/home/d/.ssh/id_ed25519"));

        // The copy button and the spawned process must agree, or the dialog is
        // documenting something Ferret does not do.
        let rendered = record.ssh_command();
        let spawned = format!("ssh {}", record.ssh_args().join(" "));
        assert_eq!(rendered, spawned);
        assert_eq!(
            rendered,
            "ssh -p 2222 -i /home/d/.ssh/id_ed25519 dorien@myserver"
        );
    }

    #[test]
    fn the_default_port_stays_out_of_the_command() {
        let mut record = sample();
        record.port = Some(22);
        assert_eq!(record.ssh_command(), "ssh dorien@myserver");
    }

    #[test]
    fn an_awkward_key_path_is_quoted() {
        let mut record = sample();
        record.key_path = Some(PathBuf::from("/home/d/my keys/id_rsa"));
        assert!(record.ssh_command().contains("'/home/d/my keys/id_rsa'"));
    }

    #[test]
    fn a_root_mount_never_reaches_the_file() {
        let mut record = sample();
        record.mount_as_root = true;
        // F-23's guarantee: the switch cannot come back on by itself, because
        // what is written can't say that it was on.
        assert!(!record.sanitised().mount_as_root);
    }

    #[test]
    fn validation_names_what_is_wrong() {
        let mut record = sample();
        record.host = String::new();
        assert!(record.validate().is_err());

        record.host = "my server".into();
        assert!(record.validate().is_err());

        record.host = "myserver".into();
        assert!(record.validate().is_ok());
    }

    #[test]
    fn an_unnamed_connection_falls_back_to_the_endpoint() {
        let mut record = sample();
        record.name = "  ".into();
        assert_eq!(record.display_name(), "dorien@myserver");
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        // A file written by a newer Ferret must not be silently stripped by an
        // older one — the same rule the settings file follows.
        let json = r#"{"id":"abc","mount":3,"host":"h","user":"u","future_key":42}"#;
        let record: Connection = serde_json::from_str(json).unwrap();
        assert_eq!(record.mount, MountId(3));
        let written = serde_json::to_string(&record).unwrap();
        assert!(written.contains("future_key"), "{written}");
    }
}
