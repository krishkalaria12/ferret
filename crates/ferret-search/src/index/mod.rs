//! The persistent index: two stores, one owner, and a rebuild that is always
//! cheaper than a repair.
//!
//! [`Index`] is the read side — a SQLite handle factory and a Tantivy reader,
//! both safe to use from any query thread. [`Indexer`] is the write side, and
//! there is exactly one of it: a single background thread owning the SQLite
//! writer, the Tantivy writer and the filesystem watchers. Splitting them this
//! way is what lets a crawl run at full speed without ever taking a lock a
//! keystroke is waiting on.
//!
//! Both stores are reconstructible from the filesystem in minutes. That fact
//! is load-bearing: it is why an incompatible or corrupt index is quarantined
//! and rebuilt rather than migrated, and why every failure path here degrades
//! to the live scan instead of trying to salvage something it cannot verify.

mod content;
mod crawl;
mod db;
mod service;
mod watch;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use content::ContentIndex;
use db::Db;
use watch::{DirtyQueue, RootWatchers};

pub use service::{IndexPlan, IndexSearchService};

use crate::config::RECONCILE_INTERVAL;
use crate::{IndexStatus, IndexedRoot, Result, RootState, SearchError, SearchFilters};

/// How long a cached [`IndexStatus`] is reused.
///
/// Status is read once per query to plan it, and computing it costs a SQLite
/// open and a directory walk. A second is far below the interval at which a
/// crawl changes the answer and far above the interval at which a person
/// types.
const STATUS_CACHE_TTL: Duration = Duration::from_secs(1);

/// The index directory, remembered so the crawl never indexes itself.
///
/// Without this the daemon watches a directory it writes to: every commit
/// produces watcher events, which produce index work, which produces commits.
static INDEX_DIR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn is_excluded(path: &Path) -> bool {
    INDEX_DIR
        .get()
        .is_some_and(|excluded| path.starts_with(excluded))
}

/// Whether the index was built under a policy that can answer these filters.
///
/// The crawl walks with [`SearchFilters::default`], so the index simply does
/// not contain hidden or ignored entries, and following symlinks would visit
/// paths it never stored. A request that turns any of those on is not
/// answerable from the index at all — not "answerable with gaps", which is the
/// mistake that would make an indexed search quietly less complete than a live
/// one.
pub(crate) fn policy_matches(filters: &SearchFilters) -> bool {
    let policy = SearchFilters::default();
    filters.include_hidden == policy.include_hidden
        && filters.respect_ignore == policy.respect_ignore
        && filters.follow_symlinks == policy.follow_symlinks
}

/// The read side of the stores.
pub struct Index {
    dir: PathBuf,
    db_path: PathBuf,
    content: ContentIndex,
    status: Mutex<Option<(Instant, IndexStatus)>>,
}

