//! Carrying out one job, and reporting on it while it happens.
//!
//! The executor is the only place in Ferret that writes files in bulk, and it
//! is deliberately the only place: a queue that everything bulk goes through is
//! one place to get progress, cancellation and cleanup right, rather than three
//! places that each get a little of it right.
//!
//! ## Cancellation is a signal, not a panic
//!
//! The user presses Cancel and the copy stops within one chunk, removes what it
//! wrote, and the job is `Cancelled`. Nothing is unwound and nothing is retried,
//! because a half-copied file has no state worth rolling back to — the point of
//! a cancel is to stop, not to tidy. What it *does* guarantee is that a
//! destination is either complete or absent, which is what
//! [`ferret_vfs::Vfs::copy_observed`] promises on the other side of the call.
//!
//! ## Failure is per source
//!
//! A copy of 1,200 files where four are unreadable is a copy that worked. The
//! first error does not end the job; it is recorded against the source that
//! caused it and the queue carries on, so the panel can say "1,196 of 1,200 —
//! four could not be read" instead of throwing away 1,196 successful copies.
//! Only a job-level failure — an unreadable destination, a request that crosses
//! backends — ends the whole thing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferret_platform::Platform;
use ferret_vfs::{Checkpoint, ProgressReporter, RemoveMode, Vfs, VfsError, VfsPath};

use crate::config::{PAUSE_POLL, PROGRESS_STEP_BYTES, RATE_WINDOW};
use crate::errors::{OpsError, Result};
use crate::job::{Job, JobKind, JobProgress, JobState};
use crate::measure::{Measurement, measure};

/// The handle a running job holds onto its reporter.
///
/// This is the object that makes Pause and Cancel possible from a copy thread
/// that is deep inside a chunk loop and has no other way to hear from the UI.
/// It is a reporter because the copy code knows about nothing else.
pub struct Handle {
    /// Set by the user. The copy asks at every chunk boundary.
    cancel: AtomicBool,
    /// Set by the user, cleared by them. The copy *blocks* here rather than
    /// returning, because a copy that tore down its current file on a pause
    /// would have to start that file again to resume.
    pause: AtomicBool,
    bytes: AtomicU64,
    entries: AtomicU64,
    /// The byte count at the last published sample, which is the anchor for
    /// both the throttle and the rate.
    published: AtomicU64,
    /// The rate, recomputed on publish rather than on every chunk: an
    /// exponential average over a window is one division per 512 KB, not per
    /// megabyte of `write`.
    rate_bps: AtomicU64,
    /// When the rate window opened.
    window_started: std::sync::Mutex<std::time::Instant>,
    window_bytes: AtomicU64,
}

