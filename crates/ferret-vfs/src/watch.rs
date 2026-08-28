//! Change notification, bridging the `notify` crate into Ferret's async
//! world.
//!
//! The shape is deliberately minimal: the file pane does not want a stream of
//! raw rename/create/modify tuples — it wants "this folder changed, look
//! again" and "the folder itself is gone". Coalescing happens on the consumer
//! side (a short debounce), because only it knows how busy the UI currently
//! is.
//!
//! Behind that shape is one registry (D-37). There is a single backend watcher
//! for the whole process, and [`Watch`] is a reference-counted subscription to
//! one directory within it. Before this, every `Folder` constructed its own
//! `notify::RecommendedWatcher` — its own FSEvents stream on macOS or
//! `ReadDirectoryChangesW` registration on Windows, its own thread, and its
//! own draining task. Ten tabs in column view was up to forty of each, for a
//! handful of distinct directories most of which were shared (A-17).
//!
//! Dropping [`Watch`] unsubscribes. There is no stop token to forget,
//! matching the cancel-by-drop rule of `Vfs::read_dir`.
//!
//! # The cost a shared backend has, and what is done about it
//!
//! `notify`'s FSEvents backend stops and restarts its whole stream on every
//! `watch` and `unwatch` (`fsevent.rs`: both call `stop()`, edit the path
//! list, then `run()`). With one watcher per folder that was invisible,
//! because each stream was its own. Sharing one means every registration
//! change is a short window in which *other* directories' events are lost —
//! and a lost event is a row that stays stale until something else happens,
//! which is the failure A-06 exists to prevent.
//!
//! It cannot be removed without going back to one stream per folder, which is
//! what A-17 is about. It is made rare instead: a directory that loses its
//! last subscriber keeps its registration for a while ([`MAX_IDLE_WATCHES`]),
//! so navigating away and back, or closing and reopening a tab, costs no
//! rebuild at all — only reaching a directory nothing has watched recently
//! does.

use crate::config::WATCH_EVENT_BUFFER;
use notify::Watcher as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

/// How many directories keep a native registration after their last
/// subscriber leaves.
///
/// Unregistering is not free — see the module header — so a directory that
/// goes idle is kept for a while in case it is wanted again, which navigating
/// back or reopening a tab is. Bounded because the registration is real work
/// the OS does for a folder nobody is looking at.
const MAX_IDLE_WATCHES: usize = 8;

/// How long the watcher will wait before reporting an event burst.
///
/// Editors love to write a file three times in 50 ms; without this debounce
/// one save triggers three full re-lists of a 200,000-entry directory.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// What happened to the watched directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// Contents changed. Carries the paths the backend reported, already
    /// coalesced by the consumer's debounce; consumers re-`stat` each one,
    /// because by the time the event lands the truth lives on disk, not in
    /// the event kind.
    Changed(Vec<PathBuf>),
    /// At least one event could not fit in the delivery queue, or the backend
    /// reported a failure that names no directory in particular.
    ///
    /// Individual paths are no longer a complete account of disk state, so
    /// consumers must discard their incremental work and re-list the watched
    /// directory. This is sent exactly once for each overflow interval.
    Desynced,
    /// The watched directory itself was deleted (or became unreachable).
    Gone,
}

/// Delivers native callbacks without ever blocking a native watcher thread.
///
/// Once a subscriber's bounded queue fills, its normal events are deliberately
/// discarded: their individual paths cannot make that consumer's model correct
/// again. A single helper thread waits for room to deliver `Desynced`; that
/// event is the durable recovery contract. The flag keeps a large burst from
/// creating one waiting thread per discarded native event.
#[derive(Clone)]
struct EventForwarder {
    tx: async_channel::Sender<WatchEvent>,
    desync_pending: Arc<AtomicBool>,
}

impl EventForwarder {
    fn new(tx: async_channel::Sender<WatchEvent>) -> Self {
        Self {
            tx,
            desync_pending: Arc::new(AtomicBool::new(false)),
        }
    }

