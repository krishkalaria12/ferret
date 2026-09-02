//! Turning native filesystem notifications into index work.
//!
//! Watcher events are treated as **hints, never as truth**. Every backend this
//! runs on drops or coalesces them: FSEvents summarises under load, and
//! `ReadDirectoryChangesW` silently discards everything once its kernel buffer
//! overflows. So nothing here tries to reconstruct history from event kinds —
//! a path is marked dirty, and the applier re-reads what is on disk now.
//!
//! The recovery contract for what this misses is the periodic reconciliation
//! sweep in [`super::crawl`], not a cleverer event decoder.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Instant;

use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::config::{DIRTY_QUEUE_CAPACITY, INDEX_DEBOUNCE};
use crate::{Result, SearchError};

pub(crate) enum Signal {
    Changed(PathBuf),
    /// Events were lost. Individual paths are no longer a complete account of
    /// what changed, so the only correct response is a full sweep.
    Desynced,
}

/// One recursive watcher per indexed root.
///
/// Per root rather than one for everything, matching `ferret-vfs`'s registry
/// (D-37): registering and unregistering a directory restarts an FSEvents
/// stream, and a root being added must not blind the others while it does.
pub(crate) struct RootWatchers {
    /// Held for their `Drop`, which unregisters the native streams.
    _watchers: Vec<RecommendedWatcher>,
    pub(crate) events: Receiver<Signal>,
}

impl RootWatchers {
    pub(crate) fn start(roots: &[PathBuf]) -> Result<Self> {
        let (sender, events) = sync_channel(DIRTY_QUEUE_CAPACITY);
        let aliases = Arc::new(
            roots
                .iter()
                .map(|root| RootAlias::for_root(root))
                .collect::<Vec<_>>(),
        );
        let mut watchers = Vec::with_capacity(roots.len());
        for root in roots {
            let sender = sender.clone();
            let aliases = Arc::clone(&aliases);
            let mut watcher =
                notify::recommended_watcher(move |event| forward(&sender, &aliases, event))
                    .map_err(|error| SearchError::IndexWrite {
                        message: format!("couldn't start a filesystem watcher: {error}"),
                    })?;
            // A failed registration is not fatal: that root simply relies on
            // reconciliation for freshness, which is slower but correct.
            let _ = watcher.watch(root, RecursiveMode::Recursive);
            watchers.push(watcher);
        }
        Ok(Self {
            _watchers: watchers,
            events,
        })
    }
}

/// One root under both the name it was registered with and the name the
/// platform's watcher reports it as.
///
/// These differ more often than they look like they should. On macOS FSEvents
/// resolves every symlink in the path, so a root under `/var/folders` — where
/// `TMPDIR` points, and where a home directory can also live on managed
/// machines — comes back as `/private/var/folders`. An event whose path no
/// root recognises is an event silently dropped, and the index then depends on
/// the fifteen-minute reconciliation for changes it should have seen at once.
///
/// The rewrite goes back to the *registered* form rather than forward to the
/// canonical one, because the stored path has to equal the one the live walk
/// produces for the same file. If they diverged, the merge could not recognise
/// two rows as the same match and every file in the tree would appear twice.
struct RootAlias {
    given: PathBuf,
    canonical: PathBuf,
}

impl RootAlias {
    fn for_root(root: &Path) -> Self {
        Self {
            given: root.to_path_buf(),
            canonical: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
        }
    }

    fn rewrite(&self, path: &Path) -> Option<PathBuf> {
        if self.canonical == self.given {
            return None;
        }
        path.strip_prefix(&self.canonical)
            .ok()
            .map(|rest| self.given.join(rest))
    }
}

fn forward(
    sender: &SyncSender<Signal>,
    aliases: &[RootAlias],
    event: notify::Result<notify::Event>,
) {
    let Ok(event) = event else {
        let _ = sender.try_send(Signal::Desynced);
        return;
    };
    for path in event.paths {
        let path = aliases
            .iter()
            .find_map(|alias| alias.rewrite(&path))
            .unwrap_or(path);
        // A full queue means the index is already further behind than the
        // individual paths can express. Saying so once is both cheaper and
        // more correct than blocking a native callback thread.
        if let Err(TrySendError::Full(_)) = sender.try_send(Signal::Changed(path)) {
            let _ = sender.try_send(Signal::Desynced);
            return;
        }
    }
}

/// Accumulates dirty paths until the burst settles.
///
/// A single editor save produces several events, and a `git checkout` or a
/// build produces thousands over a few hundred milliseconds. Committing the
/// index once per event would make the daemon the busiest process on the
/// machine during a compile.
#[derive(Default)]
pub(crate) struct DirtyQueue {
    paths: HashSet<PathBuf>,
    desynced: bool,
    first_seen: Option<Instant>,
}

impl DirtyQueue {
    pub(crate) fn absorb(&mut self, signal: Signal) {
        match signal {
            Signal::Changed(path) => {
                self.paths.insert(path);
            }
            Signal::Desynced => self.desynced = true,
        }
        self.first_seen.get_or_insert_with(Instant::now);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty() && !self.desynced
    }

    pub(crate) fn is_due(&self) -> bool {
        self.first_seen
            .is_some_and(|first| first.elapsed() >= INDEX_DEBOUNCE)
    }

    /// Takes the settled batch, dropping paths an ancestor already covers.
    ///
    /// Re-indexing a directory walks its whole subtree, so a batch containing
    /// `src` and two hundred files under `src` is one unit of work, not two
    /// hundred and one. Sorting first is what makes a single forward pass
    /// enough to find the ancestors.
    pub(crate) fn take(&mut self) -> (Vec<PathBuf>, bool) {
        self.first_seen = None;
        let desynced = std::mem::take(&mut self.desynced);
        let mut paths: Vec<PathBuf> = std::mem::take(&mut self.paths).into_iter().collect();
        paths.sort();
        let mut collapsed: Vec<PathBuf> = Vec::with_capacity(paths.len());
        for path in paths {
            if collapsed
                .last()
                .is_some_and(|previous| is_ancestor(previous, &path))
            {
                continue;
            }
            collapsed.push(path);
        }
        (collapsed, desynced)
    }
}

/// Component-wise containment, never textual: `/a/bs` starts with the text
/// `/a/b` and is not inside it.
fn is_ancestor(ancestor: &Path, path: &Path) -> bool {
    path != ancestor && path.starts_with(ancestor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dirty_directory_absorbs_its_children() {
        let mut queue = DirtyQueue::default();
        for path in ["/a/src", "/a/src/one.rs", "/a/src/deep/two.rs", "/a/other"] {
            queue.absorb(Signal::Changed(PathBuf::from(path)));
        }
        let (paths, desynced) = queue.take();

        assert!(!desynced);
        assert_eq!(
            paths,
            vec![PathBuf::from("/a/other"), PathBuf::from("/a/src")]
        );
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_not_absorbed() {
        let mut queue = DirtyQueue::default();
        queue.absorb(Signal::Changed(PathBuf::from("/a/bar")));
        queue.absorb(Signal::Changed(PathBuf::from("/a/barn")));

        assert_eq!(queue.take().0.len(), 2);
    }

    #[test]
    fn a_desync_survives_the_batch_it_arrived_in() {
        let mut queue = DirtyQueue::default();
        queue.absorb(Signal::Changed(PathBuf::from("/a/one")));
        queue.absorb(Signal::Desynced);

        let (paths, desynced) = queue.take();
        assert_eq!(paths.len(), 1);
        assert!(desynced, "a lost event must still force a full sweep");
        assert!(queue.is_empty(), "and must not repeat on the next batch");
    }
}
