//! Tunables for search traversal, the persistent index, and the daemon.

use std::time::Duration;

/// Search events waiting for a consumer before the producer parks.
///
/// Results arrive in batches, so four slots let the filesystem stay ahead of
/// one slow frame without materialising an unbounded search in memory.
pub const EVENT_BUFFER: usize = 4;

/// Results delivered in one model update.
///
/// Sixty-four amortises channel wakeups while keeping the first visible batch
/// small. The final partial batch is flushed as soon as traversal ends.
pub const RESULT_BATCH_SIZE: usize = 64;

/// Default hard cap for one search session.
///
/// A broad query over a home directory can match millions of entries. The UI
/// needs a useful working set and an honest truncated notice, not every path
/// resident in memory.
pub const DEFAULT_MAX_RESULTS: usize = 10_000;

/// Longest accepted filename pattern, in UTF-8 bytes.
///
/// Filename search does not benefit from enormous interactive patterns. The
/// limit also bounds regex compilation when regex name search lands.
pub const MAX_PATTERN_BYTES: usize = 4 * 1024;

/// Longest accepted content pattern, in UTF-8 bytes.
///
/// Content regex compilation can allocate more internal state than filename
/// substring search, so it gets its own explicit interactive budget.
pub const MAX_CONTENT_PATTERN_BYTES: usize = 8 * 1024;

/// Approximate heap budget for one file being searched by the live grep path.
///
/// `grep-searcher` grows its line buffer for long lines. A hard cap prevents a
/// minified or malformed file from becoming an unbounded interactive query.
pub const CONTENT_SEARCH_HEAP_LIMIT: usize = 8 * 1024 * 1024;

/// Approximate compiled regex program budget for a content query.
///
/// This keeps pathological regex input from building very large automata in an
/// interactive field while still leaving comfortable room for real code search.
pub const CONTENT_REGEX_SIZE_LIMIT: usize = 4 * 1024 * 1024;

/// Maximum bytes retained for one content-match preview.
///
/// A source file may contain a generated or malformed line that is many
/// megabytes long. Search remains streaming, and this cap prevents one result
/// row from retaining that entire line in the UI model.
pub const MAX_PREVIEW_BYTES: usize = 512;

/// Maximum highlighted occurrences retained for one matching line.
///
/// A pattern such as `.` can match every character. The row only needs enough
/// ranges to explain the preview and must not allocate in proportion to a
/// pathological source line.
pub const MAX_RANGES_PER_PREVIEW: usize = 32;

/// Maximum non-fatal filesystem warnings emitted by one search.
///
/// Permission failures can occur for thousands of paths in a broad home or
/// volume search. A small cap keeps the bounded event stream useful for
/// results while the skipped counter still records every failure.
pub const MAX_WARNINGS: usize = 8;

/// Rows written to SQLite inside one crawl transaction.
///
/// One transaction per file makes the initial crawl fsync-bound and takes
/// hours over a home directory; one transaction for the whole crawl loses
/// everything if the daemon is killed halfway. A few thousand rows is roughly
/// a millisecond of commit and bounds how much a crash can cost.
pub const CRAWL_BATCH: usize = 2_048;

/// Largest file whose contents enter the Tantivy index.
///
/// Above this the index would grow faster than it helps: multi-gigabyte logs
/// and datasets dominate the segment size while almost never being the file a
/// person is looking for. They stay findable by name, and by the live regex
/// path, which reads them without retaining them.
pub const MAX_INDEXED_CONTENT_BYTES: u64 = 4 * 1024 * 1024;

/// Tantivy writer heap, in bytes.
///
/// The writer buffers postings here and flushes a segment when it fills, so
/// this trades resident memory during indexing against segment count (and so
/// against merge work and query fan-out). 64 MiB is Tantivy's own recommended
/// floor for a multithreaded writer.
pub const CONTENT_WRITER_HEAP: usize = 64 * 1024 * 1024;

/// Documents Tantivy commits before the reader is reloaded during a crawl.
///
/// A commit is what makes documents searchable, and it is not free. Batching
/// keeps the initial crawl from spending its time in fsync while still making
/// an in-progress index progressively useful.
pub const CONTENT_COMMIT_BATCH: usize = 4_096;

