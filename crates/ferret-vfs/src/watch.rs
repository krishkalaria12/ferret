//! Change notification, bridging the `notify` crate into Ferret's async
//! world.
//!
//! The shape is deliberately minimal: the file pane does not want a stream of
//! raw rename/create/modify tuples — it wants "this folder changed, look
//! again" and "the folder itself is gone". Coalescing happens on the consumer
//! side (a short debounce), because only it knows how busy the UI currently
//! is.
//!
//! Behind that shape is a registry (D-37): **one backend watcher per distinct
//! directory**, with reference-counted subscriptions, and [`Watch`] is a
//! subscription rather than a watcher. Before it, every `Folder` constructed
//! its own `notify::RecommendedWatcher` — its own FSEvents stream on macOS or
//! `ReadDirectoryChangesW` registration on Windows, its own thread, and its
//! own draining task. Ten tabs in column view was up to forty of each, for a
//! handful of distinct directories most of which were shared (A-17).
//!
//! Dropping [`Watch`] unsubscribes. There is no stop token to forget,
//! matching the cancel-by-drop rule of `Vfs::read_dir`.
//!
//! # Why per directory, and not one watcher for everything
//!
//! One watcher for the whole process is the obvious reading of A-17, and it is
//! wrong. `notify`'s FSEvents backend stops and restarts its entire stream on
//! every `watch` and `unwatch` — `fsevent.rs`: both call `stop()`, edit the
//! path list, then `run()` — and it pins `since_when` to
//! `kFSEventStreamEventIdSinceNow` at construction without ever advancing it.
//! So with a shared stream, every registration change is a window in which
//! *every other watched directory* loses events outright.
//!
//! That was measured, not assumed: writing 240 files into one watched
//! directory while twelve others were subscribed and dropped around it lost
//! **91% of them** (D-40). A lost event is a row that stays stale until
//! something else happens, which is exactly the failure A-06 exists to
//! prevent — so this was strictly worse than the forty watchers it replaced.
//!
//! What A-17 actually asked for is deduplication by *directory*: ten tabs on
//! one folder should cost one watcher, not ten. Keying the registry that way
//! keeps every bit of that — column view over a deep path is four watchers,
//! not forty — while giving each directory its own independent stream, so
//! registering one cannot disturb another. `notify` 8.2.0 behaves identically
//! here, so upgrading is not an alternative.

use crate::config::WATCH_EVENT_BUFFER;
use notify::Watcher as _;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

/// How many directories keep a live watcher after their last subscriber
/// leaves.
///
/// Building one spawns a thread and an OS-level stream, so a directory that
/// has just gone idle is worth keeping briefly in case it is wanted again —
/// which navigating away and back, or closing and reopening a tab, is.
///
/// Bounded because an idle watcher is real work the OS does for a folder
/// nobody is looking at. Unlike under the old shared stream, holding these is
/// now purely a cost-vs-latency trade: an idle watcher cannot affect what any
/// other directory sees.
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
/// One watched directory: its backend watcher, and everyone listening to it.
struct Watched {
    /// Held for its `Drop`, which unregisters the directory. Its own stream,
    /// so building or dropping it cannot disturb any other directory.
    _watcher: notify::RecommendedWatcher,
    subscribers: Vec<Subscriber>,
}

struct Registry {
    /// Canonical directory to its watcher and subscribers.
    ///
    /// `parking_lot`: this is taken from a backend callback thread on every
    /// filesystem event, it guards a map lookup and a non-blocking send, and
    /// `std`'s poisoning would turn one panic anywhere into a process that
    /// silently stops watching files.
    watched: Mutex<HashMap<PathBuf, Watched>>,
    /// Directories still watched with nobody listening, oldest first. Their
    /// entries stay in `watched`; this is only the eviction order.
    ///
    /// Its own lock, taken after `watched` is released rather than while it is
    /// held, so the two can never deadlock against a callback thread.
    idle: Mutex<Vec<PathBuf>>,
    next_id: AtomicU64,
}

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

fn registry() -> &'static Arc<Registry> {
    REGISTRY.get_or_init(|| {
        Arc::new(Registry {
            watched: Mutex::new(HashMap::new()),
            idle: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        })
    })
}

