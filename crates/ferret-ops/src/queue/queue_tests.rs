//! The queue's promises, asserted against a real filesystem.
//!
//! Every test here writes actual files through a real [`Queue`], because the
//! things worth checking — that a cancel leaves nothing behind, that a pause
//! holds, that a second job does not start until the first is finished — are
//! statements about the interaction between a queue and a disk. A mock backend
//! would test the mock.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

/// A platform that is not the platform: the queue needs one for trashing, and
/// the tests that use it are about the filesystem, not about the trash.
struct NoTrash;

#[async_trait::async_trait]
impl Platform for NoTrash {
    async fn open_with_default(&self, _path: &std::path::Path) -> ferret_platform::Result<()> {
        Ok(())
    }
    async fn reveal_in_file_manager(&self, _path: &std::path::Path) -> ferret_platform::Result<()> {
        Ok(())
    }
    async fn open_in_terminal(&self, _path: &std::path::Path) -> ferret_platform::Result<()> {
        Ok(())
    }
    async fn move_to_trash(&self, _path: &std::path::Path) -> ferret_platform::Result<()> {
        Err(ferret_platform::PlatformError::Unsupported {
            capability: "the trash",
        })
    }
    fn places(&self) -> Vec<ferret_platform::Place> {
        Vec::new()
    }
    fn volumes(&self) -> Vec<ferret_platform::Place> {
        Vec::new()
    }
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ferret-ops-queue-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_queue() -> Queue {
    Queue::new(Arc::new(ferret_vfs::LocalFs::new()), Arc::new(NoTrash))
}

/// A disk that stops on demand, so a test can catch a job mid-flight.
///
/// The alternative is a big file and a hopeful sleep, and that is a test which
/// passes on a slow disk and fails on a fast one — the worst kind, because it
/// stops testing anything the moment the machine gets quicker. This holds a
/// copy at the door until the test opens it, so "the job is running" is
/// something the test decided rather than something it caught.
struct Gate {
    entered: AtomicUsize,
    released: std::sync::atomic::AtomicBool,
}

impl Gate {
    fn open(&self) {
        self.released.store(true, Ordering::SeqCst);
    }