impl Index {
    /// Opens the stores, rebuilding them if this build cannot read them.
    ///
    /// Quarantine rather than delete: the old directory is moved aside, so a
    /// corruption that turns out to be a bug in Ferret is still on disk to be
    /// looked at. A rebuild starts immediately either way, and search runs
    /// live until it finishes.
    pub fn open_or_rebuild(dir: &Path) -> Result<Self> {
        match Self::open(dir) {
            Ok(index) => Ok(index),
            Err(error @ (SearchError::IndexVersion { .. } | SearchError::IndexOpen { .. })) => {
                quarantine(dir, &error)?;
                Self::open(dir)
            }
            Err(error) => Err(error),
        }
    }

    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|error| SearchError::IndexOpen {
            path: ferret_vfs::VfsPath::from(dir),
            message: error.to_string(),
        })?;
        let _ = INDEX_DIR.set(dir.to_path_buf());
        let db_path = dir.join("paths.sqlite");
        // Opened and dropped purely to create and validate the schema before
        // anything reports the index as available.
        drop(Db::open_writer(&db_path)?);
        let content = ContentIndex::open(&dir.join("content"))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            db_path,
            content,
            status: Mutex::new(None),
        })
    }

    pub(crate) fn reader(&self) -> Result<Db> {
        Db::open_reader(&self.db_path)
    }

    pub(crate) fn content(&self) -> &ContentIndex {
        &self.content
    }

    /// A snapshot of what is indexed, cached briefly.
    ///
    /// Never fails: a store that cannot be read reports `available: false`,
    /// which the planner reads as "index nothing" and the settings surface
    /// shows as an error state. Turning this into a `Result` would make every
    /// caller decide what an unreadable index means, and they would all decide
    /// the same thing.
    pub fn status(&self) -> IndexStatus {
        {
            let cached = self.status.lock();
            if let Some((taken, status)) = cached.as_ref()
                && taken.elapsed() < STATUS_CACHE_TTL
            {
                return status.clone();
            }
        }
        let status = self.read_status();
        *self.status.lock() = Some((Instant::now(), status.clone()));
        status
    }

    fn read_status(&self) -> IndexStatus {
        let Ok(db) = self.reader() else {
            return IndexStatus::default();
        };
        let Ok(roots) = db.roots() else {
            return IndexStatus::default();
        };
        IndexStatus {
            available: true,
            roots: roots
                .into_iter()
                .map(|root| IndexedRoot {
                    path: ferret_vfs::VfsPath::local(&root.path),
                    state: root.state,
                    files: root.files,
                    last_reconciled_unix: root.last_reconciled,
                })
                .collect(),
            files: db.total_files().unwrap_or(0),
            content_documents: self.content.document_count(),
            bytes_on_disk: directory_size(&self.dir),
        }
    }
}

fn quarantine(dir: &Path, reason: &SearchError) -> Result<()> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let aside = dir.with_extension(format!("quarantine-{stamp}"));
    std::fs::rename(dir, &aside).map_err(|error| SearchError::IndexOpen {
        path: ferret_vfs::VfsPath::from(dir),
        message: format!(
            "{reason}; and moving it to {} failed: {error}",
            aside.display()
        ),
    })
}

fn directory_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => directory_size(&entry.path()),
            _ => entry.metadata().map(|metadata| metadata.len()).unwrap_or(0),
        })
        .sum()
}

/// Crawls the given roots to completion, synchronously.
///
/// The same code [`Indexer`] runs on its own thread, exposed for callers that
/// need the index *finished* rather than *maintained* — a one-shot rebuild,
/// and every test in this module, which would otherwise have to poll for a
/// root to reach [`RootState::Ready`] and be flaky about it.
///
/// Not for use alongside a running [`Indexer`] over the same directory: SQLite
/// permits one writer, and the second would spend its time in `busy_timeout`.
pub fn crawl_once(index: &Index, roots: &[PathBuf]) -> Result<()> {
    let db = Db::open_writer(&index.db_path)?;
    let mut content = index.content().writer()?;
    let stop = AtomicBool::new(false);
    let registered = register(&db, roots)?;
    initial_pass(&db, &mut content, &registered, &stop)?;
    content.commit()?;
    index.content.reload();
    // The cached status predates the crawl, and a caller that just asked for a
    // synchronous crawl will ask what is in the index next.
    *index.status.lock() = None;
    Ok(())
}

fn register(db: &Db, roots: &[PathBuf]) -> Result<Vec<db::RootRow>> {
    let mut registered = Vec::with_capacity(roots.len());
    for root in roots {
        let Some(text) = root.to_str() else { continue };
        registered.push(db.upsert_root(text)?);
    }
    Ok(registered)
}

/// The first sweep of each root, which for a fresh index is the initial crawl
/// and for an existing one is startup reconciliation. Same walk either way —
/// see [`crawl`].
fn initial_pass(
    db: &Db,
    content: &mut content::ContentWriter,
    registered: &[db::RootRow],
    stop: &AtomicBool,
) -> Result<()> {
    for root in registered {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        db.set_root_state(root.id, RootState::Crawling)?;
        let outcome = crawl::sweep_root(db, content, root, stop)?;
        if outcome.interrupted {
            return Ok(());
        }
        crawl::backfill_content(db, content, root, stop)?;
        db.set_root_state(root.id, RootState::Ready)?;
    }
    Ok(())
}

