//! Every tunable in this crate.
//!
//! Constants that change behaviour live here rather than beside the code that
//! reads them, so the knobs can be found and reasoned about together instead of
//! being discovered one `grep` at a time. Each carries the reasoning for its
//! value — a number with no explanation is one nobody can safely change.
//!
//! This is for *tunables*, not for values that are part of a type's contract.
//! An enum discriminant or a protocol constant belongs with its type.

/// How many batches may sit in a listing channel before the producer parks.
///
/// Bounded on purpose. Unbounded would let a 200,000-entry walk materialise the
/// whole directory in memory, which is the thing streaming exists to avoid. At
/// [`LISTING_BATCH_SIZE`] entries per batch, this cap still bounds the queued
/// rows while avoiding one send and receive per directory entry. Two batches
/// let disk I/O stay one model update ahead without turning a slow UI into an
/// unbounded in-memory listing.
pub const LISTING_BUFFER: usize = 2;

/// How many filesystem notifications may wait for the UI before it must
/// resynchronise with a fresh listing.
///
/// Native watcher threads must never wait for the UI: doing so can stall the
/// operating system's event delivery. This small buffer absorbs ordinary save
/// bursts; a full buffer is explicitly reported as [`crate::WatchEvent::Desynced`]
/// so the consumer can converge with one full reload instead of silently
/// keeping an incomplete picture.
pub const WATCH_EVENT_BUFFER: usize = 64;

/// Entries grouped into one item of a directory listing stream.
///
/// A 200,000-entry directory reaches the UI in roughly fifty model updates,
/// rather than one per row. This is large enough to amortize channel and
/// executor overhead, but small enough that the first visible rows do not wait
/// for a complete directory walk.
pub const LISTING_BATCH_SIZE: usize = 4_096;

/// Paths resolved by one [`crate::Vfs::metadata_batch`] call.
///
/// A batch amortises the dispatch to a blocking pool without holding back so
/// many results that Size and Date appear frozen on a large directory. It
/// lives here rather than beside the file pane because it is a property of how
/// `metadata_batch` should be called, and the benchmark has to make the same
/// call the pane does or it is measuring something else.
pub const METADATA_BATCH_SIZE: usize = 256;

/// Bytes read when sniffing a file's type or building a preview.
///
/// Nothing may read a whole file speculatively: arrow-keying down a directory
/// past a 4 GB video must not pull 4 GB into memory.
pub const SNIFF_BUDGET: usize = 8 * 1024;
