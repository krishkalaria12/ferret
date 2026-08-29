//! The watcher budget from `docs/audit.html` §11: after a burst of filesystem
//! activity, does the model end up matching the disk, and how long does it
//! take to get there?
//!
//! This is one test covering three findings at once, which is why it earns
//! being slow and touching a real filesystem:
//!
//! * **A-04** — a burst of deletes must remove the rows that were deleted,
//!   not whichever rows their stale indices happened to land on.
//! * **A-05** — the reindex runs on every burst, so a burst that used to cost
//!   several frames shows up here as a settle time.
//! * **A-06** — a burst larger than the watcher's channel overflows it, and a
//!   consumer that did not hear about the overflow would sit here stale
//!   forever. The `Desynced` contract is what makes this converge.
//!
//! The consistency assertion matters more than the timing, so the deadline is
//! generous: this runs against real FSEvents on a machine that may be busy,
//! and a flaky budget gets deleted rather than fixed.

use super::*;
use gpui::AppContext as _;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Files written in one go. Above `WATCH_RELOAD_THRESHOLD` (512) so the
/// consumer takes the re-list path, and far above the watcher's 64-event
/// channel so the overflow-and-resync contract is exercised rather than
/// assumed.
const BURST: usize = 5_000;

/// How long the model may take to agree with the disk again.
///
/// Loose on purpose. The number this guards is "does it converge at all" —
/// before A-06 it did not, and no timing would have caught that. A real
/// regression in settle time is a stall of seconds, not milliseconds.
const SETTLE_BUDGET: Duration = Duration::from_secs(20);

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferret-ui-burst-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every name the folder currently shows.
fn model_names(folder: &gpui::Entity<Folder>, cx: &mut gpui::TestAppContext) -> BTreeSet<String> {
    folder.read_with(cx, |f, _| {
        (0..f.row_count())
            .filter_map(|r| Some(f.entry_at(r)?.name().to_string()))
            .collect()
    })
}

fn disk_names(dir: &std::path::Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .collect()
}

/// Drive the executor until the model matches the disk, or give up.
///
/// Three things have to be pushed along together, which is why this is a loop
/// and not an await:
///
/// * `run_until_parked` runs the model's own tasks.
/// * `advance_clock` makes timers ready. The test executor's clock is
///   simulated — "this does not run any tasks, but does make `timer`s ready" —
///   so the consumer's `DEBOUNCE` would otherwise never fire and the watcher
///   task would sit parked forever.
/// * the real sleep gives the OS watcher thread, which is not on this
///   executor, time to actually deliver something.
///
/// The elapsed time is therefore wall clock including our own polling
/// interval, which is fine for a budget measured in seconds.
fn settle(
    folder: &gpui::Entity<Folder>,
    dir: &std::path::Path,
    cx: &mut gpui::TestAppContext,
) -> Option<Duration> {
    let started = std::time::Instant::now();
    let wanted = disk_names(dir);
    while started.elapsed() < SETTLE_BUDGET {
        cx.run_until_parked();
        if model_names(folder, cx) == wanted {
            return Some(started.elapsed());
        }
        cx.executor().advance_clock(ferret_vfs::watch::DEBOUNCE * 2);
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[gpui::test]
async fn a_burst_of_creates_and_deletes_converges_on_what_is_on_disk(
    cx: &mut gpui::TestAppContext,
) {
    let root = tempdir("mixed");
    for i in 0..50 {
        fs::write(root.join(format!("original-{i:04}.txt")), b"x").unwrap();
    }

    cx.update(|cx| cx.set_global(crate::state::load_state()));
    let folder = cx.new(|_| {
        Folder::new(
            VfsPath::local(root.clone()),
            SortRules::by_name(),
            true,
            ListingProfile::BROWSING,
        )
    });
    folder.update(cx, |f, cx| f.start_loading(cx));

    // The listing producer runs on its own OS thread, and so does the watcher.
    cx.executor().allow_parking();
    assert!(
        settle(&folder, &root, cx).is_some(),
        "the initial listing must match the directory it just read"
    );

    // A burst far past both the re-list threshold and the event channel.
    for i in 0..BURST {
        fs::write(root.join(format!("burst-{i:05}.txt")), b"x").unwrap();
    }
    let created = settle(&folder, &root, cx)
        .expect("the model must converge on the disk after a burst of creates");
    assert_eq!(
        folder.read_with(cx, |f, _| f.row_count()),
        50 + BURST,
        "every created file must have a row"
    );

    // And the other direction — the case A-04 got wrong, where removing rows
    // shifted the indices of the rows still to be removed.
    for i in 0..BURST {
        fs::remove_file(root.join(format!("burst-{i:05}.txt"))).unwrap();
    }
    let deleted = settle(&folder, &root, cx)
        .expect("the model must converge on the disk after a burst of deletes");

    let survivors = model_names(&folder, cx);
    assert_eq!(survivors.len(), 50, "exactly the originals survive");
    assert!(
        survivors.iter().all(|n| n.starts_with("original-")),
        "a delete burst removed the wrong rows: {survivors:?}"
    );

    println!("burst settle: {created:.1?} to create {BURST}, {deleted:.1?} to delete them");
    fs::remove_dir_all(&root).ok();
}