    /// Wait for the job to be inside the backend, so a cancel arrives during
    /// the operation rather than before it.
    fn await_entry(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.entered.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the job never reached the disk");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

struct GatedFs {
    inner: ferret_vfs::LocalFs,
    gate: Arc<Gate>,
}

#[async_trait::async_trait]
impl Vfs for GatedFs {
    fn capabilities(&self) -> ferret_vfs::Capabilities {
        self.inner.capabilities()
    }

    fn read_dir(
        &self,
        dir: &VfsPath,
        detail: ferret_vfs::ListingDetail,
    ) -> ferret_vfs::ReadDirStream {
        self.inner.read_dir(dir, detail)
    }

    async fn metadata(&self, path: &VfsPath) -> ferret_vfs::Result<ferret_vfs::Metadata> {
        self.inner.metadata(path).await
    }

    async fn canonicalize(&self, path: &VfsPath) -> ferret_vfs::Result<VfsPath> {
        self.inner.canonicalize(path).await
    }

    async fn read(&self, path: &VfsPath, budget: usize) -> ferret_vfs::Result<Vec<u8>> {
        self.inner.read(path, budget).await
    }

    async fn create_dir(&self, path: &VfsPath) -> ferret_vfs::Result<()> {
        self.inner.create_dir(path).await
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
        self.inner.rename(from, to).await
    }

    async fn rename_noreplace(&self, from: &VfsPath, to: &VfsPath) -> ferret_vfs::Result<()> {
        self.inner.rename_noreplace(from, to).await
    }

    async fn copy_observed(
        &self,
        from: &VfsPath,
        to: &VfsPath,
        reporter: Arc<dyn ferret_vfs::ProgressReporter>,
    ) -> ferret_vfs::Result<()> {
        self.gate.entered.fetch_add(1, Ordering::SeqCst);
        while !self.gate.released.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        // The real backend from here, so the cancel that arrived while the gate
        // was shut is answered by real chunk-boundary checking against real
        // bytes — not by a mock agreeing to be cancelled.
        self.inner.copy_observed(from, to, reporter).await
    }

    async fn remove(&self, path: &VfsPath, mode: ferret_vfs::RemoveMode) -> ferret_vfs::Result<()> {
        self.inner.remove(path, mode).await
    }
}

fn gated_queue() -> (Queue, Arc<Gate>) {
    let gate = Arc::new(Gate {
        entered: AtomicUsize::new(0),
        released: std::sync::atomic::AtomicBool::new(false),
    });
    let fs = Arc::new(GatedFs {
        inner: ferret_vfs::LocalFs::new(),
        gate: Arc::clone(&gate),
    });
    (Queue::new(fs, Arc::new(NoTrash)), gate)
}

/// Spin until `done` or the deadline. A test that sleeps a fixed time is either
/// slow or flaky; this is fast when it can be and only slow when it must be.
fn settle(queue: &Queue, id: JobId) -> Job {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job = queue.job(id).expect("the job is in the queue");
        if job.state.is_finished() {
            return job;
        }
        assert!(Instant::now() < deadline, "job {id:?} never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn write_file(path: &std::path::Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, vec![9u8; bytes]).unwrap();
}

/// A destination directory, already existing.
///
/// [`Request::copy`] takes a directory the user pointed at, so it is always
/// there already, and a test that forgets this is testing a case the UI cannot
/// produce. Ferret uses `create_dir` rather than `create_dir_all` precisely so
/// it does not invent a path nobody named.
fn destination(root: &std::path::Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_copy_job_lands_the_bytes_and_reports_a_full_bar() {
    let root = tempdir("copy");
    let src = root.join("source");
    let dst = destination(&root, "dest");
    write_file(&src.join("a.bin"), 4_096);
    write_file(&src.join("nested/b.bin"), 8_192);

    let queue = test_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&src)],
            VfsPath::local(&dst),
        ))
        .unwrap();
    let job = settle(&queue, id);

    assert_eq!(job.state, JobState::Succeeded, "{:?}", job.error);
    assert_eq!(job.progress.bytes_done, 12_288);
    assert_eq!(job.progress.bytes_total, Some(12_288));
    assert_eq!(job.progress.fraction(), Some(1.0));
    // Into a *directory*, so each source keeps its own name.
    assert!(dst.join("source/a.bin").exists());
    assert!(dst.join("source/nested/b.bin").exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_move_within_a_volume_leaves_nothing_to_copy() {
    let root = tempdir("move");
    let src = root.join("a.bin");
    let dest = destination(&root, "dest");
    write_file(&src, 1_024);

    let queue = test_queue();
    let id = queue
        .enqueue(Request::move_to(
            vec![VfsPath::local(&src)],
            VfsPath::local(&dest),
        ))
        .unwrap();
    let job = settle(&queue, id);

    assert_eq!(job.state, JobState::Succeeded, "{:?}", job.error);
    assert!(dest.join("a.bin").exists(), "it arrived");
    assert!(!src.exists(), "and it left");
    // A rename moves no bytes, so a bar that counted them would show a transfer
    // that never happened and an ETA for work the disk never did.
    assert_eq!(job.progress.bytes_done, 0);
    assert_eq!(job.entries_completed, 1);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_permanent_delete_removes_the_tree() {
    let root = tempdir("delete");
    let victim = root.join("doomed");
    for i in 0..12 {
        write_file(&victim.join(format!("d{i}/f.bin")), 64);
    }

    let queue = test_queue();
    let id = queue
        .enqueue(Request::delete(vec![VfsPath::local(&victim)]))
        .unwrap();
    let job = settle(&queue, id);

    assert_eq!(job.state, JobState::Succeeded, "{:?}", job.error);
    assert!(!victim.exists());
    assert!(job.progress.entries_done > 0, "a delete reports entries");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_pause_stops_the_copy_and_a_resume_finishes_it() {
    let root = tempdir("pause");
    let src = root.join("big.bin");
    // Large enough that the copy cannot finish before the pause is observed.
    write_file(&src, 96 * 1024 * 1024);
    let dst = destination(&root, "copy");

    let queue = test_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&src)],
            VfsPath::local(&dst),
        ))
        .unwrap();

