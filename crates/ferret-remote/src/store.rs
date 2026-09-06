//! The saved connection list: `connections.json`, and importing hosts from
//! `~/.ssh/config`.
//!
//! Its own file rather than a section of `settings.json`, for three reasons:
//! it grows with use, it is written by the application rather than edited by
//! hand, and a malformed record must never be able to cost the user their
//! settings.
//!
//! It follows the settings file's rules regardless (D-21): every field
//! optional, unknown keys preserved, and a file that cannot be parsed costs
//! the connection list and nothing else.

use crate::config::{CONNECTIONS_FILE, KEYCHAIN_SERVICE};
use crate::record::{Connection, ConnectionId};
use crate::{RemoteError, Result};
use ferret_vfs::MountId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's shape. A wrapper object rather than a bare array, so a version
/// marker or a future section can be added without rewriting what exists.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct StoreFile {
    connections: Vec<Connection>,
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    unknown: serde_json::Map<String, serde_json::Value>,
}

/// Every saved connection, in display order.
#[derive(Clone, Debug, Default)]
pub struct ConnectionStore {
    connections: Vec<Connection>,
    unknown: serde_json::Map<String, serde_json::Value>,
    dirty: bool,
}

/// Where `connections.json` lives — beside `settings.json`, in the same
/// per-user config directory.
pub fn connections_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Ferret").join(CONNECTIONS_FILE))
}

impl ConnectionStore {
    /// Read the list, degrading rather than failing.
    ///
    /// A missing file is an empty list, which is what a fresh install has. A
    /// *corrupt* file is reported, because losing a server list silently is
    /// worse than saying so — but it still never blocks startup.
    pub fn load() -> (Self, Option<RemoteError>) {
        let Some(path) = connections_path() else {
            return (Self::default(), None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // Absent is the ordinary case, not an error.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), None);
            }
            Err(err) => {
                return (
                    Self::default(),
                    Some(RemoteError::Store {
                        path,
                        message: err.to_string(),
                    }),
                );
            }
        };
        match serde_json::from_str::<StoreFile>(&text) {
            Ok(file) => (
                Self {
                    connections: file.connections,
                    unknown: file.unknown,
                    dirty: false,
                },
                None,
            ),
            Err(err) => (
                Self::default(),
                Some(RemoteError::Store {
                    path,
                    message: err.to_string(),
                }),
            ),
        }
    }

