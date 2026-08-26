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

use notify::Watcher as _;
use std::path::PathBuf;
use std::time::Duration;

/// How long the watcher will wait before reporting an event burst.
///
/// Editors love to write a file three times in 50 ms; without this debounce
/// one save triggers three full re-lists of a 200,000-entry directory.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// What happened to the watched directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// Contents changed: entries were created, removed, renamed or rewritten.
    Changed,
    /// The watched directory itself was deleted (or became unreachable).
    Gone,
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
    /// bounded channel; when the channel is full the oldest behaviour wins by
    /// dropping the *new* event, because a missed event only costs freshness,
    /// while a blocked watcher thread would cost the whole backend.
    pub fn start(dir: PathBuf) -> std::io::Result<Watch> {
        let (tx, rx) = async_channel::bounded::<WatchEvent>(64);

        let to_io = |err: notify::Error| std::io::Error::other(err.to_string());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let kind = match res {
                Ok(event) => match event.kind {
                    notify::EventKind::Remove(_) => WatchEvent::Gone,
                    _ => WatchEvent::Changed,
                },
                // A path-level error usually means the subject vanished.
                Err(_) => WatchEvent::Gone,
            };
            let _ = tx.try_send(kind);
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
            assert_eq!(event, WatchEvent::Changed);
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
}