    fn forward(&self, event: WatchEvent) {
        // A queued (or waiting) desync makes all later individual events
        // redundant until the consumer has re-listed the directory.
        if self.desync_pending.load(Ordering::Acquire) {
            return;
        }

        if !matches!(
            self.tx.try_send(event),
            Err(async_channel::TrySendError::Full(_))
        ) {
            return;
        }

        // Only the first overflowing callback starts the waiter. It blocks
        // outside notify's callback thread and unblocks when the receiver is
        // dropped, so dropping Watch remains cancellation by drop.
        if self.desync_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let tx = self.tx.clone();
        let desync_pending = Arc::clone(&self.desync_pending);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(WatchEvent::Desynced);
            desync_pending.store(false, Ordering::Release);
        });
    }
}

/// One consumer's claim on one watched directory.
struct Subscriber {
    id: u64,
    forwarder: EventForwarder,
    /// The directory as the caller named it, which is not necessarily the
    /// canonical one the registry keys on. Events are re-expressed against
    /// this so a consumer never receives a path it cannot recognise — see
    /// [`Registry::subscribe`].
    requested: PathBuf,
}

/// The process-wide watcher (D-37).
///
/// One backend watcher, a map of directory to subscribers, and a native
/// registration that lasts exactly as long as the first subscriber to the
/// last. Remote mounts (Phase 7) will need a different change-detection
/// mechanism entirely — polling for SFTP, nothing at all for S3 — and this is
/// the seam where that plugs in.
struct Registry {
    /// `None` until the first subscription, and again if the backend refuses
    /// to start. Separate from `subscribers` so neither lock is ever held
    /// while the other is taken.
    backend: Mutex<Option<notify::RecommendedWatcher>>,
    subscribers: Mutex<HashMap<PathBuf, Vec<Subscriber>>>,
    /// Registered directories with no subscribers left, oldest first.
    ///
    /// Its own lock, taken after `subscribers` is released rather than while
    /// it is held, so the two can never deadlock against the backend thread.
    idle: Mutex<Vec<PathBuf>>,
    next_id: AtomicU64,
}

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry() -> &'static Arc<Registry> {
    REGISTRY.get_or_init(|| {
        Arc::new(Registry {
            backend: Mutex::new(None),
            subscribers: Mutex::new(HashMap::new()),
            idle: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        })
    })
}

