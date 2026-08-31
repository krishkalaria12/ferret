//! Tunables for search traversal and result delivery.

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

/// Worker count for one live parallel search.
///
/// Leave one logical CPU for GPUI and cap the fan-out because broad content
/// search is usually storage-bound before it is CPU-bound. `ignore` owns both
/// traversal and its worker threads, so this is a single bounded pool per
/// active live query rather than an unbounded task per path.
pub fn live_search_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(2)
        .saturating_sub(1)
        .clamp(1, 8)
}
