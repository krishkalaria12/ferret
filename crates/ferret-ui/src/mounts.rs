//! What is connected right now (R-4).
//!
//! The [`Router`] answers *where a path goes*; this answers *what the sidebar
//! draws and what the browser may navigate to*. They are deliberately separate:
//! the router is a storage concern that knows nothing about connection
//! records, and this is a UI concern that knows nothing about SFTP.
//!
//! This is also what keeps a mount alive. A [`Session`] owns the command
//! channel into the runtime actor that holds the SSH handle, so dropping the
//! last one closes the connection — which means the session has to live
//! somewhere with the application's lifetime rather than on a view that a tab
//! close could take with it.
//!
//! [`Router`]: ferret_vfs::Router

use std::collections::HashMap;
use std::sync::Arc;

use ferret_remote::{Connection, ConnectionId, Session, SftpVfs};
use ferret_vfs::{MountId, VfsPath};
use gpui::SharedString;

/// What one saved connection is doing.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum MountStatus {
    /// Saved, not connected. The resting state, and the one a fresh launch
    /// starts in for every record — nothing dials itself (R-Q03).
    #[default]
    Idle,
    Connecting,
    Connected,
    /// The last attempt failed. The message is shown in the row's tooltip, so
    /// it has to be worth reading.
    Failed(SharedString),
}

impl MountStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, MountStatus::Connected)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, MountStatus::Connecting)
    }
}

struct Mount {
    status: MountStatus,
    /// Where the mount opened — the account's home directory, or the record's
    /// `initial_path`. Held so a second click navigates instead of re-dialling.
    home: Option<VfsPath>,
    /// The live connection. Dropping this closes the SSH session, so it stays
    /// here until the user disconnects or the application quits.
    session: Option<Arc<Session>>,
    /// The same backend the router holds, but concretely.
    ///
    /// The router's `Arc<dyn Vfs>` is the right shape for listing and
    /// mutation and the wrong one for a transfer, which needs handles and
    /// offsets the trait deliberately has no room for.
    backend: Option<Arc<SftpVfs>>,
    /// Whether this mount is running as root on the far side (F-23).
    ///
    /// Lives here and nowhere else. It is deliberately *not* on the saved
    /// record — `Connection::sanitised` makes the file incapable of expressing
    /// it — so it cannot survive a restart and cannot be turned on except by
    /// someone typing the host's name into the confirmation.
    as_root: bool,
}

impl Mount {
    fn idle() -> Self {
        Self {
            status: MountStatus::Idle,
            home: None,
            session: None,
            backend: None,
            as_root: false,
        }
    }
}

/// Every connection's live state, keyed by record id.
#[derive(Default)]
pub struct Mounts {
    by_id: HashMap<ConnectionId, Mount>,
}

impl Mounts {
    pub fn status(&self, id: &ConnectionId) -> MountStatus {
        self.by_id
            .get(id)
            .map(|mount| mount.status.clone())
            .unwrap_or_default()
    }

    pub fn is_connected(&self, id: &ConnectionId) -> bool {
        self.by_id
            .get(id)
            .is_some_and(|mount| mount.status.is_connected())
    }

    /// Where to navigate for an already-connected mount.
    pub fn home(&self, id: &ConnectionId) -> Option<VfsPath> {
        self.by_id
            .get(id)
            .filter(|mount| mount.status.is_connected())
            .and_then(|mount| mount.home.clone())
    }

    /// Whether any connection is currently mounted, so the UI can skip work
    /// that only remote paths need.
    pub fn any_connected(&self) -> bool {
        self.by_id.values().any(|mount| mount.status.is_connected())
    }

    /// The signal that fires when this mount's connection ends.
    ///
    /// A signal rather than the session itself, so a watcher cannot keep a
    /// connection alive past the user closing it.
    pub fn closed_signal(&self, id: &ConnectionId) -> Option<ferret_remote::ClosedSignal> {
        self.by_id
            .get(id)
            .filter(|mount| mount.status.is_connected())
            .and_then(|mount| mount.session.as_ref())
            .map(|session| session.closed_signal())
    }

