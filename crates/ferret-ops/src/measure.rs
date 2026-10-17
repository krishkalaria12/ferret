//! Working out how much work a job is before doing any of it.
//!
//! This is the pass that makes a progress bar possible, and it is also the
//! reason [`crate::JobState::Measuring`] exists: on a 40,000-file selection the
//! count takes long enough to see, and a bar sitting at zero with nothing
//! happening is indistinguishable from a hang.
//!
//! ## One walker, one job
//!
//! `jwalk`, across Ferret's own pool, because this is bulk enumeration with no
//! ignore semantics — exactly the job D-34 gives it, and the same reason the
//! tree copy uses it. It is *not* the single-directory listing walk (that one is
//! a serial `read_dir` stream) and it is *not* search, which `ignore` owns
//! because honouring `.gitignore` is that feature. Three jobs, three walkers,
//! none of them doing another's work.
//!
//! ## What a total is worth
//!
//! A total is only worth measuring if it will be roughly right by the time the
//! last byte lands. Four things would break that, and each is refused rather
//! than papered over:
//!
//! * **A remote source.** Counting a server's tree is a round trip per
//!   directory and the answer goes stale while it is being counted. Sources
//!   that are not local get an inexact [`Measurement`], and the job shows a
//!   live byte count with no denominator rather than a percentage of a guess.
//! * **A directory too large to be worth counting.** [`config::MEASURE_ENTRY_CAP`]
//!   stops the walk and reports what it saw, so measuring is never the most
//!   expensive part of a copy.
//! * **A source that cannot be read.** Measured as unknown rather than as an
//!   error, and this is the one that matters: a selection of 1,200 files where
//!   four have been deleted since it was made is an ordinary thing to hand a
//!   file manager. Failing the measurement would fail the job before the copy
//!   started, throwing away 1,196 files that could have been copied — and the
//!   executor would have reported those four on its own, per source, and got
//!   on with it.
//! * **Anything that changed while we counted.** A total is a statement about
//!   the past. The executor re-reads each file as it copies it, so a total that
//!   is slightly wrong makes the bar finish at 97% or run to 103% — clamped —
//!   and never makes the copy wrong.

use std::sync::Arc;

use ferret_vfs::VfsPath;

use crate::config::MEASURE_ENTRY_CAP;
use crate::execute::Handle;

/// How much work a set of sources turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub bytes: u64,
    pub entries: u64,
    /// `false` when the count is incomplete: a cap, a remote source, or a
    /// source that could not be read. The bar then reports a count without a
    /// percentage, which is the honest shape.
    pub exact: bool,
}

impl Default for Measurement {
    /// The empty count of nothing, and it *is* exact.
    ///
    /// Derived `Default` would say `false`, and `merge` intersects `exact` —
    /// so an accumulator built from `Default` would make every total inexact
    /// and every job would lose its percentage. The empty sum is the one
    /// measurement that is certainly complete.
    fn default() -> Self {
        Self {
            bytes: 0,
            entries: 0,
            exact: true,
        }
    }
}

impl Measurement {
    /// Nothing worth counting, and no promise that counting would help.
    fn unknown() -> Self {
        Self {
            bytes: 0,
            entries: 0,
            exact: false,
        }
    }

    fn merge(&mut self, other: Measurement) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.entries = self.entries.saturating_add(other.entries);
        self.exact &= other.exact;
    }
}

/// Count the bytes and entries a job will move.
///
/// Polls `handle` per entry rather than only between sources, because the shape
/// that makes this slow is one enormous source: a million-file tree is a
/// million stats, and a Cancel pressed during them has to land.
///
/// No `Vfs`, and that is the finding rather than an oversight: a remote source is
/// refused below without consulting a backend, so the only thing that needs
/// counting is local disk, and `std::fs` plus `jwalk` is the whole of it.
pub async fn measure(sources: &[VfsPath], handle: &Arc<Handle>) -> Measurement {
    let mut total = Measurement::default();
    for source in sources {
        if handle.is_cancelled() {
            // Whatever was counted is not used: the job is cancelled, so there
            // is no bar left to draw it on.
            return Measurement::unknown();
        }
        total.merge(measure_one(source, Arc::clone(handle)).await);
    }
    total
}

async fn measure_one(source: &VfsPath, handle: Arc<Handle>) -> Measurement {
    // The honest answer for a server: it would cost a round trip per directory
    // to count, and it would be stale before it was printed.
    let Some(local) = source.as_local().map(std::path::Path::to_path_buf) else {
        return Measurement::unknown();
    };

    // A source that cannot be counted is a source the executor will report on
    // by name, when it fails. Measuring is a courtesy to the progress bar, and
    // it must never be the reason a job does not start.
    smol::unblock(move || measure_local(&local, &handle))
        .await
        .unwrap_or_else(|_| Measurement::unknown())
}

