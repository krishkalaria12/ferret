//! The allocations-per-frame budget from `docs/audit.html` §11.
//!
//! Every finding in §3 — the OS icon rebuilt per row, the drag payload that
//! scanned the listing, the status bar that recounted it, the `Entry` clone
//! that was two heap allocations — leaves a wall-clock benchmark on a fast
//! machine essentially untouched, and is glaring in an allocation count. This
//! is the one number that would have caught them the day they were written,
//! and as an assertion it fails a build instead of scrolling past in a log.
//!
//! # What it measures, precisely
//!
//! Not a rendered frame. There is no GPUI paint pass; what this drives is
//! **the questions a frame asks of the model and the icon tables** for one
//! viewport of rows, with everything selected — which is where all seven of
//! the findings above lived. A regression in GPUI's own element tree would not
//! show up, and this file should not pretend otherwise.
//!
//! It does run under a `TestAppContext`, which is what lets it reach
//! `os_icon` — the platform icon lookup. That used to be skipped for want of
//! an `App`, and A-33 walked straight through the hole: a `String` allocated
//! per row per frame to build a cache key, while this budget passed at 2.00.
//! A documented hole in the one check whose job is to have none is still a
//! hole.
//!
//! A selection pass runs alongside the scroll, because A-29 — a full rescan of
//! the listing on every selection change — was also invisible to a budget that
//! only scrolled.
//!
//! # The floor
//!
//! It is not zero and should not be. The Date and Size cells turn a
//! `SystemTime` and a `u64` into text, and that is a `String` each — real work
//! for a real result, done once per visible cell. Two allocations per row is
//! the floor; the budget is set just above it, so anything that reintroduces a
//! per-row clone, scan, format or lock fails immediately.
//!
//! Run with `just budget`.

use super::*;
use crate::browser::{GitHandle, GitSnapshot};
use crate::icons::FileIcons;
use ferret_vfs::{EntryKind, Metadata};
use gpui::AppContext as _;

/// Rows on screen at once. Roughly a maximised list view at 30 px rows; the
/// exact figure does not matter because the budget is per row.
const VISIBLE_ROWS: usize = 40;

/// Rows in the directory being scrolled. Large enough that anything O(the
/// directory) per row is unmissable, small enough to build quickly.
const DIRECTORY_ROWS: usize = 20_000;

/// Frames to average over.
const FRAMES: usize = 20;

/// One row in this many carries a git badge in the budget's repository.
///
/// Busier than most real trees, and deliberately: a lookup that missed on
/// every row would exercise only the map's empty path.
const CHANGED_EVERY: usize = 3;

/// Heap allocations one visible row may cost per frame.
///
/// Two are the Date and Size strings (see the module header). The third is
/// headroom for allocator bookkeeping and rounding, not licence for a third
/// per-row allocation. Anything O(the directory) lands orders of magnitude
/// above this, which is the point — this is a tripwire, not a stopwatch.
const ALLOCATIONS_PER_ROW: u64 = 3;

/// A populated status snapshot over the same names `folder_of` generates.
///
/// Built by parsing real porcelain v2 rather than by hand, so the budget
/// measures the index the application actually builds — including the
/// absolute-path keys, which are what a row is looked up by.
fn git_of(n: usize) -> GitHandle {
    let mut output = b"# branch.head main\0".to_vec();
    for i in (0..n).step_by(CHANGED_EVERY) {
        output.extend_from_slice(
            format!("1 .M N... 100644 100644 100644 aaaa bbbb file-{i:06}.txt\0").as_bytes(),
        );
    }
    let status = ferret_git::RepoStatus::parse(&output, std::path::Path::new("/x"));
    assert!(
        status.index.tracked_len() > 0,
        "the budget must run against a populated index, not an empty one"
    );
    // No untracked-root fallback: every row here is a tracked file, so the
    // budget measures the index lookup itself rather than the `starts_with`
    // shortcut in front of it.
    std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(GitSnapshot::from_status(
        status, None,
    )))
}

fn folder_of(n: usize) -> Folder {
    let mut f = Folder::new(
        VfsPath::local("/x"),
        SortRules::by_name(),
        true,
        ListingProfile::BROWSING,
    );
    for i in 0..n {
        let name = format!("file-{i:06}.txt");
        let entry = Entry::new(VfsPath::local(format!("/x/{name}")), &name, EntryKind::File)
            .with_metadata(Some(Metadata {
                len: i as u64 * 977,
                modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(i as u64)),
                ..Default::default()
            }));
        let id = f.insert(entry, Placement::Top);
        f.entries.push(id);
    }
    f.loading = false;
    f.resort();
    f.select_all();
    f
}