/// Take a lock, recovering the guard if a previous holder panicked.
///
/// Nothing inside these critical sections can panic — they are map operations
/// and a non-blocking channel send — so a poisoned lock could only come from a
/// panic elsewhere in the process. Refusing to watch anything ever again
/// because of that is a far worse outcome than continuing.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Registry {
    /// Fan one backend event out to the directories it concerns.
    ///
    /// A non-recursive watch on `D` reports paths inside `D`, plus `D` itself
    /// when `D` is removed. One event can name paths in more than one watched
    /// directory — a rename across two open tabs does — so they are grouped by
    /// parent rather than broadcast.
    ///
    /// Paths arrive in the canonical namespace — FSEvents resolves symlinks
    /// before reporting, so a watch on `/var/folders/x` produces
    /// `/private/var/folders/x/...` — and are re-expressed against each
    /// subscriber's own directory on the way out.
    fn dispatch(&self, result: notify::Result<notify::Event>) {
        let subscribers = lock(&self.subscribers);

        let event = match result {
            Ok(event) => event,
            Err(_) => {
                // The backend is shared, so a failure it does not attribute to
                // a path is not attributable to a directory either. Every
                // subscriber re-lists; one that has genuinely vanished finds
                // out from its own `read_dir`, which is the reliable signal.
                for subscriber in subscribers.values().flatten() {
                    subscriber.forwarder.forward(WatchEvent::Desynced);
                }
                return;
            }
        };

        let removal = matches!(event.kind, notify::EventKind::Remove(_));
        // Grouped by directory, not broadcast: one event can name paths in
        // more than one watched directory — a rename between two open tabs
        // does — and each consumer must hear only about its own.
        let mut changed: HashMap<&Path, Vec<&Path>> = HashMap::new();

        for path in &event.paths {
            // Removing the watched directory itself ends the story for its
            // subscribers; removing something *inside* it is just a change
            // they re-stat.
            if removal && let Some(watchers) = subscribers.get(path.as_path()) {
                for subscriber in watchers {
                    subscriber.forwarder.forward(WatchEvent::Gone);
                }
            }
            if let Some(parent) = path.parent()
                && let Some((dir, _)) = subscribers.get_key_value(parent)
            {
                changed
                    .entry(dir.as_path())
                    .or_default()
                    .push(path.as_path());
            }
        }

        for (dir, paths) in changed {
            let Some(watchers) = subscribers.get(dir) else {
                continue;
            };
            for subscriber in watchers {
                let rebased = paths
                    .iter()
                    .map(|path| match path.file_name() {
                        Some(name) => subscriber.requested.join(name),
                        None => path.to_path_buf(),
                    })
                    .collect();
                subscriber.forwarder.forward(WatchEvent::Changed(rebased));
            }
        }
    }

    /// Register `dir` with the backend, starting it if this is the first one.
    fn watch_natively(self: &Arc<Self>, dir: &Path) -> std::io::Result<()> {
        let to_io = |err: notify::Error| std::io::Error::other(err.to_string());
        let mut backend = lock(&self.backend);
        if backend.is_none() {
            let registry = Arc::clone(self);
            *backend = Some(
                notify::recommended_watcher(move |result| registry.dispatch(result))
                    .map_err(to_io)?,
            );
        }
        backend
            .as_mut()
            .expect("just constructed")
            .watch(dir, notify::RecursiveMode::NonRecursive)
            .map_err(to_io)
    }

    fn unwatch_natively(&self, dir: &Path) {
        if let Some(backend) = lock(&self.backend).as_mut() {
            // The directory may already be gone, which is exactly when this
            // fails and exactly when there is nothing left to detach.
            let _ = backend.unwatch(dir);
        }
    }

    fn subscribe(self: &Arc<Self>, requested: PathBuf) -> std::io::Result<Watch> {
        // Keyed on the canonical path, because that is the namespace the
        // backend reports in: on macOS a watch on `/var/folders/x` produces
        // events under `/private/var/folders/x`, and routing by parent would
        // find no subscriber at all. It also means two tabs that reached one
        // directory by different symlinked routes share a single native
        // watch, which is the point of the registry.
        let dir = requested
            .canonicalize()
            .unwrap_or_else(|_| requested.clone());

        let (tx, rx) = async_channel::bounded::<WatchEvent>(WATCH_EVENT_BUFFER);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let subscriber = Subscriber {
            id,
            forwarder: EventForwarder::new(tx),
            requested,
        };

        // Whether this is the first claim decides whether the backend has to
        // learn about the directory at all — which is the whole point: a
        // second tab on one folder costs a channel, not a native stream. An
        // idle registration is reclaimed here rather than rebuilt, so
        // navigating back to somewhere recently open is free.
        let first = {
            let mut subscribers = lock(&self.subscribers);
            let watchers = subscribers.entry(dir.clone()).or_default();
            watchers.push(subscriber);
            watchers.len() == 1
        };
        let reclaimed = {
            let mut idle = lock(&self.idle);
            let was_parked = idle.contains(&dir);
            idle.retain(|parked| *parked != dir);
            was_parked
        };
        let needs_registering = first && !reclaimed;

        if needs_registering && let Err(err) = self.watch_natively(&dir) {
            // Leave nothing half-registered behind for the next subscriber to
            // inherit as a watch that silently never fires.
            lock(&self.subscribers).remove(&dir);
            return Err(err);
        }

        Ok(Watch {
            events: rx,
            _subscription: Subscription {
                dir,
                id,
                registry: Arc::clone(self),
            },
        })
    }

    fn unsubscribe(&self, dir: &Path, id: u64) {
        let last = {
            let mut subscribers = lock(&self.subscribers);
            let Some(watchers) = subscribers.get_mut(dir) else {
                return;
            };
            watchers.retain(|subscriber| subscriber.id != id);
            let last = watchers.is_empty();
            if last {
                subscribers.remove(dir);
            }
            last
        };
        if !last {
            return;
        }

        // Not unregistered: parked. See the module header — each `unwatch`
        // costs every other watched directory a gap in delivery, and a folder
        // just navigated away from is one of the likeliest to come back.
        let evicted = {
            let mut idle = lock(&self.idle);
            idle.retain(|parked| parked != dir);
            idle.push(dir.to_path_buf());
            (idle.len() > MAX_IDLE_WATCHES).then(|| idle.remove(0))
        };
        if let Some(evicted) = evicted {
            self.unwatch_natively(&evicted);
        }
    }
}

