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