    // Wait until it is genuinely running, then hold it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while queue.job(id).unwrap().state != JobState::Running {
        assert!(Instant::now() < deadline, "never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(queue.set_paused(id, true).unwrap(), "the pause took");
    assert_eq!(queue.job(id).unwrap().state, JobState::Paused);

    // A paused job stays paused: this is the assertion that matters, because a
    // pause that only sets a flag the copy happens to ignore looks identical to
    // a working one right up until someone waits for it.
    std::thread::sleep(Duration::from_millis(200));
    let held = queue.job(id).unwrap();
    assert_eq!(held.state, JobState::Paused, "the job did not stay paused");
    let held_bytes = held.progress.bytes_done;
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        queue.job(id).unwrap().progress.bytes_done,
        held_bytes,
        "a paused copy is still copying"
    );

    assert!(!queue.set_paused(id, false).unwrap());
    let job = settle(&queue, id);
    assert_eq!(job.state, JobState::Succeeded, "{:?}", job.error);
    assert_eq!(job.progress.bytes_done, 96 * 1024 * 1024);
    assert!(dst.exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancel_stops_the_job_and_leaves_no_half_written_destination() {
    let root = tempdir("cancel");
    let src = root.join("big.bin");
    write_file(&src, 24 * 1024 * 1024);
    let dst = destination(&root, "copy");

    let (queue, gate) = gated_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&src)],
            VfsPath::local(&dst),
        ))
        .unwrap();

    // The job is inside the copy, held at the door, having written nothing.
    gate.await_entry();
    queue.cancel(id).unwrap();
    gate.open();
    let job = settle(&queue, id);

    assert_eq!(job.state, JobState::Cancelled);
    assert!(
        !dst.join("big.bin").exists(),
        "a cancelled copy must not leave {} behind",
        dst.display()
    );
    assert!(src.exists(), "the source is untouched");

    // A second Cancel is accepted and changes nothing, because the button is
    // still on screen and a person who clicks it twice should not be told they
    // did something wrong.
    assert!(queue.cancel(id).is_ok());
    assert_eq!(queue.job(id).unwrap().state, JobState::Cancelled);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn cancelling_a_finished_job_says_it_is_already_over() {
    let root = tempdir("cancel-finished");
    let file = root.join("a.bin");
    write_file(&file, 1_024);
    let dest = destination(&root, "dest");

    let queue = test_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&file)],
            VfsPath::local(&dest),
        ))
        .unwrap();
    assert_eq!(settle(&queue, id).state, JobState::Succeeded);

    let error = queue.cancel(id).unwrap_err();
    assert!(matches!(error, OpsError::AlreadySettled { .. }), "{error}");
    assert_eq!(queue.job(id).unwrap().state, JobState::Succeeded);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancel_asked_for_before_the_copy_starts_still_stops_it() {
    let root = tempdir("cancel-early");
    let src = root.join("big.bin");
    write_file(&src, 24 * 1024 * 1024);
    let dst = destination(&root, "copy");

    let (queue, gate) = gated_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&src)],
            VfsPath::local(&dst),
        ))
        .unwrap();

    gate.await_entry();
    // Cancelled while the gate is shut, so the copy has not opened its
    // destination yet. The ask has to survive that wait rather than being lost
    // until the next thing happens to look.
    queue.cancel(id).unwrap();
    gate.open();
    let job = settle(&queue, id);

    assert_eq!(job.state, JobState::Cancelled);
    assert!(!dst.join("big.bin").exists());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_job_queued_behind_another_waits_for_it() {
    let root = tempdir("order");
    let first = root.join("first.bin");
    let second = root.join("second.bin");
    write_file(&first, 32 * 1024 * 1024);
    write_file(&second, 32 * 1024 * 1024);
    let dest = destination(&root, "dest");

    let queue = test_queue();
    let one = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&first)],
            VfsPath::local(&dest),
        ))
        .unwrap();
    let two = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&second)],
            VfsPath::local(&dest),
        ))
        .unwrap();

    // Both names are free, so the second does not collide with the first.
    assert_eq!(queue.pending(), 1, "the second job is waiting");
    assert_eq!(settle(&queue, one).state, JobState::Succeeded);
    assert_eq!(settle(&queue, two).state, JobState::Succeeded);
    assert!(dest.join("first.bin").exists());
    assert!(dest.join("second.bin").exists());
    assert_eq!(queue.pending(), 0);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cancelled_waiting_job_never_runs_at_all() {
    let root = tempdir("cancel-queued");
    let blocker = root.join("blocker.bin");
    let never = root.join("never.bin");
    write_file(&blocker, 48 * 1024 * 1024);
    write_file(&never, 64);
    let dest = destination(&root, "dest");

    let queue = test_queue();
    let running = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&blocker)],
            VfsPath::local(&dest),
        ))
        .unwrap();
    let waiting = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&never)],
            VfsPath::local(&dest),
        ))
        .unwrap();

    // Settled outright, not left for a worker to pick up and decline.
    queue.cancel(waiting).unwrap();
    assert_eq!(queue.job(waiting).unwrap().state, JobState::Cancelled);

    assert_eq!(settle(&queue, running).state, JobState::Succeeded);
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !dest.join("never.bin").exists(),
        "the cancelled job ran anyway"
    );
    assert_eq!(queue.pending(), 0);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn one_unreadable_source_does_not_sink_the_rest_of_the_job() {
    let root = tempdir("partial");
    let dest = destination(&root, "dest");
    write_file(&root.join("a.bin"), 128);
    write_file(&root.join("b.bin"), 128);

    let queue = test_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![
                VfsPath::local(root.join("a.bin")),
                // Never created, so this one cannot be read.
                VfsPath::local(root.join("missing.bin")),
                VfsPath::local(root.join("b.bin")),
            ],
            VfsPath::local(&dest),
        ))
        .unwrap();
    let job = settle(&queue, id);

    // Two of three landed. Reporting this as a failure would throw away two
    // good copies; reporting it as a success would hide a file that is not there.
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.entries_completed, 2);
    assert_eq!(job.failures.len(), 1);
    assert!(
        job.failures[0].contains("missing.bin"),
        "the failure names its file: {}",
        job.failures[0]
    );
    assert!(dest.join("a.bin").exists());
    assert!(dest.join("b.bin").exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_copy_that_cannot_reach_its_destination_names_the_destination() {
    let root = tempdir("fail");
    let queue = test_queue();
    // A file where a folder was expected, so nothing can be created inside it.
    let blocker = root.join("blocker");
    write_file(&blocker, 8);
    let src = root.join("a.bin");
    write_file(&src, 32);

    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&src)],
            VfsPath::local(&blocker),
        ))
        .unwrap();
    let job = settle(&queue, id);

    // The one source failed, so the job failed rather than reporting a copy
    // that copied nothing.
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.entries_completed, 0);
    // The destination, not the source. A tree operation has hundreds of paths
    // and the root is almost never the one that broke, so blaming the folder
    // somebody dragged is how a copy failure becomes untraceable.
    assert!(
        job.failures
            .iter()
            .any(|failure| failure.contains("blocker")),
        "the failure names the destination: {:?}",
        job.failures
    );

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_cross_backend_job_is_refused_before_it_is_accepted() {
    let queue = test_queue();
    let error = queue
        .enqueue(Request::copy(
            vec![VfsPath::local("/tmp/a")],
            VfsPath::remote(ferret_vfs::MountId(1), "/srv"),
        ))
        .unwrap_err();
    // A refusal that arrived as a job row three seconds later would be a
    // refusal nobody reads.
    assert!(matches!(error, OpsError::CrossBackend { .. }), "{error}");
    assert!(!queue.is_active());
}