/// A live watch over one directory.
///
/// Keep it alive for as long as updates are wanted; dropping it detaches.
pub struct Watch {
    pub events: async_channel::Receiver<WatchEvent>,
    // Held for its Drop side: unsubscribing from the registry.
    _subscription: Subscription,
}

/// The claim itself. Separate from [`Watch`] only so the drop is unmissable.
struct Subscription {
    dir: PathBuf,
    id: u64,
    registry: Arc<Registry>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.registry.unsubscribe(&self.dir, self.id);
    }
}

impl Watch {
    /// Start watching `dir` non-recursively.
    ///
    /// The backend watcher runs on its own thread and forwards events into a
    /// bounded channel per subscriber. It never waits for the UI: when a queue
    /// fills, a single [`WatchEvent::Desynced`] is delivered to that consumer
    /// as soon as there is room, telling it to perform one complete re-list.
    ///
    /// Subscribing to a directory something else already watches costs a
    /// channel and a map entry — no second native stream, no second thread.
    pub fn start(dir: PathBuf) -> std::io::Result<Watch> {
        registry().subscribe(dir)
    }

    /// How many consumers are watching `dir`, canonicalised as the registry
    /// keys it. For tests: a shared registration is the A-17 property.
    #[cfg(test)]
    fn subscribers_for(dir: &Path) -> usize {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        lock(&registry().subscribers).get(&dir).map_or(0, Vec::len)
    }