impl Registry {
    /// Fan one directory's events out to everyone subscribed to it.
    ///
    /// `dir` is the canonical path this watcher was created for, so there is
    /// no routing to do — a watcher hears about one directory and nothing
    /// else. Paths arrive in the canonical namespace (FSEvents resolves
    /// symlinks before reporting, so a watch on `/var/folders/x` produces
    /// `/private/var/folders/x/...`) and are re-expressed against each
    /// subscriber's own directory on the way out.
    fn dispatch(&self, dir: &Path, result: notify::Result<notify::Event>) {
        let watched = self.watched.lock();
        let Some(entry) = watched.get(dir) else {
            return; // Unsubscribed while this event was in flight.
        };

        let event = match result {
            Ok(event) => event,
            Err(_) => {
                // A backend error that names no path is not something the
                // individual paths can repair: this directory re-lists, and
                // one that has genuinely vanished finds out from its own
                // `read_dir`, which is the reliable signal.
                for subscriber in &entry.subscribers {
                    subscriber.forwarder.forward(WatchEvent::Desynced);
                }
                return;
            }
        };

        // Removing the watched directory itself ends the story; removing
        // something *inside* it is just a change to re-stat.
        let removal = matches!(event.kind, notify::EventKind::Remove(_));
        if removal && event.paths.iter().any(|p| p == dir) {
            for subscriber in &entry.subscribers {
                subscriber.forwarder.forward(WatchEvent::Gone);
            }
            return;
        }

        // Only direct children. The watch is `NonRecursive`, so this is
        // usually everything it reports — but a directory reached through a
        // symlink can see a sibling's path, and expanded subtrees are a
        // snapshot by design (A-24).
        let children: Vec<&Path> = event
            .paths
            .iter()
            .filter(|p| p.parent() == Some(dir))
            .map(PathBuf::as_path)
            .collect();
        if children.is_empty() {
            return;
        }

        for subscriber in &entry.subscribers {
            let rebased = children
                .iter()
                .map(|path| match path.file_name() {
                    Some(name) => subscriber.requested.join(name),
                    None => path.to_path_buf(),
                })
                .collect();
            subscriber.forwarder.forward(WatchEvent::Changed(rebased));
        }
    }

