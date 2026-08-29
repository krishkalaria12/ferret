//! Every tunable for the fixture and the benchmark.
//!
//! Kept together so the shape of the fixture and the budgets it is measured
//! against can be read and changed in one place. Raising a budget is a decision
//! worth seeing in a diff on its own.

use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixture shape
// ---------------------------------------------------------------------------

/// Entries in `flat/`. The scroll-performance target from the plan.
pub const FLAT_ENTRIES: usize = 200_000;

/// Nesting attempted in `deep/`.
///
/// The platform stops this short — `PATH_MAX` (1024) on macOS, 260 on Windows
/// without long-path support — and where it stops is the interesting part.
pub const DEEP_LEVELS: usize = 512;

/// Apparent size of `sparse/huge.bin`.
///
/// `set_len` leaves a hole on both APFS and NTFS, so this allocates almost
/// nothing while still making any code that reads a file whole fall over.
pub const SPARSE_LEN: u64 = 4 * 1024 * 1024 * 1024;

/// Extensions used across `flat/` and `types/`, for icon dispatch and sorting.
pub const EXTENSIONS: &[&str] = &[
    "rs", "toml", "lock", "md", "txt", "json", "yaml", "yml", "html", "css", "js", "ts", "tsx",
    "py", "go", "c", "h", "cpp", "hpp", "java", "kt", "swift", "rb", "php", "sh", "zsh", "fish",
    "sql", "csv", "tsv", "parquet", "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico",
    "pdf", "zip", "tar", "gz", "zst", "7z", "mp3", "wav", "flac", "mp4", "mov", "mkv", "ttf",
    "otf", "woff2", "so", "dylib", "dll", "exe", "bin",
];

/// Upper bound on threads used to create `flat/`. Creating 200,000 files is the
/// slow part of the fixture; past this the filesystem, not the CPU, is the limit.
pub const FIXTURE_MAX_THREADS: usize = 8;

// ---------------------------------------------------------------------------
// Budgets — exceeding one fails the bench
// ---------------------------------------------------------------------------

/// How long the first row may take to arrive.
///
/// The one number that decides whether the app feels alive, and the entire
/// justification for streaming listings. Measured at ~0.3 ms; the budget is
/// generous so it catches a regression in kind — something collecting the
/// listing into a `Vec` — rather than machine-to-machine variation.
pub const FIRST_ROW_BUDGET: Duration = Duration::from_millis(50);

/// How long the full 200,000-entry listing may take. Measured at ~143 ms.
pub const FLAT_TOTAL_BUDGET: Duration = Duration::from_secs(3);

/// How long until every row in `flat/` can draw its size and its date.
///
/// The number the other two budgets do not watch. Time-to-first-row says
/// whether the app feels alive; this says whether the list you are looking at
/// is a file list or a file list that fills itself in for the next two minutes
/// (`docs/audit.html` §11). It is what A-03 destroyed and what D-33 fixes, and
/// on a backend that resolves metadata during the walk it collapses into
/// [`FLAT_TOTAL_BUDGET`].
pub const FLAT_POPULATED_BUDGET: Duration = Duration::from_secs(5);

/// Runs per fixture; the reported figure is the median.
///
/// A single sample on a machine that is also compiling something is noise, and
/// noise is what forces a budget to be set loose enough to miss a real 2x
/// regression. Three is enough to throw out one outlier and cheap enough to
/// stay a command people actually run.
pub const BENCH_RUNS: usize = 3;

/// Fixture directories included in the benchmark sweep.
///
/// `links` earns its place: a cyclic symlink must not hang the walk, and if
/// that regresses the row simply never prints.
pub const BENCH_FIXTURES: &[&str] = &["flat", "types", "realistic", "names", "links"];

// ---------------------------------------------------------------------------
// Location
// ---------------------------------------------------------------------------

/// Where the fixture is built.
///
/// Outside the repository on purpose: 200,000 files inside a Cargo workspace
/// makes every `cargo` invocation and every editor file-watcher crawl.
pub fn default_root() -> PathBuf {
    std::env::temp_dir().join("ferret-fixture")
}
