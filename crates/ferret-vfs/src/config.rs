//! Every tunable in this crate.
//!
//! Constants that change behaviour live here rather than beside the code that
//! reads them, so the knobs can be found and reasoned about together instead of
//! being discovered one `grep` at a time. Each carries the reasoning for its
//! value — a number with no explanation is one nobody can safely change.
//!
//! This is for *tunables*, not for values that are part of a type's contract.
//! An enum discriminant or a protocol constant belongs with its type.

/// How many entries may sit in a listing channel before the producer parks.
///
/// Bounded on purpose. Unbounded would let a 200,000-entry walk materialise the
/// whole directory in memory, which is the thing streaming exists to avoid.
/// Large enough that the consumer is never starved between frames; small enough
/// that the buffer is a rounding error against the rows already on screen.
pub const LISTING_BUFFER: usize = 512;

/// How many folders keep their view preferences before the oldest is evicted.
///
/// Roughly 500 KB on disk and a sub-10 ms load. Windows Explorer caps its
/// equivalent store too — the requirement is "remember the folders I use", not
/// "remember every folder forever", and an unbounded file would grow for the
/// life of the install. See D-16.
pub const FOLDER_PREFS_CAPACITY: usize = 4_000;

/// Bytes read when sniffing a file's type or building a preview.
///
/// Nothing may read a whole file speculatively: arrow-keying down a directory
/// past a 4 GB video must not pull 4 GB into memory.
pub const SNIFF_BUDGET: usize = 8 * 1024;
