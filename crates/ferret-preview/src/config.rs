//! Every budget in the preview pipeline.
//!
//! Per the repository convention these live here rather than beside the code
//! that reads them, each documented with why the number is what it is. The
//! user-facing subset (pane visible, width, line numbers) belongs to
//! `ferret-settings` beside `sidebar` and `terminal`, and lands with the code
//! that reads it — never before (D-41).

use std::time::Duration;

/// How long a selection must sit still before its preview is requested.
///
/// Below the ~110 ms a deliberate single arrow-press takes, above the ~30 ms
/// of key-repeat. One read for a sweep, no perceptible delay for a single
/// move.
pub const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(80);

/// How long a read may take before the pane admits it is working.
///
/// Under this the skeleton would flash and be gone. Local reads almost never
/// reach it; remote ones always do.
pub const LOADING_THRESHOLD: Duration = Duration::from_millis(120);

/// Bytes read to decide what a file is.
///
/// Every magic number we care about is in the first 32 bytes; 8 KiB is what
/// a binary/UTF-8 decision needs to be reliable. The final bounded read
/// rereads this prefix — a second read, deliberately: reading 2 MiB of a
/// 4 GB video to discover it is a video is the alternative.
pub const SNIFF_BYTES: usize = 8 * 1024;

/// Largest text file previewed whole.
///
/// Comfortably past any hand-written source file, well short of a log. Past
/// it the head is shown with an explicit "first 2 MiB of N" banner.
pub const TEXT_BUDGET: usize = 2 * 1024 * 1024;

/// Largest rendered-Markdown source.
///
/// An initial rendered-mode limit; structural limits (blocks, depth) and
/// layout measurements decide whether to raise it. Source fallback remains
/// available past it.
pub const MARKDOWN_BUDGET: usize = 256 * 1024;

/// Lines handed to the editor, at most.
///
/// Not a preference. Upstream states the editor targets "about 50K lines";
/// 20,000 is a conservative starting point. First-paint performance still
/// requires measurement.
pub const TEXT_LINE_CAP: usize = 20_000;

/// Longest single displayed line.
///
/// A minified one-line file must not reach layout whole: past this the line
/// is cut with an explicit marker before the editor ever sees it.
pub const TEXT_MAX_LINE_BYTES: usize = 16 * 1024;

/// Bytes sniffed for CSV delimiter scoring and header detection.
///
/// Enough rows for the consistency score to be stable on wide exports.
pub const TABLE_SNIFF_BYTES: usize = 64 * 1024;

/// Largest CSV/TSV sample parsed.
///
/// Maximum records in the bounded sample; no automatic full-file loading.
/// A 500 MB file opens as an explicitly labelled sample. True incremental
/// loading needs a future cancellable streaming/range VFS contract.
pub const TABLE_INITIAL_ROWS: usize = 2_000;

/// Largest CSV/TSV input read.
pub const TABLE_BUDGET: usize = 2 * 1024 * 1024;

/// Widest a parsed table may be.
pub const TABLE_MAX_COLUMNS: usize = 128;

/// Largest single CSV field kept.
pub const TABLE_MAX_FIELD_BYTES: usize = 64 * 1024;

/// Largest retained cell data across a parsed sample.
pub const TABLE_MAX_CELL_BYTES: usize = 32 * 1024 * 1024;

/// Largest image file decoded in-process.
///
/// Covers essentially every photograph; refuses the pathological. Larger
/// sources need proven decoder-level subsampling, otherwise the native
/// thumbnail path or an explicit TooLarge state.
pub const IMAGE_BUDGET: usize = 64 * 1024 * 1024;

/// Largest image dimensions decoded to a full raster.
///
/// A conservative fallback for full-raster decoders under the memory budget.
/// Checked from the header *before* any allocation happens.
pub const IMAGE_SOURCE_PIXEL_CAP: u64 = 16_000_000;

/// Long edge of the downscaled bitmap handed to the view.
///
/// Sharp at any pane width on a 2x display, bounded regardless of how wide
/// the window gets.
pub const IMAGE_TARGET_LONG_EDGE: u32 = 4096;

/// Most animation frames retained.
///
/// Ten seconds at 24 fps. Past it, the first frame and a footer note. Memory
/// is the other bound (32 MiB retained); whichever limit comes first wins.
pub const ANIMATION_FRAME_CAP: usize = 240;

/// Bytes retained for animation frames in aggregate.
pub const ANIMATION_MEMORY_CAP: usize = 32 * 1024 * 1024;

/// Bytes shown by the hex floor.
///
/// 4,096 rows of hex. Nobody scrolls further, and it bounds the one view
/// with no structure to virtualise against.
pub const HEX_BUDGET: usize = 64 * 1024;

/// Largest PDF read whole for page rendering.
///
/// PDFs need random access and can place their index at the end, so a
/// truncated head is useless: within budget the complete bytes are loaded,
/// past it an explicit TooLarge state.
pub const PDF_BUDGET: usize = 64 * 1024 * 1024;

/// Largest file handed to a native thumbnail service by path.
///
/// Native services receive paths and may read the whole file internally, so
/// this is a request limit, not a byte budget — Rust read-budget guarantees
/// do not apply to OS APIs. Documented exception, not an oversight.
pub const NATIVE_THUMBNAIL_INPUT_LIMIT: usize = 64 * 1024 * 1024;

/// Scale applied to every byte budget on a non-local backend.
///
/// A 2 MiB text budget over a 200 ms-RTT link is several seconds of waiting
/// for something the user was arrow-keying past; 512 KiB is a preview.
pub const REMOTE_BUDGET_SCALE_NUM: usize = 1;
pub const REMOTE_BUDGET_SCALE_DEN: usize = 4;

/// Apply [`REMOTE_BUDGET_SCALE_NUM`]`/`[`REMOTE_BUDGET_SCALE_DEN`] to a
/// budget, rounding up so a small cap never becomes zero.
pub fn scale_for_remote(budget: usize) -> usize {
    (budget * REMOTE_BUDGET_SCALE_NUM).div_ceil(REMOTE_BUDGET_SCALE_DEN)
}
