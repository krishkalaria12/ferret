//! The reported variants: what a copy that is watched and stopped actually
//! does, as opposed to what a copy nobody is watching does.

use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// A reporter that stops the operation once it has been told about `limit`
/// bytes.
///
/// Cancelling *after* a threshold rather than before any work is deliberate:
/// the promise under test is that an interrupted copy removes what it wrote,
/// and a reporter that refused before the first chunk would prove nothing about
/// the destination.
struct StopAfter {
    limit: u64,
    bytes: AtomicU64,
    entries: AtomicU64,
    stopped: AtomicBool,
}

impl StopAfter {
    fn after(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        })
    }

    fn cancelled(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }
}

impl crate::ProgressReporter for StopAfter {
    fn bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    fn entries(&self, count: u64) {
        self.entries.fetch_add(count, Ordering::Relaxed);
    }

    fn checkpoint(&self) -> crate::Checkpoint {
        if self.bytes.load(Ordering::Relaxed) >= self.limit {
            self.stopped.store(true, Ordering::Relaxed);
            return crate::Checkpoint::Cancel;
        }
        crate::Checkpoint::Continue
    }
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferret-vfs-observed-{tag}-{}-{:?}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Bigger than one copy chunk, so there is a boundary *inside* the file for a
/// cancel to land on. `config::COPY_CHUNK_BYTES` is what a test has to stay
/// ahead of; three chunks leaves no room for the constant to grow quietly.
fn three_chunks() -> Vec<u8> {
    vec![7u8; crate::config::COPY_CHUNK_BYTES * 3]
}

#[test]
fn a_watched_copy_reports_every_byte_it_writes() {
    let root = tempdir("report-bytes");
    let bytes = three_chunks();
    fs::write(root.join("src.bin"), &bytes).unwrap();
    let dst = root.join("dst.bin");

    let meters = Arc::new(crate::Meters::new());
    let reporter: Arc<dyn crate::ProgressReporter> = meters.clone();
    smol::block_on(LocalFs::new().copy_observed(
        &VfsPath::local(root.join("src.bin")),
        &VfsPath::local(&dst),
        reporter,
    ))
    .unwrap();

    assert_eq!(meters.bytes(), bytes.len() as u64);
    assert_eq!(meters.entries(), 1);
    assert_eq!(fs::read(&dst).unwrap().len(), bytes.len());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancelled_copy_leaves_nothing_that_looks_like_a_copy() {
    let root = tempdir("cancel-file");
    let bytes = three_chunks();
    fs::write(root.join("big.bin"), &bytes).unwrap();
    let dst = root.join("big.bin.copy");

    // One chunk in, so the file is genuinely half-written at the cancel.
    let reporter = StopAfter::after(crate::config::COPY_CHUNK_BYTES as u64);
    let error = smol::block_on(LocalFs::new().copy_observed(
        &VfsPath::local(root.join("big.bin")),
        &VfsPath::local(&dst),
        reporter.clone(),
    ))
    .unwrap_err();

    assert!(reporter.cancelled(), "the reporter asked to stop");
    assert!(
        matches!(error, VfsError::Cancelled { .. }),
        "a cancel is not a failure: {error}"
    );
    assert!(
        !dst.exists(),
        "a half-written {} must not be left behind",
        dst.display()
    );
    assert_eq!(
        fs::read(root.join("big.bin")).unwrap(),
        bytes,
        "source intact"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancelled_tree_copy_removes_the_directories_it_created() {
    let root = tempdir("cancel-tree");
    let src = root.join("Photos");
    for day in 0..4 {
        let dir = src.join(format!("2026-01-0{day}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("frame.bin"), three_chunks()).unwrap();
    }
    let dst = root.join("Photos copy");

    let reporter = StopAfter::after(crate::config::COPY_CHUNK_BYTES as u64);
    let error = smol::block_on(LocalFs::new().copy_observed(
        &VfsPath::local(&src),
        &VfsPath::local(&dst),
        reporter,
    ))
    .unwrap_err();

    assert!(matches!(error, VfsError::Cancelled { .. }), "{error}");
    // The whole point: an abandoned copy of a folder must not leave a folder
    // that reads as a complete one.
    assert!(
        !dst.exists(),
        "the half-created tree {} survived",
        dst.display()
    );
    assert!(src.join("2026-01-00/frame.bin").exists(), "source intact");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_copy_keeps_the_source_modification_time() {
    let root = tempdir("timestamps");
    let src = root.join("report.txt");
    fs::write(&src, b"quarterly").unwrap();

    // A date with no claim to be now, so a copy that stamped the destination
    // with the time of the copy cannot pass by coincidence.
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let file = fs::OpenOptions::new().write(true).open(&src).unwrap();
    file.set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
    drop(file);

    let dst = root.join("report-copy.txt");
    smol::block_on(LocalFs::new().copy(&VfsPath::local(&src), &VfsPath::local(&dst))).unwrap();

    let copied = fs::metadata(&dst).unwrap().modified().unwrap();
    assert_eq!(
        copied, when,
        "the copy must not claim to be newer than the source"
    );

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn a_copy_keeps_the_source_permissions() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir("permissions");
    let src = root.join("private.txt");
    fs::write(&src, b"secret").unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();

    let dst = root.join("private-copy.txt");
    smol::block_on(LocalFs::new().copy(&VfsPath::local(&src), &VfsPath::local(&dst))).unwrap();

    let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a private file must not arrive world-readable");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancelled_delete_stops_and_reports_what_landed() {
    let root = tempdir("cancel-delete");
    let victim = root.join("trash-me");
    fs::create_dir_all(&victim).unwrap();
    // Top-level children, because that is the level the parallel delete
    // checkpoints at.
    for i in 0..64 {
        fs::write(victim.join(format!("f{i}.txt")), b"x").unwrap();
    }

    // A limit of zero cancels at the first checkpoint, which is before the
    // first unlink — so this asserts the walk stops rather than that it stops
    // *quickly*.
    let reporter = StopAfter::after(0);
    let error = smol::block_on(LocalFs::new().remove_observed(
        &VfsPath::local(&victim),
        RemoveMode::Permanent,
        reporter.clone(),
    ))
    .unwrap_err();

    assert!(matches!(error, VfsError::Cancelled { .. }), "{error}");
    assert!(
        victim.exists(),
        "nothing was removed, so the tree is still there"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_watched_delete_reports_the_entries_it_removed() {
    let root = tempdir("delete-count");
    let victim = root.join("doomed");
    fs::create_dir_all(&victim).unwrap();
    for i in 0..8 {
        fs::write(victim.join(format!("f{i}.txt")), b"x").unwrap();
    }

    let meters = Arc::new(crate::Meters::new());
    let reporter: Arc<dyn crate::ProgressReporter> = meters.clone();
    smol::block_on(LocalFs::new().remove_observed(
        &VfsPath::local(&victim),
        RemoveMode::Permanent,
        reporter,
    ))
    .unwrap();

    assert!(!victim.exists());
    // Eight children plus the directory itself.
    assert_eq!(
        meters.entries(),
        9,
        "one count per removal, including the root"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn the_trash_is_not_a_thing_the_local_backend_can_do() {
    // Gate 5. The reported remove must not have quietly become a second
    // permanent-delete path: a backend that cannot reach the trash has to say
    // so, and the queue sends trashing through the platform instead.
    let error = smol::block_on(LocalFs::new().remove_observed(
        &VfsPath::local("/tmp/whatever"),
        RemoveMode::Trash,
        Arc::new(crate::Meters::new()),
    ))
    .unwrap_err();
    assert!(matches!(error, VfsError::Unsupported { .. }), "{error}");
}