/// `jwalk` over one local path.
///
/// A plain `stat` for a file, which is the common case and must not pay for a
/// walker at all.
fn measure_local(
    path: &std::path::Path,
    handle: &Arc<Handle>,
) -> std::result::Result<Measurement, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;

    if metadata.is_symlink() || !metadata.is_dir() {
        return Ok(Measurement {
            bytes: if metadata.is_symlink() {
                0
            } else {
                metadata.len()
            },
            entries: 1,
            exact: true,
        });
    }

    // `follow_links(false)` for the reason the tree copy has: a link back to
    // `/` inside the tree must not turn a count into a walk of the disk.
    let mut measured = Measurement {
        bytes: 0,
        entries: 0,
        exact: true,
    };
    for entry in jwalk::WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false)
        .parallelism(ferret_vfs::pool::parallelism())
    {
        if measured.entries >= MEASURE_ENTRY_CAP as u64 || handle.is_cancelled() {
            measured.exact = false;
            break;
        }
        let entry = entry.map_err(|error| format!("{}: {error}", path.display()))?;
        measured.entries += 1;
        if entry.file_type().is_file() {
            // A file that vanished between the walk and the stat is counted as
            // zero rather than failing the measurement: the copy will find out
            // on its own, and a total is not worth an error over.
            if let Ok(metadata) = entry.metadata() {
                measured.bytes = measured.bytes.saturating_add(metadata.len());
            }
        }
    }
    Ok(measured)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ferret-ops-measure-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_measures_as_itself() {
        let root = tempdir("file");
        let file = root.join("a.bin");
        std::fs::write(&file, vec![0u8; 4_096]).unwrap();

        let measured = measure_local(&file, &Handle::new()).unwrap();
        assert_eq!(
            measured,
            Measurement {
                bytes: 4_096,
                entries: 1,
                exact: true
            }
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_tree_measures_every_file_beneath_it() {
        let root = tempdir("tree");
        let src = root.join("tree");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("top.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(src.join("a/mid.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(src.join("a/b/deep.bin"), vec![0u8; 300]).unwrap();

        let measured = measure_local(&src, &Handle::new()).unwrap();
        assert_eq!(measured.bytes, 600);
        // The root plus `a`, `a/b` and the three files.
        assert_eq!(measured.entries, 6);
        assert!(measured.exact);

        std::fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_counts_as_one_entry_and_no_bytes() {
        // Following it would measure whatever it points at — the whole disk, if
        // it points at `/` — and the copy does not follow it either, so the two
        // have to agree or the bar would never finish.
        let root = tempdir("link");
        let file = root.join("real.bin");
        std::fs::write(&file, vec![0u8; 500]).unwrap();
        let link = root.join("link.bin");
        std::os::unix::fs::symlink(&file, &link).unwrap();

        let measured = measure_local(&link, &Handle::new()).unwrap();
        assert_eq!(
            measured,
            Measurement {
                bytes: 0,
                entries: 1,
                exact: true
            }
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_cancelled_measurement_reports_nothing_rather_than_a_partial_count() {
        // A count of half a tree is not a smaller truth, it is a wrong one, and
        // the bar would fill against it.
        let root = tempdir("cancelled");
        let src = root.join("tree");
        for i in 0..16 {
            std::fs::create_dir_all(src.join(format!("d{i}"))).unwrap();
            std::fs::write(src.join(format!("d{i}/f.bin")), vec![0u8; 10]).unwrap();
        }
        let handle = Handle::new();
        handle.cancel();

        let measured = measure_local(&src, &handle).unwrap();
        assert!(!measured.exact);
        assert_eq!(measured, Measurement::unknown());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_source_is_an_error_rather_than_a_zero() {
        // A zero total would be read as "there is nothing to copy", and the job
        // would report success having copied nothing.
        let error = measure_local(
            std::path::Path::new("/nonexistent/ferret-measure"),
            &Handle::new(),
        )
        .unwrap_err();
        assert!(error.contains("ferret-measure"), "{error}");
    }

    #[test]
    fn several_sources_add_up() {
        let root = tempdir("many");
        let mut sources = Vec::new();
        for i in 0..3 {
            let file = root.join(format!("f{i}.bin"));
            std::fs::write(&file, vec![0u8; 1_000 * (i + 1)]).unwrap();
            sources.push(VfsPath::local(&file));
        }

        let total = smol::block_on(measure(&sources, &Handle::new()));
        assert_eq!(total.bytes, 1_000 + 2_000 + 3_000);
        assert_eq!(total.entries, 3);
        assert!(total.exact);
        std::fs::remove_dir_all(&root).ok();
    }
}