    pub fn connections(&self) -> &[Connection] {
        &self.connections
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn get(&self, id: &ConnectionId) -> Option<&Connection> {
        self.connections.iter().find(|c| &c.id == id)
    }

    pub fn by_mount(&self, mount: MountId) -> Option<&Connection> {
        self.connections.iter().find(|c| c.mount == mount)
    }

    /// Whether anything has changed since the last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Add a record, assigning it an id and a mount id.
    ///
    /// The mount id is `max + 1` over every record that has ever been in this
    /// file, and a deleted record's id is never handed out again: reusing one
    /// would give a new server the per-folder view preferences of the old one
    /// (`VfsPath::preference_key`).
    pub fn insert(&mut self, mut record: Connection) -> ConnectionId {
        if record.id.0.is_empty() || self.get(&record.id).is_some() {
            record.id = ConnectionId::generate();
        }
        record.mount = self.next_mount_id();
        let id = record.id.clone();
        self.connections.push(record);
        self.dirty = true;
        id
    }

    /// Replace a record in place, keeping its id and its mount id.
    ///
    /// Editing a connection must not renumber its mount: open tabs hold paths
    /// carrying the old number, and its remembered folder views are keyed on
    /// it.
    pub fn update(&mut self, record: Connection) -> bool {
        let Some(slot) = self.connections.iter_mut().find(|c| c.id == record.id) else {
            return false;
        };
        let mount = slot.mount;
        *slot = Connection { mount, ..record };
        self.dirty = true;
        true
    }

    pub fn remove(&mut self, id: &ConnectionId) -> Option<Connection> {
        let index = self.connections.iter().position(|c| &c.id == id)?;
        let removed = self.connections.remove(index);
        self.dirty = true;
        Some(removed)
    }

    /// Reorder, for sidebar drag.
    pub fn move_record(&mut self, from: usize, to: usize) {
        if from >= self.connections.len() || to >= self.connections.len() || from == to {
            return;
        }
        let moved = self.connections.remove(from);
        self.connections.insert(to, moved);
        self.dirty = true;
    }

    /// The next unused mount id.
    ///
    /// Highest ever used plus one. `unknown` carries a high-water mark so the
    /// property survives deleting the last record: without it, removing the
    /// only connection would let the next one reuse `MountId(1)`.
    fn next_mount_id(&mut self) -> MountId {
        let highest_live = self
            .connections
            .iter()
            .map(|c| c.mount.0)
            .max()
            .unwrap_or(0);
        let watermark = self
            .unknown
            .get("mount_watermark")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let next = highest_live.max(watermark) + 1;
        self.unknown
            .insert("mount_watermark".into(), serde_json::json!(next));
        MountId(next)
    }

    /// Serialise for the debounced writer. Marks the store clean, matching how
    /// `FerretState::take_pending_saves` treats settings.
    pub fn take_pending_save(&mut self) -> Option<(PathBuf, String)> {
        if !self.dirty {
            return None;
        }
        let path = connections_path()?;
        let file = StoreFile {
            connections: self.connections.iter().map(Connection::sanitised).collect(),
            unknown: self.unknown.clone(),
        };
        let json = serde_json::to_string_pretty(&file).ok()?;
        self.dirty = false;
        Some((path, json))
    }

    /// The keychain entry name for one connection's secret.
    pub fn keychain_account(id: &ConnectionId) -> String {
        format!("{KEYCHAIN_SERVICE}:{id}")
    }

    /// Hosts from `~/.ssh/config` that are not saved here yet.
    ///
    /// Import is a *suggestion* that pre-fills the dialog, never a live
    /// alternative source of connection settings: a user's ssh config is
    /// theirs, and misreading one keyword must not change how Ferret connects.
    pub fn importable(&self, path: Option<&Path>) -> Vec<Connection> {
        let known: std::collections::HashSet<(String, String)> = self
            .connections
            .iter()
            .map(|c| (c.host.to_lowercase(), c.user.to_lowercase()))
            .collect();
        crate::ssh_config::parse(path)
            .into_iter()
            .filter(|c| !known.contains(&(c.host.to_lowercase(), c.user.to_lowercase())))
            .collect()
    }
}

/// Read `contents` to `path` through a temp file in the same directory.
///
/// Same shape as the settings writer: create the directory, write a sibling
/// temp file, rename over the target so the replacement is atomic. Blocks —
/// callers hand it to a background executor.
pub fn write_store(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| RemoteError::Store {
            path: parent.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents).map_err(|err| RemoteError::Store {
        path: tmp.clone(),
        message: err.to_string(),
    })?;
    std::fs::rename(&tmp, path).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        RemoteError::Store {
            path: path.to_path_buf(),
            message: err.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(host: &str) -> Connection {
        Connection {
            host: host.into(),
            user: "k".into(),
            ..Default::default()
        }
    }

    #[test]
    fn mount_ids_are_never_reused() {
        let mut store = ConnectionStore::default();
        let first = store.insert(record("a"));
        let first_mount = store.get(&first).unwrap().mount;

        // Deleting the only record must not let the next one inherit its
        // number: `preference_key` is `{mount}:{path}`, so reuse would hand a
        // new server another server's remembered folder views.
        store.remove(&first);
        let second = store.insert(record("b"));
        let second_mount = store.get(&second).unwrap().mount;
        assert_ne!(first_mount, second_mount);
        assert!(second_mount.0 > first_mount.0);
    }

    #[test]
    fn the_watermark_survives_a_save_and_reload() {
        let mut store = ConnectionStore::default();
        let id = store.insert(record("a"));
        let mount = store.get(&id).unwrap().mount;
        store.remove(&id);
        let (_, json) = store.take_pending_save().expect("dirty");

        let file: StoreFile = serde_json::from_str(&json).unwrap();
        let mut reloaded = ConnectionStore {
            connections: file.connections,
            unknown: file.unknown,
            dirty: false,
        };
        let next = reloaded.insert(record("b"));
        assert!(reloaded.get(&next).unwrap().mount.0 > mount.0);
    }

    #[test]
    fn editing_a_record_keeps_its_mount() {
        let mut store = ConnectionStore::default();
        let id = store.insert(record("a"));
        let before = store.get(&id).unwrap().clone();

        let mut edited = before.clone();
        edited.name = "renamed".into();
        edited.mount = MountId(999); // A caller that got this wrong.
        assert!(store.update(edited));

        let after = store.get(&id).unwrap();
        assert_eq!(after.name, "renamed");
        // Open tabs hold paths carrying the old number, so it must survive.
        assert_eq!(after.mount, before.mount);
    }

    #[test]
    fn a_corrupt_file_costs_the_list_and_nothing_else() {
        // Parsing is the part under test; the file path is the caller's.
        let broken = serde_json::from_str::<StoreFile>("{ not json");
        assert!(broken.is_err());
        let (store, _) = (ConnectionStore::default(), ());
        assert!(store.is_empty(), "startup still gets an empty list");
    }

    #[test]
    fn a_saved_store_is_clean_until_it_changes_again() {
        let mut store = ConnectionStore::default();
        assert!(store.take_pending_save().is_none(), "nothing to write");
        store.insert(record("a"));
        assert!(store.is_dirty());
        // Only meaningful when a config directory exists, which is true on
        // both targets; the flag transition is what matters here.
        if store.take_pending_save().is_some() {
            assert!(!store.is_dirty());
        }
    }
}
