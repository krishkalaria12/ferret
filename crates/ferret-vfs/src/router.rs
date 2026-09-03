//! One [`Vfs`] over local disk and every mounted remote backend.
//!
//! The application holds exactly one of these and hands it to every task that
//! lists, watches, probes or mutates. Before it existed those tasks held a
//! concrete [`LocalFs`], which is why adding a remote backend is a change to
//! this file and to almost nothing else.
//!
//! Two properties are load-bearing:
//!
//! 1. **Local paths are not slowed down.** The `Local` arm calls [`LocalFs`]
//!    directly — no map lookup, no dynamic dispatch, nothing that would show up
//!    in `cargo xtask bench`'s 200,000-entry listing.
//! 2. **A two-path operation must stay on one backend.** `rename` and `copy`
//!    take a source and a destination; when those resolve to different backends
//!    the answer is [`VfsError::CrossBackend`] and the caller routes the gesture
//!    to a transfer with progress and cancel. Copying byte-by-byte through the
//!    trait would be a hidden network transfer that nobody could stop.

use crate::watch::Watch;
use crate::{
    Capabilities, Entry, ListingDetail, LocalFs, Metadata, MountId, ReadDirStream, RemoveMode,
    Result, Vfs, VfsError, VfsPath,
};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

/// What is mounted right now, keyed by the id embedded in every remote path.
///
/// Read on every remote call and written only when a mount appears or goes
/// away, which is what makes [`ArcSwap`] the right shape: readers never block
/// and never take a lock, and a mount that vanishes mid-listing leaves the
/// in-flight stream holding its own `Arc` rather than a dangling id.
type Mounts = HashMap<MountId, Arc<dyn Vfs>>;

#[derive(Clone, Default)]
pub struct Router {
    local: LocalFs,
    mounts: Arc<ArcSwap<Mounts>>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a backend under `id`, replacing any previous one.
    ///
    /// Remounting the same id is how a reconnect installs a fresh session: the
    /// paths in open tabs keep resolving, and the previous backend drops once
    /// the last in-flight call using it finishes.
    pub fn mount(&self, id: MountId, backend: Arc<dyn Vfs>) {
        self.mounts.rcu(|current| {
            let mut next = Mounts::clone(current);
            next.insert(id, Arc::clone(&backend));
            next
        });
    }

    pub fn unmount(&self, id: MountId) {
        self.mounts.rcu(|current| {
            let mut next = Mounts::clone(current);
            next.remove(&id);
            next
        });
    }

    pub fn is_mounted(&self, id: MountId) -> bool {
        self.mounts.load().contains_key(&id)
    }

    /// The backend a path belongs to, or `None` for a remote path whose mount
    /// is not connected. Local paths always resolve.
    pub fn backend(&self, path: &VfsPath) -> Option<Arc<dyn Vfs>> {
        match path {
            VfsPath::Local(_) => Some(Arc::new(self.local)),
            VfsPath::Remote { mount, .. } => self.mounts.load().get(mount).cloned(),
        }
    }

    /// What the backend holding `path` can do.
    ///
    /// Callers that draw or disable an action ask this rather than
    /// [`Vfs::capabilities`], which can only answer for local disk: whether
    /// there is a trash, a watcher or an atomic rename is a property of the
    /// folder on screen, not of the router.
    pub fn capabilities_for(&self, path: &VfsPath) -> Capabilities {
        match path {
            VfsPath::Local(_) => self.local.capabilities(),
            VfsPath::Remote { mount, .. } => self
                .mounts
                .load()
                .get(mount)
                .map(|fs| fs.capabilities())
                // An unmounted path can do nothing, which is the honest answer
                // while a tab sits waiting to reconnect.
                .unwrap_or(UNMOUNTED_CAPABILITIES),
        }
    }

    fn remote(&self, path: &VfsPath) -> Result<Arc<dyn Vfs>> {
        self.backend(path).ok_or_else(|| VfsError::NotMounted {
            path: path.clone(),
        })
    }

    /// Resolve both sides of a two-path operation, refusing a pair that spans
    /// two backends.
    fn pair(&self, from: &VfsPath, to: &VfsPath) -> Result<Arc<dyn Vfs>> {
        let same = match (from, to) {
            (VfsPath::Local(_), VfsPath::Local(_)) => true,
            (VfsPath::Remote { mount: a, .. }, VfsPath::Remote { mount: b, .. }) => a == b,
            _ => false,
        };
        if !same {
            return Err(VfsError::CrossBackend {
                from: from.clone(),
                to: to.clone(),
            });
        }
        self.remote(from)
    }
}

/// What a path whose mount is not connected reports.
///
/// Every capability false, so the UI disables rather than offering an action
/// that cannot reach a server.
const UNMOUNTED_CAPABILITIES: Capabilities = Capabilities {
    symlinks: false,
    permissions: false,
    atomic_rename: false,
    watch: false,
    metadata_in_listing: true,
    hidden_in_listing: true,
};