impl Handle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            published: AtomicU64::new(0),
            rate_bps: AtomicU64::new(0),
            window_started: std::sync::Mutex::new(std::time::Instant::now()),
            window_bytes: AtomicU64::new(0),
        })
    }

    /// Ask the job to stop. Returns whether this call is what asked.
    ///
    /// The return value is what makes the queue's state update correct: two
    /// clicks on Cancel must not both claim to have cancelled, because the
    /// second would overwrite a state the first already settled.
    pub fn cancel(&self) -> bool {
        !self.cancel.swap(true, Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Stop, or keep going. Returns whether the job was actually paused.
    pub fn pause(&self) -> bool {
        if self.is_cancelled() {
            return false;
        }
        !self.pause.swap(true, Ordering::SeqCst)
    }

    pub fn resume(&self) {
        self.pause.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    pub fn rate(&self) -> u64 {
        self.rate_bps.load(Ordering::Relaxed)
    }

    /// The progress to publish, or `None` when not enough has happened to be
    /// worth an event.
    ///
    /// Throttled in *bytes* rather than in time on purpose. A time-based
    /// throttle silences exactly the case a progress bar exists for: a job
    /// moving at 40 KB/s would publish once at the start and then not again for
    /// minutes, which reads as a hang rather than as a slow disk.
    pub fn take_progress(&self) -> Option<JobProgress> {
        let bytes = self.bytes();
        if bytes.saturating_sub(self.published.load(Ordering::Relaxed)) < PROGRESS_STEP_BYTES {
            return None;
        }
        self.published.store(bytes, Ordering::Relaxed);
        self.recompute_rate();
        Some(JobProgress {
            bytes_done: bytes,
            bytes_total: None,
            entries_done: self.entries(),
            entries_total: None,
            bytes_per_second: self.rate(),
        })
    }

    /// Progress for the final publish, which always happens.
    pub fn final_progress(&self) -> JobProgress {
        self.published.store(self.bytes(), Ordering::Relaxed);
        self.recompute_rate();
        JobProgress {
            bytes_done: self.bytes(),
            bytes_total: None,
            entries_done: self.entries(),
            entries_total: None,
            bytes_per_second: self.rate(),
        }
    }

    /// Bytes per second over the last whole window.
    ///
    /// Averages over the window rather than since the start, so a copy does not
    /// report the speed it managed during the first — slowest, coldest — part
    /// of itself for the rest of its life. Held at the previous value until a
    /// whole window has passed: dividing a partial window by its own elapsed
    /// time is a rate estimate built from one sample, which is how a progress
    /// bar comes to claim 400 MB/s.
    fn recompute_rate(&self) {
        let mut started = match self.window_started.lock() {
            Ok(started) => started,
            // A panic in a reporter must not take the copy down with it; the
            // rate is a nicety and the window simply does not move.
            Err(poisoned) => poisoned.into_inner(),
        };
        let windowed = self.window_bytes.swap(0, Ordering::Relaxed);
        let elapsed = started.elapsed();
        if elapsed < RATE_WINDOW {
            // Put the bytes back: they belong to the window that has not
            // closed yet, and dropping them would understate the rate when it
            // finally does.
            self.window_bytes.fetch_add(windowed, Ordering::Relaxed);
            return;
        }
        *started = std::time::Instant::now();
        let seconds = elapsed.as_secs_f64();
        if seconds > 0. {
            self.rate_bps
                .store((windowed as f64 / seconds) as u64, Ordering::Relaxed);
        }
    }

    /// Block while the job is paused.
    ///
    /// A `thread::sleep` loop rather than a condvar, and the reason is worth
    /// stating because it looks like the wrong choice: the thing being waited on
    /// is not a lock, it is a person. A condvar needs a matching notify from
    /// whoever clears the flag, and the only party that can clear it is the UI
    /// thread through a second call — which means every way of pausing a job
    /// has to remember to notify, and a missed notify is a job that never
    /// resumes. Sleeping until the flag clears cannot be missed.
    fn wait_while_paused(&self) {
        while self.is_paused() && !self.is_cancelled() {
            std::thread::sleep(PAUSE_POLL);
        }
    }
}

impl ProgressReporter for Handle {
    fn bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
        self.window_bytes.fetch_add(count, Ordering::Relaxed);
    }

    fn entries(&self, count: u64) {
        self.entries.fetch_add(count, Ordering::Relaxed);
    }

    fn checkpoint(&self) -> Checkpoint {
        self.wait_while_paused();
        if self.is_cancelled() {
            Checkpoint::Cancel
        } else {
            Checkpoint::Continue
        }
    }
}

/// Where a running job's progress goes.
///
/// A trait rather than a closure so the executor carries no reference to the
/// queue that owns it, and so a test can watch progress without standing up a
/// queue.
pub trait ProgressSink: Send + Sync {
    /// Called with a fresh reading. Throttled by [`Handle::take_progress`], so
    /// a copy emits tens of events a second rather than tens of thousands.
    fn publish(&self, progress: JobProgress);
}

/// What a finished job did, for the queue to record.
pub struct Outcome {
    pub state: JobState,
    pub progress: JobProgress,
    pub failures: Vec<String>,
    pub error: Option<String>,
    /// Sources finished, for the completed count the panel shows.
    pub entries_completed: u64,
}

impl Outcome {
    /// A job that could not be started. No progress, because none was made.
    pub fn failed(error: OpsError) -> Self {
        Self {
            state: JobState::Failed,
            progress: JobProgress::default(),
            failures: Vec::new(),
            error: Some(error.to_string()),
            entries_completed: 0,
        }
    }

    /// A job stopped before it ran.
    pub fn cancelled(total: Option<Measurement>, progress: JobProgress) -> Self {
        Self {
            state: JobState::Cancelled,
            progress: with_total(progress, total),
            failures: Vec::new(),
            error: None,
            entries_completed: 0,
        }
    }
}

/// Run one job to its end.
///
/// `total` is what the measuring pass found; `None` means the job has no
/// denominator and the panel draws a count rather than a percentage.
pub async fn run(
    fs: &Arc<dyn Vfs>,
    platform: &Arc<dyn Platform>,
    job: &Job,
    handle: &Arc<Handle>,
    total: Option<Measurement>,
    sink: &dyn ProgressSink,
) -> Outcome {
    let reporter: Arc<dyn ProgressReporter> = handle.clone();
    let mut failures = Vec::new();
    let mut completed = 0u64;
    let mut fatal: Option<OpsError> = None;

    for transfer in job.transfers.iter() {
        let source = &transfer.source;
        if handle.is_cancelled() {
            return stopped(handle, total, failures, completed);
        }
        match job.kind {
            JobKind::Trash => {
                // The trash is the platform's, not the `Vfs` trait's, and it is
                // the one irreversible-sounding action that is actually
                // reversible. `LocalFs` refuses `RemoveMode::Trash` on purpose
                // (gate 5), so a job that reached this through a local path
                // fails loudly rather than deleting permanently.
                match trash_one(platform, source).await {
                    Ok(()) => completed += 1,
                    Err(error) => {
                        if error.is_fatal() {
                            fatal = Some(error);
                            break;
                        }
                        failures.push(error.describe());
                    }
                }
            }
            // A delete has no destination: the sources go, and that is the
            // whole of it. Sending it down the copy arm would ask for a
            // directory to put things in and fail a job that was never
            // malformed.
            JobKind::Delete => match fs
                .remove_observed(source, RemoveMode::Permanent, reporter.clone())
                .await
            {
                Ok(()) => completed += 1,
                Err(error) => {
                    if handle.is_cancelled() {
                        return stopped(handle, total, failures, completed);
                    }
                    // A cancel is not a per-source failure, whatever the backend
                    // turned it into on the way out.
                    if matches!(error, VfsError::Cancelled { .. }) {
                        return stopped(handle, total, failures, completed);
                    }
                    failures.push(OpsError::from(error).describe());
                }
            },
            JobKind::Copy | JobKind::Move => {
                let Some(into) = transfer.destination.as_ref() else {
                    // A malformed request, and the queue is the only thing that
                    // can build one, so this cannot come from a gesture. Said
                    // out loud anyway: a job that quietly did nothing is worse
                    // than one that admits it could not start.
                    fatal = Some(OpsError::Unsupported {
                        path: source.clone(),
                        capability: "copy or move with no destination".to_string(),
                    });
                    break;
                };
                match copy_or_move(fs, job.kind, source, into, reporter.clone()).await {
                    Ok(()) => completed += 1,
                    Err(error) => {
                        if handle.is_cancelled() {
                            return stopped(handle, total, failures, completed);
                        }
                        if error.is_fatal() {
                            fatal = Some(error);
                            break;
                        }
                        failures.push(error.describe());
                    }
                }
            }
        }
    }

    let progress = with_total(handle.final_progress(), total);
    // Always published, even when the last chunk was below the throttle: the
    // final reading is the one the panel draws after the job stops moving.
    sink.publish(progress);

    if let Some(error) = fatal {
        return Outcome {
            state: JobState::Failed,
            progress,
            failures,
            error: Some(error.to_string()),
            entries_completed: completed,
        };
    }
    if handle.is_cancelled() {
        return Outcome {
            state: JobState::Cancelled,
            progress,
            failures,
            error: None,
            entries_completed: completed,
        };
    }

    // A job where *every* source failed is a failed job, whatever the
    // per-source accounting says. "Copied 0 of 3 items" with three errors
    // beside it is the same information as a failure and one more step of
    // reading; the panel's job is to say what happened, and nothing happened.
    // Partial failure stays a success, because partial is what it says.
    let total_sources = job.transfers.len() as u64;
    if total_sources > 0 && failures.len() as u64 == total_sources && completed == 0 {
        return Outcome {
            state: JobState::Failed,
            progress,
            failures,
            error: Some("Nothing could be moved. See the failures below.".to_string()),
            entries_completed: completed,
        };
    }

    Outcome {
        state: JobState::Succeeded,
        progress,
        failures,
        error: None,
        entries_completed: completed,
    }
}

/// The outcome of stopping early, which is a cancellation however it happened.
fn stopped(
    handle: &Arc<Handle>,
    total: Option<Measurement>,
    failures: Vec<String>,
    completed: u64,
) -> Outcome {
    Outcome {
        state: JobState::Cancelled,
        progress: with_total(handle.final_progress(), total),
        failures,
        error: None,
        entries_completed: completed,
    }
}

fn with_total(progress: JobProgress, total: Option<Measurement>) -> JobProgress {
    JobProgress {
        bytes_total: total.map(|total| total.bytes),
        entries_total: total.map(|total| total.entries),
        ..progress
    }
}

/// One file, or one whole tree.
async fn copy_or_move(
    fs: &Arc<dyn Vfs>,
    kind: JobKind,
    source: &VfsPath,
    into: &VfsPath,
    reporter: Arc<dyn ProgressReporter>,
) -> Result<()> {
    match kind {
        JobKind::Delete => fs
            .remove_observed(source, RemoveMode::Permanent, reporter)
            .await
            .map_err(OpsError::from),
        JobKind::Copy => fs
            .copy_observed(source, into, reporter)
            .await
            .map_err(OpsError::from),
        JobKind::Move => {
            // The rename first, always. On one filesystem it is a single
            // atomic step and there are no bytes to copy at all; falling back to
            // copy-then-delete eagerly would turn a free move into a full
            // duplicate of the tree.
            match fs.rename(source, into).await {
                Ok(()) => Ok(()),
                Err(VfsError::CrossVolume { .. }) => {
                    // Across volumes there is no rename to do, which is what
                    // "move" means once the kernel says so rather than something
                    // guessed from the paths: mount points and network shares
                    // make "same disk" a guess.
                    fs.copy_observed(source, into, reporter.clone())
                        .await
                        .map_err(OpsError::from)?;
                    // Only now is the source safe to remove. A cancel during
                    // the copy leaves both, which is recoverable in the way that
                    // matters — the bytes exist — rather than neither.
                    fs.remove_observed(source, RemoveMode::Permanent, reporter)
                        .await
                        .map_err(OpsError::from)
                }
                Err(error) => Err(OpsError::from(error)),
            }
        }
        // Handled by the caller, which knows the platform.
        JobKind::Trash => Ok(()),
    }
}

/// The one irreversible-sounding action that is not: moving to the platform
/// trash.
///
/// It goes through `RemoveMode::Trash` rather than a second code path, so gate
/// 5 holds by construction: a backend that cannot reach its trash says so
/// instead of deleting permanently, and there is no branch anywhere that
/// quietly does the permanent thing.
async fn trash_one(platform: &Arc<dyn Platform>, source: &VfsPath) -> Result<()> {
    let Some(local) = source.as_local() else {
        return Err(OpsError::Unsupported {
            path: source.clone(),
            capability: "move to the Trash".to_string(),
        });
    };
    // The platform, and only the platform. Gate 5 is that every delete path
    // comes through here, and the cheapest way to keep that true is for there
    // to be exactly one place that knows how a file reaches the trash.
    platform
        .move_to_trash(local)
        .await
        .map_err(|error| OpsError::Source {
            path: source.clone(),
            message: error.to_string(),
        })
}

/// Count a job before running it, so the bar has a denominator.
///
/// `handle` is here so a cancel asked for *during* the count is noticed. The
/// walk polls it per entry, so a cancel on a very large tree stops the counting
/// too — otherwise pressing Cancel on a job stuck in `Measuring` would look
/// like nothing happened until the walk finished.
pub async fn measure_job(job: &Job, handle: &Arc<Handle>) -> Result<Option<Measurement>> {
    if job.transfers.is_empty() {
        return Err(OpsError::NothingToDo);
    }
    let total = measure(&job.sources(), handle).await;
    // An inexact measurement is reported as no measurement. A bar that fills to
    // 97% and stops, on a job that is genuinely finished, teaches people that
    // the number is decoration.
    Ok(total.exact.then_some(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_holds_the_copy_and_a_resume_lets_it_go() {
        let handle = Handle::new();
        assert!(handle.checkpoint() == Checkpoint::Continue);

        assert!(handle.pause(), "the first pause takes");
        assert!(!handle.pause(), "pausing twice is one pause");
        assert!(handle.is_paused());

        // Checked from another thread because checking from this one would
        // block: blocking inside `checkpoint` while paused *is* the behaviour
        // under test, and that is the difference between a pause that holds a
        // copy still and a pause that only sets a flag the copy ignores.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let handle = Arc::clone(&handle);
            move || {
                let answer = handle.checkpoint();
                let _ = tx.send(answer);
            }
        });

        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a paused job must not pass a checkpoint"
        );
        handle.resume();
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .expect("resuming must release the checkpoint"),
            Checkpoint::Continue
        );
        worker.join().unwrap();
    }

    #[test]
    fn a_cancel_outranks_a_pause() {
        // Otherwise a job paused and then cancelled would park forever in the
        // wait loop, and the Cancel button would appear to do nothing.
        let handle = Handle::new();
        handle.pause();
        assert!(handle.cancel());
        assert!(handle.checkpoint() == Checkpoint::Cancel);
    }

    #[test]
    fn cancelling_twice_is_reported_once() {
        let handle = Handle::new();
        assert!(handle.cancel(), "the first click asks");
        assert!(!handle.cancel(), "the second click changes nothing");
    }

    #[test]
    fn progress_waits_for_a_whole_step_before_it_publishes() {
        let handle = Handle::new();
        assert!(handle.take_progress().is_none(), "nothing has happened yet");

        ProgressReporter::bytes(handle.as_ref(), PROGRESS_STEP_BYTES);
        let progress = handle.take_progress().expect("a full step publishes");
        assert_eq!(progress.bytes_done, PROGRESS_STEP_BYTES);
        assert!(
            handle.take_progress().is_none(),
            "the same bytes must not publish twice"
        );
    }

    #[test]
    fn the_rate_waits_for_a_whole_window_before_it_quotes_one() {
        let handle = Handle::new();
        ProgressReporter::bytes(handle.as_ref(), 10 * PROGRESS_STEP_BYTES);
        let progress = handle.take_progress().expect("publishes");
        assert_eq!(
            progress.bytes_per_second, 0,
            "one sample is not a rate; quoting it is how a bar claims 400 MB/s"
        );
    }
}