    /// The backend a transfer needs, or `None` when nothing is mounted.
    pub fn backend(&self, id: &ConnectionId) -> Option<Arc<SftpVfs>> {
        self.by_id
            .get(id)
            .filter(|mount| mount.status.is_connected())
            .and_then(|mount| mount.backend.clone())
    }

    pub(crate) fn begin(&mut self, id: &ConnectionId) {
        let mount = self.by_id.entry(id.clone()).or_insert_with(Mount::idle);
        mount.status = MountStatus::Connecting;
    }

    pub(crate) fn finish(
        &mut self,
        id: &ConnectionId,
        session: Arc<Session>,
        backend: Arc<SftpVfs>,
        home: VfsPath,
        as_root: bool,
    ) {
        self.by_id.insert(
            id.clone(),
            Mount {
                status: MountStatus::Connected,
                home: Some(home),
                session: Some(session),
                backend: Some(backend),
                as_root,
            },
        );
    }

    /// Whether this mount is running as root (F-23).
    ///
    /// Drawn wherever the mount is: a window that can delete anything on a
    /// server must never look like one that cannot.
    pub fn is_root(&self, id: &ConnectionId) -> bool {
        self.by_id
            .get(id)
            .is_some_and(|mount| mount.as_root && mount.status.is_connected())
    }

    /// Whether the mount behind a path is running as root, for the path bar,
    /// which has an id for the mount but not for the connection.
    pub fn mount_is_root(&self, mount: MountId, connections: &[Connection]) -> bool {
        connection_for_mount(connections, mount).is_some_and(|record| self.is_root(&record.id))
    }

    pub(crate) fn fail(&mut self, id: &ConnectionId, reason: impl Into<SharedString>) {
        let mount = self.by_id.entry(id.clone()).or_insert_with(Mount::idle);
        mount.status = MountStatus::Failed(reason.into());
        mount.home = None;
        // Dropping the session closes the connection, which is what a failure
        // means: there is nothing left to reuse.
        mount.session = None;
        mount.backend = None;
        // A failed root mount is not a root mount. Leaving this set would draw
        // the marker on a row that is no longer privileged.
        mount.as_root = false;
    }

    /// Forget a mount, returning its session so the caller can close it
    /// politely rather than only dropping it.
    pub(crate) fn take(&mut self, id: &ConnectionId) -> Option<Arc<Session>> {
        self.by_id.remove(id).and_then(|mount| mount.session)
    }

    /// Every connected mount id, for disconnecting everything at quit.
    pub(crate) fn connected_ids(&self) -> Vec<ConnectionId> {
        self.by_id
            .iter()
            .filter(|(_, mount)| mount.status.is_connected())
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// A tab pointing at a mount that is no longer connected still has a valid
/// path — it just cannot be listed. The sidebar and path bar use this to name
/// the server rather than showing a bare mount number.
pub fn connection_for_mount(connections: &[Connection], mount: MountId) -> Option<&Connection> {
    connections.iter().find(|c| c.mount == mount)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> ConnectionId {
        ConnectionId("test".into())
    }

    #[test]
    fn an_unknown_connection_is_idle_rather_than_missing() {
        let mounts = Mounts::default();
        // Every saved record renders a row before it has ever been dialled, so
        // "no entry" has to be a state rather than an absence.
        assert_eq!(mounts.status(&id()), MountStatus::Idle);
        assert!(!mounts.is_connected(&id()));
        assert!(mounts.home(&id()).is_none());
    }

    #[test]
    fn a_failure_clears_the_home_path_it_used_to_have() {
        let mut mounts = Mounts::default();
        mounts.begin(&id());
        assert!(mounts.status(&id()).is_busy());

        mounts.fail(&id(), "the connection timed out");
        // A stale home path would let a click navigate into a mount that is no
        // longer routable, which surfaces as an empty pane rather than an
        // error worth reading.
        assert!(mounts.home(&id()).is_none());
        assert!(!mounts.is_connected(&id()));
        assert!(matches!(mounts.status(&id()), MountStatus::Failed(_)));
    }
}