/// The single writer: crawl, watch, reconcile.
///
/// Owning all three in one thread is deliberate. They contend for the same two
/// writers and the same disk, and a scheduler that let them run concurrently
/// would need locks around both stores — at which point a reconciliation sweep
/// could stall an incremental update behind a two-minute walk.
pub struct Indexer {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Indexer {
    pub fn start(index: Arc<Index>, roots: Vec<PathBuf>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("ferret-search-index".into())
            .spawn(move || {
                // A failure here leaves the daemon serving live results, which
                // is the whole point of the index being optional.
                let _ = maintain(&index, roots, &thread_stop);
            })
            .map_err(|error| SearchError::IndexWrite {
                message: format!("couldn't start the indexer thread: {error}"),
            })?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // Joined rather than detached: the thread holds the SQLite writer,
            // and letting the process exit mid-transaction is how a store ends
            // up needing the quarantine path on the next launch.
            let _ = handle.join();
        }
    }
}

/// How often the indexer wakes when the filesystem is quiet.
///
/// Short enough that a settled debounce window is applied promptly, long
/// enough that an idle daemon is not measurably running.
const IDLE_POLL: Duration = Duration::from_millis(250);

fn maintain(index: &Index, roots: Vec<PathBuf>, stop: &AtomicBool) -> Result<()> {
    let db = Db::open_writer(&index.db_path)?;
    let mut content = index.content().writer()?;
    let registered = register(&db, &roots)?;
    initial_pass(&db, &mut content, &registered, stop)?;
    content.commit()?;
    index.content().reload();

    let watchers = RootWatchers::start(&roots)?;
    let mut queue = DirtyQueue::default();
    let mut last_reconcile = Instant::now();

    while !stop.load(Ordering::Acquire) {
        match watchers.events.recv_timeout(IDLE_POLL) {
            Ok(signal) => queue.absorb(signal),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if !queue.is_empty() && queue.is_due() {
            let (paths, desynced) = queue.take();
            if desynced {
                reconcile(&db, &mut content, &registered, stop)?;
                last_reconcile = Instant::now();
            } else {
                apply(&db, &mut content, &registered, &paths, stop)?;
            }
            content.commit()?;
            index.content().reload();
        }

        if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
            reconcile(&db, &mut content, &registered, stop)?;
            content.commit()?;
            index.content().reload();
            last_reconcile = Instant::now();
        }
    }
    content.commit()
}

fn apply(
    db: &Db,
    content: &mut content::ContentWriter,
    roots: &[db::RootRow],
    paths: &[PathBuf],
    stop: &AtomicBool,
) -> Result<()> {
    let mut touched: Vec<&db::RootRow> = Vec::new();
    for path in paths {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if is_excluded(path) {
            continue;
        }
        let Some(root) = roots
            .iter()
            .find(|root| path.starts_with(Path::new(&root.path)))
        else {
            continue;
        };
        crawl::apply_change(db, content, root, path, stop)?;
        if !touched.iter().any(|seen| seen.id == root.id) {
            touched.push(root);
        }
    }
    for root in touched {
        crawl::backfill_content(db, content, root, stop)?;
    }
    Ok(())
}

fn reconcile(
    db: &Db,
    content: &mut content::ContentWriter,
    roots: &[db::RootRow],
    stop: &AtomicBool,
) -> Result<()> {
    for root in roots {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        // The state is not moved back to `Crawling`: the rows already stored
        // are still a valid answer while the sweep runs, and dropping coverage
        // every fifteen minutes would send every query to the live path for
        // the duration of the sweep.
        let outcome = crawl::sweep_root(db, content, root, stop)?;
        if outcome.interrupted {
            return Ok(());
        }
        crawl::backfill_content(db, content, root, stop)?;
        db.set_root_state(root.id, RootState::Ready)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