/// One frame's worth of what the list delegate asks for, for `count` rows
/// starting at `first`.
///
/// Mirrors `render_tr` and `render_td` in `pane/list.rs`: the row decides its
/// selection fill and its drag source, then the Name, Date and Size cells each
/// ask the model something. The results are folded into a return value so
/// nothing here can be optimised away.
fn draw_rows(
    folder: &Folder,
    icons: &FileIcons,
    git: &GitHandle,
    cx: &gpui::App,
    first: usize,
    count: usize,
) -> usize {
    let mut sink = 0usize;
    for row in first..first + count {
        // render_tr: the fill, and the payload a drag would carry.
        let selected = folder.is_row_selected(row);
        sink += selected as usize;
        if selected {
            sink += folder.selected_paths_shared().len();
        }

        // Every cell works from its own clone of the entry (D-35).
        let Some(entry) = folder.entry_at(row).cloned() else {
            continue;
        };

        // Name: indent, disclosure, icon, label.
        sink += folder.depth_at(row) as usize;
        sink += folder.expansion_at(row).is_some() as usize;
        sink += icons.asset_path(&entry).len();
        sink += gpui::SharedString::new(entry.name()).len();
        // The platform icon lookup, which is where A-33 hid. A cache miss
        // returns `None` and the row draws the bundled SVG — but the *key* is
        // built either way, which is the part that was allocating.
        sink += crate::icons::os_icon(&entry, cx).is_some() as usize;
        // The git badge (F-51). One atomic load and one hash of a path the
        // entry already owns — this must add nothing to the count, and Gate 7
        // is the reason the check exists at all.
        if let Some(path) = entry.path.as_local() {
            sink += git.load().status_for(path, entry.is_dir_like()).is_some() as usize;
        }

        // Date and Size — the two that legitimately allocate.
        if let Some(modified) = entry.metadata.and_then(|m| m.modified) {
            sink += crate::format::format_date(modified).len();
        }
        if let Some(len) = entry.display_len() {
            sink += crate::format::format_size(len).len();
        }
    }
    sink
}

#[gpui::test]
async fn frame_allocation_budget(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_global(crate::state::load_state()));

    let mut folder = folder_of(DIRECTORY_ROWS);
    let icons = FileIcons::embedded();
    let git = git_of(DIRECTORY_ROWS);
    assert_eq!(
        folder.selection_count(),
        DIRECTORY_ROWS,
        "the budget is measured with everything selected, which is the worst case"
    );

    let profiler = dhat::Profiler::builder().testing().build();

    // Warm up: the first pass resolves lazily-initialised things that a real
    // app resolves before its first frame.
    cx.update(|cx| std::hint::black_box(draw_rows(&folder, icons, &git, cx, 0, VISIBLE_ROWS)));

    let before = dhat::HeapStats::get();
    for frame in 0..FRAMES {
        // Scroll, so no frame draws the same rows as the last.
        let first = (frame * VISIBLE_ROWS) % (DIRECTORY_ROWS - VISIBLE_ROWS);
        cx.update(|cx| {
            std::hint::black_box(draw_rows(&folder, icons, &git, cx, first, VISIBLE_ROWS))
        });

        // And drag a marquee band across those rows while doing it: a
        // selection change per frame is what a drag actually does, and it is
        // the path A-29 made O(the directory).
        folder.select_rows(first..first + VISIBLE_ROWS);
        std::hint::black_box((folder.selection_count(), folder.selection_size()));
    }
    let after = dhat::HeapStats::get();
    drop(profiler);

    let allocations = after.total_blocks - before.total_blocks;
    let per_row = allocations as f64 / (FRAMES * VISIBLE_ROWS) as f64;
    println!(
        "{allocations} allocations over {FRAMES} frames of {VISIBLE_ROWS} rows \
         ({per_row:.2} per row, budget {ALLOCATIONS_PER_ROW})"
    );

    let budget = ALLOCATIONS_PER_ROW * (FRAMES * VISIBLE_ROWS) as u64;
    assert!(
        allocations <= budget,
        "{per_row:.2} heap allocations per visible row per frame, budget is \
         {ALLOCATIONS_PER_ROW}. Something on the render path is allocating per row — \
         a clone that is not a shared handle, a scan of the listing, a `format!`, \
         or a collection built to answer one question."
    );
}
