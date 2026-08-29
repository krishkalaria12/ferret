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

/// Largest batch of entries in one item of a directory listing stream.
///
/// A 200,000-entry directory reaches the UI in roughly fifty model updates
/// rather than one per row: large enough to amortise the channel send, the
/// task wake and the model update.
///
/// This is a *ceiling*, reached by doubling from [`FIRST_LISTING_BATCH`] —
/// see there for why it is not also the first batch.
pub const LISTING_BATCH_SIZE: usize = 4_096;

/// Entries in the first batch, before the ramp doubles towards
/// [`LISTING_BATCH_SIZE`].
///
/// The constant above was chosen when a batch was names only. D-33 changed
/// what a batch *is*: every entry now carries a resolved `stat`, so the batch
/// size also decides how long the first row waits — and a flat 4,096 made that
/// 4,096 stats, taking time-to-first-row from 2.0 ms to 5.8 ms.
///
/// Ramping keeps both properties: the first rows appear almost immediately,
/// and the steady state is still one update per few thousand entries. It also
/// bounds what a cancelled walk leaves in flight to one batch, which is
/// smallest exactly when a walk is most likely to be abandoned — right at the
/// start, while someone is typing past a directory.
pub const FIRST_LISTING_BATCH: usize = 128;

/// Threads in Ferret's own worker pool ([`crate::pool`]).
///
/// Sized from the machine rather than fixed, but bounded: the work on this
/// pool is `stat`-bound rather than CPU-bound, so past a point more threads
/// only add contention with smol's blocking pool and GPUI's executor, which
/// are also sized from the core count.
pub fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .clamp(2, MAX_WORKER_THREADS)
}

/// Ceiling for [`worker_threads`]. A 64-core machine gains nothing from 64
/// threads issuing `stat` calls at one disk.
pub const MAX_WORKER_THREADS: usize = 16;

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