#[test]
fn an_empty_request_is_refused() {
    assert!(matches!(
        test_queue().enqueue(Request::trash(Vec::new())),
        Err(OpsError::NothingToDo)
    ));
}

#[test]
fn trashing_a_remote_path_is_refused_because_there_is_no_trash_there() {
    let queue = test_queue();
    let error = queue
        .enqueue(Request::trash(vec![VfsPath::remote(
            ferret_vfs::MountId(1),
            "/srv/file",
        )]))
        .unwrap_err();
    assert!(matches!(error, OpsError::Unsupported { .. }), "{error}");
}

#[test]
fn a_trash_job_reports_that_the_platform_refused_rather_than_deleting() {
    // The `NoTrash` platform fails every trash. A job that fell back to a
    // permanent delete here would be the exact data-loss bug gate 5 exists to
    // prevent, and it would look like a success.
    let root = tempdir("trash-fails");
    let file = root.join("keep-me.txt");
    write_file(&file, 16);

    let queue = test_queue();
    let id = queue
        .enqueue(Request::trash(vec![VfsPath::local(&file)]))
        .unwrap();
    let job = settle(&queue, id);

    assert_ne!(job.state, JobState::Succeeded);
    assert!(file.exists(), "the file is still there");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn clearing_finished_jobs_empties_the_list() {
    let root = tempdir("clear");
    let big = root.join("big.bin");
    write_file(&big, 32 * 1024 * 1024);
    let dest = destination(&root, "dest");

    let queue = test_queue();
    let running = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&big)],
            VfsPath::local(&dest),
        ))
        .unwrap();
    assert_eq!(settle(&queue, running).state, JobState::Succeeded);
    assert_eq!(queue.jobs().len(), 1);

    queue.clear_finished();
    assert!(queue.jobs().is_empty());
    assert!(!queue.is_active(), "a cleared job is not still running");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn clearing_never_drops_a_job_that_is_still_going() {
    let root = tempdir("clear-running");
    let big = root.join("big.bin");
    write_file(&big, 1024);
    let dest = destination(&root, "dest");

    let (queue, gate) = gated_queue();
    let id = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&big)],
            VfsPath::local(&dest),
        ))
        .unwrap();

    gate.await_entry();
    // A queue that forgot what it was copying would finish a job nobody could
    // see and report it as a success.
    queue.clear_finished();
    assert!(queue.job(id).is_some(), "the running job was dropped");
    assert!(queue.is_active());

    gate.open();
    assert_eq!(settle(&queue, id).state, JobState::Succeeded);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn an_observer_is_told_about_a_job_and_stops_being_called_when_dropped() {
    let root = tempdir("observe");
    let file = root.join("a.bin");
    write_file(&file, 1_024);

    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    let queue = test_queue();
    {
        let _observer = queue.observe(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        let id = queue
            .enqueue(Request::copy(
                vec![VfsPath::local(&file)],
                VfsPath::local(destination(&root, "dest")),
            ))
            .unwrap();
        settle(&queue, id);
    }
    let after_drop = seen.load(Ordering::Relaxed);
    assert!(after_drop > 0, "the observer heard nothing at all");

    // A dropped observer must not keep being called, or a closed window leaks
    // into every later job.
    let later = queue
        .enqueue(Request::copy(
            vec![VfsPath::local(&file)],
            VfsPath::local(destination(&root, "dest2")),
        ))
        .unwrap();
    settle(&queue, later);
    assert_eq!(seen.load(Ordering::Relaxed), after_drop);

    std::fs::remove_dir_all(&root).ok();
}
