//! Every tunable in this crate.
//!
//! Each constant carries the reasoning for its value, so a number nobody
//! remembers choosing can still be changed safely.

/// How many entries the measuring walk will count before giving up on an exact
/// total.
///
/// A million files is roughly 3% of a modern disk, and counting it means a stat
/// per entry — seconds of work whose only product is a nicer progress bar, spent
/// before the first byte is copied. Past this the walk stops and reports
/// [`crate::JobProgress::bytes_total`] as `None`, which the panel draws as a
/// live count with no percentage. A slightly wrong denominator is worse than no
/// denominator: it is one that reaches 100% while work remains.
pub const MEASURE_ENTRY_CAP: usize = 1_000_000;

/// How many jobs may run at once.
///
/// One, deliberately. Two concurrent copies of large trees on one disk make
/// both of them slower without finishing either sooner, and the queue's whole
/// promise is that a job is *visible* — a second one silently sharing the disk
/// makes the first one's throughput number a lie. A job that is not the
/// bottleneck (a delete, a small copy) is held for the length of a large one,
/// which is the cost of the guarantee, and it is the same trade every
/// serialised queue makes.
pub const MAX_CONCURRENT_JOBS: usize = 1;

/// How many finished jobs the queue keeps before dropping the oldest.
///
/// Bounded because the queue outlives navigation and quitting is not the only
/// way it grows: a person copying files all afternoon would otherwise
/// accumulate a list nobody will ever scroll. Well past the number of jobs
/// anyone scrolls back through, and small enough that the panel can render its
/// whole contents without virtualising.
pub const HISTORY_LIMIT: usize = 64;

/// How often the worker publishes progress, in bytes of work done rather than
/// in wall-clock time.
///
/// Publishing per chunk would be one event per mebibyte of a large copy — tens
/// of thousands of them — and the panel cannot show more than about ten a
/// second anyway. Measured in bytes so a slow operation still reports: a
/// threshold in time would leave a job moving at 40 KB/s silent for its whole
/// life, which is the exact case a progress bar exists to cover.
pub const PROGRESS_STEP_BYTES: u64 = 512 * 1024;

/// The window throughput is averaged over.
///
/// Five seconds. Long enough that the number does not swing wildly with a
/// single slow write — a UI that reads "3 MB/s, 40 MB/s, 2 MB/s" is not
/// reporting throughput, it is reporting noise — and short enough that the
/// estimate left is still roughly true by the time it is read. The first sample
/// waits for this window rather than dividing the first chunk by the time it
/// took, which on a cold cache is almost entirely seek latency.
pub const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a paused job's reporter blocks between checks.
///
/// The pause is a thread sitting still, not a spin: 50 ms is short enough that
/// a resumed copy is moving essentially immediately, and long enough that a job
/// left paused overnight does not wake a worker two thousand times a minute.
/// A cancel still lands within this, which is bounded and imperceptible.
pub const PAUSE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How many files a batch rename may plan.
pub const MAX_RULES: usize = 12;
/// Widest a sequential-numbering affix may grow.
pub const MAX_NUMBER_WIDTH: usize = 12;