#[async_trait]
impl Vfs for Router {
    /// Local disk's capabilities. Anything deciding what a *folder* can do
    /// wants [`Router::capabilities_for`] instead.
    fn capabilities(&self) -> Capabilities {
        self.local.capabilities()
    }

    fn read_dir(&self, dir: &VfsPath, detail: ListingDetail) -> ReadDirStream {
        match dir {
            VfsPath::Local(_) => self.local.read_dir(dir, detail),
            VfsPath::Remote { .. } => match self.backend(dir) {
                Some(fs) => fs.read_dir(dir, detail),
                None => {
                    let err = VfsError::NotMounted { path: dir.clone() };
                    Box::pin(futures::stream::once(async move { Err(err) }))
                }
            },
        }
    }

    async fn metadata(&self, path: &VfsPath) -> Result<Metadata> {
        match path {
            VfsPath::Local(_) => self.local.metadata(path).await,
            VfsPath::Remote { .. } => self.remote(path)?.metadata(path).await,
        }
    }

    async fn metadata_batch(&self, paths: Vec<VfsPath>) -> Vec<(VfsPath, Result<Metadata>)> {
        // Nearly every batch is one backend's worth of paths — they come from
        // one directory listing — so the common case dispatches once rather
        // than grouping.
        match paths.first() {
            None => Vec::new(),
            Some(VfsPath::Local(_)) if paths.iter().all(VfsPath::is_local) => {
                self.local.metadata_batch(paths).await
            }
            Some(first) => match self.backend(first) {
                Some(fs) if paths.iter().all(|p| same_backend(first, p)) => {
                    fs.metadata_batch(paths).await
                }
                // Mixed or unmounted: fall back to one call each, which the
                // per-path dispatch above already handles correctly.
                _ => {
                    let mut resolved = Vec::with_capacity(paths.len());
                    for path in paths {
                        let result = self.metadata(&path).await;
                        resolved.push((path, result));
                    }
                    resolved
                }
            },
        }
    }

    async fn probe(&self, path: &VfsPath) -> Result<Entry> {
        match path {
            VfsPath::Local(_) => self.local.probe(path).await,
            VfsPath::Remote { .. } => self.remote(path)?.probe(path).await,
        }
    }

    async fn exists(&self, path: &VfsPath) -> bool {
        match self.backend(path) {
            Some(fs) => fs.exists(path).await,
            None => false,
        }
    }

    async fn canonicalize(&self, path: &VfsPath) -> Result<VfsPath> {
        match path {
            VfsPath::Local(_) => self.local.canonicalize(path).await,
            VfsPath::Remote { .. } => self.remote(path)?.canonicalize(path).await,
        }
    }

    async fn read(&self, path: &VfsPath, budget: usize) -> Result<Vec<u8>> {
        match path {
            VfsPath::Local(_) => self.local.read(path, budget).await,
            VfsPath::Remote { .. } => self.remote(path)?.read(path, budget).await,
        }
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<()> {
        match path {
            VfsPath::Local(_) => self.local.create_dir(path).await,
            VfsPath::Remote { .. } => self.remote(path)?.create_dir(path).await,
        }
    }

    async fn create_file(&self, path: &VfsPath) -> Result<()> {
        match path {
            VfsPath::Local(_) => self.local.create_file(path).await,
            VfsPath::Remote { .. } => self.remote(path)?.create_file(path).await,
        }
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.pair(from, to)?.rename(from, to).await
    }

    async fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.pair(from, to)?.copy(from, to).await
    }

    fn watch(&self, dir: &VfsPath) -> Result<Watch> {
        match dir {
            VfsPath::Local(_) => self.local.watch(dir),
            // No remote backend has change notification, so this is the trait's
            // `Unsupported` rather than a lookup that would only fail later.
            VfsPath::Remote { .. } => Err(VfsError::Unsupported {
                capability: "change notification",
            }),
        }
    }

    async fn remove(&self, path: &VfsPath, mode: RemoveMode) -> Result<()> {
        match path {
            VfsPath::Local(_) => self.local.remove(path, mode).await,
            VfsPath::Remote { .. } => self.remote(path)?.remove(path, mode).await,
        }
    }
}