/// Filename candidates fetched from SQLite at a time.
///
/// A page bounds query-side materialisation without limiting the complete
/// result set. Unselective queries fetch subsequent pages until the user's
/// result cap is reached or the scope is exhausted.
pub const INDEX_CANDIDATE_PAGE_SIZE: usize = 4_096;

/// Ranked content candidates fetched from Tantivy at a time.
///
/// Re-reading candidates to build current snippets is deliberately incremental
/// so cancellation can interrupt a broad query between bounded batches.
pub const CONTENT_CANDIDATE_PAGE_SIZE: usize = 256;

/// Shortest query the trigram index can answer.
///
/// FTS5's trigram tokenizer indexes three-character windows, so a one- or
/// two-character pattern has no token to look up. Shorter queries fall back to
/// a bounded `LIKE` scan rather than silently returning nothing.
pub const MIN_TRIGRAM_CHARS: usize = 3;

/// How long the indexer waits for a filesystem event burst to settle.
///
/// Longer than the file pane's 150 ms debounce on purpose: a stale row in a
/// search result is cheaper than a re-index storm, and a compile or a `git
/// checkout` produces thousands of events over several hundred milliseconds.
pub const INDEX_DEBOUNCE: Duration = Duration::from_millis(400);

/// Paths held for re-indexing before the queue declares itself desynced.
///
/// Past this the individual paths have stopped being a complete account of
/// what changed, and a reconciliation sweep is both cheaper and more correct
/// than applying a partial list.
pub const DIRTY_QUEUE_CAPACITY: usize = 16_384;

/// How often each indexed root is reconciled against the filesystem.
///
/// Native watchers drop and coalesce events — FSEvents under load, and
/// `ReadDirectoryChangesW` whenever its buffer overflows — so freshness cannot
/// rest on them alone. This is the backstop that converges the index to disk
/// truth; every root is also reconciled once at startup.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long the daemon stays alive with no client connected.
///
/// The index is only worth maintaining for a running Ferret. Exiting releases
/// the watchers and the SQLite handle rather than leaving a process watching a
/// home directory nobody is searching.
pub const DAEMON_IDLE_SHUTDOWN: Duration = Duration::from_secs(30 * 60);

/// Largest single IPC frame, in bytes.
///
/// A result batch is bounded by [`RESULT_BATCH_SIZE`] and previews by
/// [`MAX_PREVIEW_BYTES`], so a legitimate frame is far below this. The limit
/// exists so a corrupt length prefix cannot make the reader allocate a
/// gigabyte before it discovers the connection is broken.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// How long a client waits for a freshly launched daemon to accept.
///
/// Cold start has to open SQLite and Tantivy before it binds. Past this the
/// client stops waiting and searches live, which is a slower answer rather
/// than no answer.
pub const DAEMON_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client waits for a freshly launched daemon to accept.
///
/// Cold start has to open SQLite and Tantivy before it binds. Past this the
/// client stops waiting and searches live, which is a slower answer rather
/// than no answer.
pub const DAEMON_LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between connection attempts while waiting for a launching daemon.
pub const DAEMON_CONNECT_RETRY: Duration = Duration::from_millis(25);

/// Consecutive daemon launches allowed before the client gives up for good.
///
/// A daemon that crashes on startup — an incompatible index it cannot
/// quarantine, a missing binary — must not be relaunched once per keystroke.
/// After this many failures the client stays on the live path for the rest of
/// the session and says so.
pub const DAEMON_LAUNCH_ATTEMPTS: u32 = 3;

/// Entries one live search submits to the shared filesystem pool per batch.
///
/// Leave one logical CPU for GPUI and cap the fan-out because broad content
/// search is usually storage-bound before it is CPU-bound. `ignore` walks the
/// tree serially for its ignore semantics; each bounded batch then runs on
/// `ferret_vfs::pool`, so concurrent queries share the same global limit.
pub fn live_search_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .saturating_sub(1)
        .clamp(1, 8)
}
