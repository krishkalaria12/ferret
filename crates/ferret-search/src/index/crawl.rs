//! Filling the index, and keeping it honest.
//!
//! There is only one traversal here and it does both jobs. A first crawl and a
//! reconciliation sweep are the same walk over the same tree writing the same
//! rows; the only difference is that the first one finds nothing already
//! stored. Writing them as one function is what guarantees that a
//! reconciliation cannot converge on a *different* answer than a rebuild
//! would — which is the failure a separate "repair" path invites.
//!
//! Rows are stamped with the root's sweep generation as they are visited, and
//! anything still carrying an older stamp when the walk ends is exactly the
//! set of paths the filesystem no longer has. That is why a dropped watcher
//! event is a latency problem rather than a correctness one.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;

use super::content::{ContentWriter, indexable_text};
use super::db::{ContentWork, Db, FileRow, RootRow, content_state, kind};
use crate::config::{CRAWL_BATCH, MAX_INDEXED_CONTENT_BYTES};
use crate::{Result, SearchFilters};

/// What one sweep did.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SweepOutcome {
    pub visited: u64,
    /// Entries the index cannot represent — today, paths that are not valid
    /// UTF-8. They stay findable by the live scan, which never converts a path
    /// to text at all.
    pub skipped: u64,
    pub removed: u64,
    /// `true` when the sweep stopped early because the daemon is shutting
    /// down. The root must not be marked `Ready` in that case: its absences
    /// are not yet meaningful.
    pub interrupted: bool,
}

/// The filter policy the crawl walks with.
///
/// It is exactly [`SearchFilters::default`] — hidden entries excluded, ignore
/// files respected, symlinks not followed — and that is a contract, not a
/// coincidence. The index can only answer a query whose filters agree with the
/// policy it was built under; [`super::policy_matches`] is the check, and a
/// query that disagrees is routed to the live scan instead of being answered
/// from a corpus that was never allowed to contain the files it is asking for.
fn crawl_walker(root: &Path) -> WalkBuilder {
    let policy = SearchFilters::default();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!policy.include_hidden)
        .ignore(policy.respect_ignore)
        .git_ignore(policy.respect_ignore)
        .git_global(policy.respect_ignore)
        .git_exclude(policy.respect_ignore)
        .require_git(false)
        .parents(policy.respect_ignore)
        .follow_links(policy.follow_symlinks)
        // One thread: indexing is background work that must lose to a
        // foreground query for the disk, and `ignore`'s parallel walker would
        // take as many threads as the machine has.
        .threads(1)
        // The index must never index itself. Without this the daemon watches a
        // directory it writes to, and every commit schedules the work that
        // produces the next commit.
        .filter_entry(|entry| !super::is_excluded(entry.path()));
    builder
}

/// Crawls or reconciles one root in full.
pub(crate) fn sweep_root(
    db: &Db,
    content: &mut ContentWriter,
    root: &RootRow,
    stop: &AtomicBool,
) -> Result<SweepOutcome> {
    let generation = db.begin_sweep(root.id)?;
    let mut outcome = SweepOutcome::default();
    let mut batch = 0usize;

    db.begin()?;
    for entry in crawl_walker(Path::new(&root.path)).build() {
        if stop.load(Ordering::Acquire) {
            outcome.interrupted = true;
            break;
        }
        let Ok(entry) = entry else {
            outcome.skipped += 1;
            continue;
        };
        if entry.depth() == 0 {
            continue;
        }
        let Some(row) = row_for(entry.path(), entry.depth()) else {
            outcome.skipped += 1;
            continue;
        };
        if db.upsert_file(root.id, generation, &row)? == ContentWork::Replace {
            content.delete(&row.path);
        }
        outcome.visited += 1;
        batch += 1;
        if batch >= CRAWL_BATCH {
            db.commit()?;
            db.begin()?;
            batch = 0;
        }
    }
    db.commit()?;

    if outcome.interrupted {
        // An interrupted walk did not visit every path, so the unvisited rows
        // are not stale — deleting them would turn a shutdown into data loss.
        return Ok(outcome);
    }
    for stale in db.finish_sweep(root.id, generation)? {
        content.delete(&stale);
        outcome.removed += 1;
    }
    content.commit_if_due()?;
    Ok(outcome)
}

/// Re-walks one directory after the watcher reported activity in it.
///
/// Scoped to the subtree rather than the root: a `git checkout` in one
/// repository must not cost a walk of the whole home directory. Paths the DB
/// has under `dir` but the walk did not reach are deleted, which is what makes
/// this handle a directory rename correctly — the old name's subtree vanishes
/// in one pass.
pub(crate) fn reindex_subtree(
    db: &Db,
    content: &mut ContentWriter,
    root: &RootRow,
    dir: &Path,
    stop: &AtomicBool,
) -> Result<SweepOutcome> {
    let Some(dir_text) = dir.to_str() else {
        return Ok(SweepOutcome {
            skipped: 1,
            ..SweepOutcome::default()
        });
    };
    let mut outcome = SweepOutcome::default();
    let mut visited = std::collections::HashSet::new();
    let generation = db.current_generation(root.id)?;

    db.begin()?;
    for entry in crawl_walker(dir).build() {
        if stop.load(Ordering::Acquire) {
            outcome.interrupted = true;
            break;
        }
        let Ok(entry) = entry else {
            outcome.skipped += 1;
            continue;
        };
        let Some(row) = row_for(entry.path(), entry.depth()) else {
            outcome.skipped += 1;
            continue;
        };
        if db.upsert_file(root.id, generation, &row)? == ContentWork::Replace {
            content.delete(&row.path);
        }
        visited.insert(row.path.clone());
        outcome.visited += 1;
    }
    db.commit()?;
    if outcome.interrupted {
        return Ok(outcome);
    }

    for known in db.paths_under(dir_text)? {
        if visited.contains(&known) {
            continue;
        }
        db.delete_path(&known)?;
        content.delete(&known);
        outcome.removed += 1;
    }
    Ok(outcome)
}