fn same_backend(a: &VfsPath, b: &VfsPath) -> bool {
    match (a, b) {
        (VfsPath::Local(_), VfsPath::Local(_)) => true,
        (VfsPath::Remote { mount: x, .. }, VfsPath::Remote { mount: y, .. }) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DirEntryBatch, EntryKind};
    use futures::StreamExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A backend that records what it was asked and answers from memory, so a
    /// routing test needs no server and no disk.
    #[derive(Default)]
    struct FakeFs {
        listings: AtomicUsize,
    }

    #[async_trait]
    impl Vfs for FakeFs {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                symlinks: true,
                permissions: true,
                atomic_rename: false,
                watch: false,
                metadata_in_listing: true,
                hidden_in_listing: true,
            }
        }

        fn read_dir(&self, dir: &VfsPath, _detail: ListingDetail) -> ReadDirStream {
            self.listings.fetch_add(1, Ordering::Relaxed);
            let entry = Entry::new(dir.join("remote.txt"), "remote.txt", EntryKind::File);
            Box::pin(futures::stream::once(async move {
                Ok(DirEntryBatch::from(vec![entry]))
            }))
        }

        async fn metadata(&self, _path: &VfsPath) -> Result<Metadata> {
            Ok(Metadata::default())
        }

        async fn canonicalize(&self, path: &VfsPath) -> Result<VfsPath> {
            Ok(path.clone())
        }

        async fn read(&self, _path: &VfsPath, _budget: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn create_dir(&self, _path: &VfsPath) -> Result<()> {
            Ok(())
        }

        async fn rename(&self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
            Ok(())
        }

        async fn remove(&self, _path: &VfsPath, _mode: RemoveMode) -> Result<()> {
            Ok(())
        }
    }

    fn remote(path: &str) -> VfsPath {
        VfsPath::remote(MountId(7), path)
    }

    #[test]
    fn a_remote_path_with_no_mount_is_an_error_not_a_panic() {
        let router = Router::new();
        let mut stream = router.read_dir(&remote("/srv"), ListingDetail::Full);
        let first = smol::block_on(stream.next()).expect("one item");
        assert!(
            matches!(first, Err(VfsError::NotMounted { .. })),
            "{first:?}"
        );

        // And the same for the async methods, which is what a tab restored
        // from a previous session hits before its server is dialled.
        let err = smol::block_on(router.metadata(&remote("/srv/x"))).unwrap_err();
        assert!(matches!(err, VfsError::NotMounted { .. }), "{err}");
    }

    #[test]
    fn mounting_routes_remote_paths_to_the_backend() {
        let router = Router::new();
        let fake = Arc::new(FakeFs::default());
        router.mount(MountId(7), fake.clone());

        let mut stream = router.read_dir(&remote("/srv"), ListingDetail::Full);
        let batch = smol::block_on(stream.next()).unwrap().unwrap();
        assert_eq!(batch[0].name(), "remote.txt");
        assert_eq!(fake.listings.load(Ordering::Relaxed), 1);

        // Unmounting takes effect for the next call; the id stops resolving.
        router.unmount(MountId(7));
        assert!(!router.is_mounted(MountId(7)));
        let mut stream = router.read_dir(&remote("/srv"), ListingDetail::Full);
        assert!(smol::block_on(stream.next()).unwrap().is_err());
    }

    #[test]
    fn local_paths_never_reach_a_mount() {
        let router = Router::new();
        let fake = Arc::new(FakeFs::default());
        router.mount(MountId(7), fake.clone());

        let mut stream = router.read_dir(&VfsPath::local("/"), ListingDetail::NamesOnly);
        // Whatever the local root holds, the fake must not have been asked.
        let _ = smol::block_on(stream.next());
        assert_eq!(fake.listings.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_move_across_backends_is_refused_by_name() {
        let router = Router::new();
        router.mount(MountId(7), Arc::new(FakeFs::default()));

        let err = smol::block_on(router.rename(&VfsPath::local("/tmp/a"), &remote("/srv/a")))
            .unwrap_err();
        let VfsError::CrossBackend { from, to } = &err else {
            panic!("{err:?}");
        };
        assert!(from.is_local() && !to.is_local());
        // The message has to name both ends: "it failed" with no subject is
        // the failure this convention exists to prevent.
        let message = err.to_string();
        assert!(message.contains("/tmp/a") && message.contains("/srv/a"), "{message}");

        // Two different servers are also two backends.
        let other = VfsPath::remote(MountId(8), "/srv/a");
        let err = smol::block_on(router.copy(&remote("/srv/a"), &other)).unwrap_err();
        assert!(matches!(err, VfsError::CrossBackend { .. }), "{err}");
    }

    #[test]
    fn capabilities_follow_the_folder_not_the_router() {
        let router = Router::new();
        router.mount(MountId(7), Arc::new(FakeFs::default()));

        // Local disk has a watcher and an atomic rename; the fake has neither.
        assert!(router.capabilities_for(&VfsPath::local("/tmp")).watch);
        assert!(!router.capabilities_for(&remote("/srv")).watch);
        assert!(!router.capabilities_for(&remote("/srv")).atomic_rename);

        // An unmounted path claims nothing at all.
        let unmounted = VfsPath::remote(MountId(99), "/srv");
        assert_eq!(router.capabilities_for(&unmounted), UNMOUNTED_CAPABILITIES);
    }

    #[test]
    fn a_remote_directory_has_no_watcher() {
        let router = Router::new();
        router.mount(MountId(7), Arc::new(FakeFs::default()));
        // `Watch` is not `Debug` — it owns a live registration — so this
        // destructures rather than reaching for `unwrap_err`.
        let Err(err) = router.watch(&remote("/srv")) else {
            panic!("a remote directory must not report a watcher");
        };
        assert!(matches!(err, VfsError::Unsupported { .. }), "{err}");
    }
}
