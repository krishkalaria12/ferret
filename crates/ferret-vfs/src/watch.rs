//! Change notification, bridging the `notify` crate into Ferret's async
//! world.
//!
//! The shape is deliberately minimal: the file pane does not want a stream of
//! raw rename/create/modify tuples — it wants "this folder changed, look
//! again" and "the folder itself is gone". Coalescing happens on the consumer
//! side (a short debounce), because only it knows how busy the UI currently
//! is.
//!
//! Dropping [`Watch`] unregisters the watcher. There is no stop token to
//! forget, matching the cancel-by-drop rule of `Vfs::read_dir`.

use crate::config::WATCH_EVENT_BUFFER;
use notify::Watcher as _;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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
    Changed(Vec<std::path::PathBuf>),
    /// At least one event could not fit in the delivery queue.
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
/// Once the bounded queue fills, the normal events are deliberately discarded:
/// their individual paths cannot make the consumer's model correct again. A
/// single helper thread waits for room to deliver `Desynced`; that event is the
/// durable recovery contract. The flag keeps a large burst from creating one
/// waiting thread per discarded native event.
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

/// A live watch over one directory.
///
/// Keep it alive for as long as updates are wanted; dropping it detaches.
pub struct Watch {
    pub events: async_channel::Receiver<WatchEvent>,
    // Held for its Drop side: unregistering the native watcher.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Watch {
    /// Start watching `dir` non-recursively.
    ///
    /// The native watcher runs on its own thread and forwards events into a
    /// bounded channel. It never waits for the UI: when that queue fills, a
    /// single [`WatchEvent::Desynced`] is delivered as soon as there is room,
    /// telling the consumer to perform one complete re-list.
    pub fn start(dir: PathBuf) -> std::io::Result<Watch> {
        let (tx, rx) = async_channel::bounded::<WatchEvent>(WATCH_EVENT_BUFFER);
        let forwarder = EventForwarder::new(tx);
        let watched = dir.clone();

        let to_io = |err: notify::Error| std::io::Error::other(err.to_string());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let kind = match res {
                Ok(event) => {
                    // Removing the watched directory itself ends the
                    // story; removing something *inside* it is just a
                    // change the consumer re-stats.
                    let removed_self = matches!(event.kind, notify::EventKind::Remove(_))
                        && event.paths.len() == 1
                        && event.paths.first().is_some_and(|p| p == &*watched);
                    if removed_self {
                        WatchEvent::Gone
                    } else {
                        WatchEvent::Changed(event.paths)
                    }
                }
                // A path-level error usually means the subject vanished.
                Err(_) => WatchEvent::Gone,
            };
            forwarder.forward(kind);
        })
        .map_err(to_io)?;

        watcher
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .map_err(to_io)?;

        Ok(Watch {
            events: rx,
            _watcher: Some(watcher),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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

    #[test]
    fn writes_surface_as_changed() {
        let root = tempdir("changed");
        let watch = Watch::start(root.clone()).unwrap();

        fs::write(root.join("new.txt"), b"x").unwrap();

        smol::block_on(async {
            let event = watch
                .events
                .recv()
                .await
                .expect("watcher must report the write");
            let WatchEvent::Changed(paths) = event else {
                panic!("expected Changed, got {event:?}");
            };
            assert!(
                paths.iter().any(|p| p.ends_with("new.txt")),
                "the write's path must be reported: {paths:?}"
            );
        });

        fs::remove_dir_all(&root).ok();
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