/// Applies one path the watcher reported.
///
/// The event kind is deliberately ignored — by the time it arrives the truth
/// is on disk, not in the event, and a create followed by a delete inside one
/// debounce window would otherwise be applied in the wrong order.
pub(crate) fn apply_change(
    db: &Db,
    content: &mut ContentWriter,
    root: &RootRow,
    path: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let Some(text) = path.to_str() else {
        return Ok(());
    };
    let metadata = std::fs::symlink_metadata(path);
    match metadata {
        Err(_) => {
            // Gone, or unreadable — which for index purposes is the same
            // thing, because a path Ferret cannot stat is one it cannot open
            // for the user either.
            for removed in db.delete_subtree(text)? {
                content.delete(&removed);
            }
        }
        Ok(metadata) if metadata.is_dir() => {
            reindex_subtree(db, content, root, path, stop)?;
        }
        Ok(_) => {
            let depth = subtree_depth(&root.path, text);
            let Some(row) = row_for(path, depth) else {
                return Ok(());
            };
            let generation = db.current_generation(root.id)?;
            if db.upsert_file(root.id, generation, &row)? == ContentWork::Replace {
                content.delete(&row.path);
            }
        }
    }
    Ok(())
}

/// Reads bodies for rows whose contents are not in the index yet.
///
/// Separate from the path sweep because they have different costs and
/// different urgency: a path row is a few hundred bytes of SQLite, a body is a
/// file open and a read. Paths become searchable immediately and bodies
/// follow, which is why an in-progress index can already answer filename
/// queries.
pub(crate) fn backfill_content(
    db: &Db,
    content: &mut ContentWriter,
    root: &RootRow,
    stop: &AtomicBool,
) -> Result<u64> {
    let mut indexed = 0;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let pending = db.pending_content(root.id, CRAWL_BATCH)?;
        if pending.is_empty() {
            break;
        }
        for row in pending {
            if stop.load(Ordering::Acquire) {
                break;
            }
            // Every row is marked either way. Leaving one `PENDING` after
            // looking at it would make this loop reselect it forever.
            let state = match index_body(content, &row) {
                Ok(true) => {
                    indexed += 1;
                    content_state::INDEXED
                }
                Ok(false) | Err(_) => content_state::SKIPPED,
            };
            db.mark_content_state(&row.path, state)?;
            content.commit_if_due()?;
        }
    }
    content.commit()?;
    Ok(indexed)
}

fn index_body(content: &mut ContentWriter, row: &FileRow) -> Result<bool> {
    if row.kind != kind::FILE || row.size as u64 > MAX_INDEXED_CONTENT_BYTES {
        return Ok(false);
    }
    let Ok(bytes) = std::fs::read(&row.path) else {
        return Ok(false);
    };
    let Some(text) = indexable_text(&bytes) else {
        return Ok(false);
    };
    content.add(&row.path, &row.name, &text)?;
    Ok(true)
}

/// Builds the stored row for one filesystem entry.
///
/// `None` for a path that is not valid UTF-8. The index keys on text and
/// SQLite's `TEXT` is UTF-8, so those paths are the one thing it genuinely
/// cannot hold; they are counted and left to the live scan rather than stored
/// lossily under a name that would never be openable.
fn row_for(path: &Path, depth: usize) -> Option<FileRow> {
    let text = path.to_str()?;
    let name = path.file_name()?.to_str()?;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        kind::DIR
    } else if file_type.is_file() {
        kind::FILE
    } else if file_type.is_symlink() {
        match std::fs::metadata(path) {
            Ok(target) if target.is_dir() => kind::SYMLINK_DIR,
            Ok(_) => kind::SYMLINK_FILE,
            Err(_) => kind::SYMLINK_BROKEN,
        }
    } else {
        kind::OTHER
    };
    let modified = metadata.modified().ok();
    let nanos = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    Some(FileRow {
        path: text.to_owned(),
        name: name.to_owned(),
        ext: ferret_vfs::VfsPath::local(path).extension().map(Into::into),
        kind,
        size: metadata.len() as i64,
        mtime: modified
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64),
        hidden: name.starts_with('.'),
        depth: depth as i64,
        fingerprint: format!("{}:{nanos}", metadata.len()),
    })
}

fn subtree_depth(root: &str, path: &str) -> usize {
    path.strip_prefix(root)
        .map(|rest| {
            rest.split(std::path::MAIN_SEPARATOR)
                .filter(|part| !part.is_empty())
                .count()
        })
        .unwrap_or(1)
}