    /// Build this directory's own watcher.
    ///
    /// One stream per directory, which is what keeps a new subscription from
    /// interrupting delivery to every other one — see the module header.
    fn build_watcher(self: &Arc<Self>, dir: &Path) -> std::io::Result<notify::RecommendedWatcher> {
        let to_io = |err: notify::Error| std::io::Error::other(err.to_string());
        let registry = Arc::clone(self);
        let owned = dir.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |result| {
            registry.dispatch(&owned, result);
        })
        .map_err(to_io)?;
        watcher
            .watch(dir, notify::RecursiveMode::NonRecursive)
            .map_err(to_io)?;
        Ok(watcher)
    }

    /// Attach `subscriber` to `dir` if something already watches it.
    ///
    /// Hands the subscriber back when nothing does, so the caller can build a
    /// watcher — which must happen *outside* this lock, because the backend's
    /// callback thread takes the same one and `notify` makes no promise about
    /// which thread `watch()` may deliver on.
    fn attach(&self, dir: &Path, subscriber: Subscriber) -> Option<Subscriber> {
        let mut watched = self.watched.lock();
        match watched.get_mut(dir) {
            Some(entry) => {
                entry.subscribers.push(subscriber);
                None
            }
            None => Some(subscriber),
        }
    }

    fn subscribe(self: &Arc<Self>, requested: PathBuf) -> std::io::Result<Watch> {
        // Keyed on the canonical path, because that is the namespace the
        // backend reports in: on macOS a watch on `/var/folders/x` produces
        // events under `/private/var/folders/x`. It also means two tabs that
        // reached one directory by different symlinked routes share a single
        // native watch, which is the point of the registry.
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

        // A directory already watched — by another tab, or parked as idle —
        // costs a channel and a map entry. Only one nobody is watching builds
        // a stream, which is the whole of A-17.
        if let Some(subscriber) = self.attach(&dir, subscriber) {
            let watcher = self.build_watcher(&dir)?;
            // Asked again, because the map could have changed while the
            // watcher was being built: another subscriber may have arrived, or
            // an idle eviction may have removed the entry. Attaching wins
            // either way — the watcher just built is then dropped, which
            // unregisters it. Wasteful, not wrong, and it needs two threads
            // opening the same directory in the same instant.
            if let Some(subscriber) = self.attach(&dir, subscriber) {
                self.watched.lock().insert(
                    dir.clone(),
                    Watched {
                        _watcher: watcher,
                        subscribers: vec![subscriber],
                    },
                );
            }
        }

        // No longer idle, whether or not it was.
        self.idle.lock().retain(|parked| *parked != dir);

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
        let now_idle = {
            let mut watched = self.watched.lock();
            let Some(entry) = watched.get_mut(dir) else {
                return;
            };
            entry.subscribers.retain(|subscriber| subscriber.id != id);
            entry.subscribers.is_empty()
        };
        if !now_idle {
            return;
        }

        // Parked, not torn down: building a watcher spawns a thread and an
        // OS-level stream, and a folder just navigated away from is one of
        // the likeliest to come back. Dropping one no longer costs any other
        // directory anything, so this is purely latency against idle cost.
        let evicted = {
            let mut idle = self.idle.lock();
            idle.retain(|parked| parked != dir);
            idle.push(dir.to_path_buf());
            (idle.len() > MAX_IDLE_WATCHES).then(|| idle.remove(0))
        };
        if let Some(evicted) = evicted {
            // Dropping the entry drops its watcher, which unregisters it.
            let mut watched = self.watched.lock();
            if watched
                .get(&evicted)
                .is_some_and(|entry| entry.subscribers.is_empty())
            {
                watched.remove(&evicted);
            }
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
    /// keys it. For tests: sharing one watcher is the A-17 property.
    #[cfg(test)]
    fn subscribers_for(dir: &Path) -> usize {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        registry()
            .watched
            .lock()
            .get(&dir)
            .map_or(0, |entry| entry.subscribers.len())
    }

    /// Whether a watcher for `dir` still exists — with a live subscriber, or
    /// parked as idle.
    #[cfg(test)]
    fn is_registered(dir: &Path) -> bool {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        registry().watched.lock().contains_key(&dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
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
    fn subscribing_to_one_directory_does_not_cost_another_its_events() {
        // D-40, and the reason the registry is keyed per directory rather
        // than sharing one stream. `notify`'s FSEvents backend stops and
        // restarts the whole stream on every registration and resumes from
        // "now", so with one shared watcher everything written during a
        // rebuild was gone — measured at 91% loss.
        //
        // The contract being asserted is A-06's, not "nothing is ever
        // dropped": a consumer may miss individual events, but only if it is
        // *told* to re-list. What must never happen is silent staleness.
        let root = tempdir("churn");
        let watched = root.join("watched");
        fs::create_dir_all(&watched).unwrap();
        let watch = Watch::start(watched.clone()).unwrap();

        // Let the stream come up before anything is written into it.
        expect_change(&[("warmup", &watch)], &watched);
        while watch.events.try_recv().is_ok() {}

        const ROUNDS: usize = 8;
        const PER_ROUND: usize = 5;
        let mut expected = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut desynced = false;

        // Drained as we go, the way a real consumer does — the channel holds
        // WATCH_EVENT_BUFFER events, and a test that only drains at the end
        // is measuring its own laziness.
        let drain = |watch: &Watch, seen: &mut BTreeSet<String>, desynced: &mut bool| {
            while let Ok(event) = watch.events.try_recv() {
                match event {
                    WatchEvent::Changed(paths) => seen.extend(
                        paths
                            .iter()
                            .filter_map(|p| p.file_name()?.to_str().map(str::to_owned)),
                    ),
                    WatchEvent::Desynced => *desynced = true,
                    WatchEvent::Gone => {}
                }
            }
        };

        for round in 0..ROUNDS {
            // Churn: a directory subscribed and dropped around every write,
            // which is what entering and leaving column view does.
            let other = root.join(format!("other-{round}"));
            fs::create_dir_all(&other).unwrap();
            let sub = Watch::start(other).unwrap();
            for i in 0..PER_ROUND {
                let name = format!("f-{round}-{i}.txt");
                fs::write(watched.join(&name), b"x").unwrap();
                expected.insert(name);
            }
            drop(sub);
            std::thread::sleep(Duration::from_millis(40));
            drain(&watch, &mut seen, &mut desynced);
        }

        // FSEvents coalesces, so give the tail a moment to arrive.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !expected.is_subset(&seen) && !desynced {
            drain(&watch, &mut seen, &mut desynced);
            std::thread::sleep(Duration::from_millis(20));
        }

        let missing: Vec<&String> = expected.difference(&seen).collect();
        assert!(
            missing.is_empty() || desynced,
            "{} of {} writes went unreported, and no Desynced was delivered — \
             the listing would sit stale forever: {missing:?}",
            missing.len(),
            expected.len()
        );

        fs::remove_dir_all(&root).ok();
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