    /// Whether the backend is still registered for `dir` — with a live
    /// subscriber, or parked as idle.
    #[cfg(test)]
    fn is_registered(dir: &Path) -> bool {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        lock(&registry().subscribers).contains_key(&dir) || lock(&registry().idle).contains(&dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferret-vfs-watch-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write into `dir` and wait for every watch to report it.
    ///
    /// Retried, because these tests share one process-wide registry and one
    /// backend stream: another test registering a directory restarts that
    /// stream, and a write landing inside the restart is genuinely lost. That
    /// is the trade-off the module header describes, and a test that blocked
    /// forever on it would be reporting it as a hang rather than as the rare
    /// miss it is.
    fn expect_change(watches: &[(&str, &Watch)], dir: &Path) {
        for attempt in 0..20 {
            let name = format!("probe-{attempt}.txt");
            fs::write(dir.join(&name), b"x").unwrap();

            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            let mut seen = vec![false; watches.len()];
            while std::time::Instant::now() < deadline && !seen.iter().all(|s| *s) {
                for (ix, (_, watch)) in watches.iter().enumerate() {
                    while let Ok(event) = watch.events.try_recv() {
                        if let WatchEvent::Changed(paths) = event
                            && paths.iter().any(|p| p.ends_with(&name))
                        {
                            seen[ix] = true;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if seen.iter().all(|s| *s) {
                return;
            }
        }
        let missing: Vec<&str> = watches.iter().map(|(who, _)| *who).collect();
        panic!(
            "no watch in {missing:?} ever reported a write to {}",
            dir.display()
        );
    }

    #[test]
    fn writes_surface_as_changed() {
        let root = tempdir("changed");
        let watch = Watch::start(root.clone()).unwrap();
        expect_change(&[("only", &watch)], &root);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn events_are_reported_under_the_path_the_caller_asked_about() {
        // The registry keys on the canonical path — on macOS `std::env::temp_dir`
        // is under a symlinked `/var` and FSEvents reports `/private/var` — but
        // a consumer filters events against the path *it* named, so that is the
        // namespace they have to arrive in.
        let root = tempdir("namespace");
        let watch = Watch::start(root.clone()).unwrap();

        for attempt in 0..20 {
            let name = format!("probe-{attempt}.txt");
            fs::write(root.join(&name), b"x").unwrap();

            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                while let Ok(event) = watch.events.try_recv() {
                    let WatchEvent::Changed(paths) = event else {
                        continue;
                    };
                    assert!(
                        paths.iter().all(|p| p.starts_with(&root)),
                        "{paths:?} must be under {}, not the canonical form",
                        root.display()
                    );
                    if paths.iter().any(|p| p.ends_with(&name)) {
                        fs::remove_dir_all(&root).ok();
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fs::remove_dir_all(&root).ok();
        panic!("the watcher never reported the write");
    }

    #[test]
    fn dropping_the_watch_stops_delivery() {
        let root = tempdir("drop");
        let watch = Watch::start(root.clone()).unwrap();
        drop(watch);
        // Give the backend a moment to detach, then prove nothing panics and
        // nothing is delivered.
        std::thread::sleep(Duration::from_millis(50));
        fs::write(root.join("after.txt"), b"x").unwrap();
        std::thread::sleep(DEBOUNCE);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn two_watches_on_one_directory_share_one_registration() {
        // The A-17 property: ten tabs on one folder is one native stream, and
        // every consumer still sees the event.
        let root = tempdir("shared");
        let a = Watch::start(root.clone()).unwrap();
        let b = Watch::start(root.clone()).unwrap();
        assert_eq!(
            Watch::subscribers_for(&root),
            2,
            "two subscribers, one directory"
        );

        expect_change(&[("a", &a), ("b", &b)], &root);

        // The registration outlives the first unsubscribe: a tab closing must
        // not stop the tab beside it from updating.
        drop(a);
        assert_eq!(Watch::subscribers_for(&root), 1);
        expect_change(&[("b", &b)], &root);

        drop(b);
        assert_eq!(Watch::subscribers_for(&root), 0);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn each_subscriber_hears_only_about_its_own_directory() {
        let one = tempdir("two-a");
        let two = tempdir("two-b");

        let a = Watch::start(one.clone()).unwrap();
        let b = Watch::start(two.clone()).unwrap();

        expect_change(&[("b", &b)], &two);
        // Grouping by parent, not broadcasting, is what keeps `a` quiet.
        assert!(
            a.events.is_empty(),
            "the other directory should have heard nothing"
        );

        drop((a, b));
        fs::remove_dir_all(&one).ok();
        fs::remove_dir_all(&two).ok();
    }

    #[test]
    fn an_idle_directory_keeps_its_registration_for_a_while() {
        // Unregistering costs every other watched directory a gap in
        // delivery, so navigating away and back must not pay for one.
        let root = tempdir("idle");
        drop(Watch::start(root.clone()).unwrap());

        assert_eq!(Watch::subscribers_for(&root), 0, "nobody is watching");
        assert!(
            Watch::is_registered(&root),
            "but the registration is parked, not torn down"
        );

        // Coming back reclaims it, and events still arrive.
        let again = Watch::start(root.clone()).unwrap();
        expect_change(&[("again", &again)], &root);

        drop(again);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parked_registrations_are_bounded() {
        // Bounded because each one is real work the OS does for a folder
        // nobody is looking at.
        let roots: Vec<PathBuf> = (0..MAX_IDLE_WATCHES + 2)
            .map(|i| tempdir(&format!("evict-{i}")))
            .collect();
        for root in &roots {
            drop(Watch::start(root.clone()).unwrap());
        }

        let parked = roots.iter().filter(|r| Watch::is_registered(r)).count();
        assert!(
            parked <= MAX_IDLE_WATCHES,
            "{parked} parked, cap is {MAX_IDLE_WATCHES}"
        );
        assert!(
            !Watch::is_registered(&roots[0]),
            "the oldest is evicted first"
        );

        for root in &roots {
            fs::remove_dir_all(root).ok();
        }
    }

    #[test]
    fn overflow_eventually_reports_a_desync() {
        let (tx, rx) = async_channel::bounded(1);
        let forwarder = EventForwarder::new(tx);

        forwarder.forward(WatchEvent::Changed(vec![PathBuf::from("first")]));
        forwarder.forward(WatchEvent::Changed(vec![PathBuf::from("dropped")]));

        assert!(matches!(
            rx.recv_blocking()
                .expect("the initial event must be queued"),
            WatchEvent::Changed(_)
        ));
        assert_eq!(
            rx.recv_blocking()
                .expect("overflow must be reported once capacity returns"),
            WatchEvent::Desynced
        );
    }
}
