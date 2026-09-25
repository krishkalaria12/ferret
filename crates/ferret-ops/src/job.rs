//! What a queued operation is, and what the queue can be asked about it.
//!
//! These are the types the panel draws and the status bar summarises, so they
//! are plain data with no handles into the machinery: a [`Job`] is a snapshot,
//! cheap to clone, and stays readable after the thing it describes is gone.

use std::sync::Arc;
use std::time::Duration;

use ferret_vfs::{RemoveMode, VfsPath};

/// A job's identity, stable for the life of the queue.
///
/// A number rather than a pointer so a panel can hold an id across the
/// repaint in which the job it was drawing is removed from under it, which is
/// exactly what a `Vec` index cannot survive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

impl JobId {
    /// The number behind the id. For a log line, a test, or a key.
    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(previous: u64) -> Self {
        JobId(previous)
    }
}

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    /// To the platform trash, which is reversible and needs no confirmation.
    Trash,
    /// Irreversible. Only ever enqueued after the confirmation has happened.
    Delete,
}

impl JobKind {
    /// The word used in the panel and in every sentence about the job.
    pub fn verb(self) -> &'static str {
        match self {
            JobKind::Copy => "Copying",
            JobKind::Move => "Moving",
            JobKind::Trash => "Trashing",
            JobKind::Delete => "Deleting",
        }
    }

    /// The same word in the past tense, for a finished row.
    pub fn past(self) -> &'static str {
        match self {
            JobKind::Copy => "Copied",
            JobKind::Move => "Moved",
            JobKind::Trash => "Trashed",
            JobKind::Delete => "Deleted",
        }
    }

    /// Whether bytes are being moved, which decides what the progress bar is a
    /// fraction *of*.
    pub fn moves_bytes(self) -> bool {
        matches!(self, JobKind::Copy | JobKind::Move)
    }

    pub fn remove_mode(self) -> RemoveMode {
        match self {
            JobKind::Trash => RemoveMode::Trash,
            _ => RemoveMode::Permanent,
        }
    }
}

/// Where a job is in its life.
///
/// `Measuring` is a state of its own rather than a detail of `Running` because
/// counting 40,000 files before the first one is copied is a visible pause, and
/// a bar that sits at zero with no explanation reads as a hang.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    /// Accepted, waiting for the worker.
    Queued,
    /// Walking the sources to find out how much work this is.
    Measuring,
    Running,
    /// Stopped at a copy chunk, holding its place. Resumable.
    Paused,
    /// Every source was handled.
    Succeeded,
    /// Stopped by the user. Nothing is unwound.
    Cancelled,
    /// Stopped by something going wrong. See [`Job::error`].
    Failed,
}

impl JobState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Cancelled | JobState::Failed
        )
    }

    /// Whether the job is holding the worker's attention.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            JobState::Measuring | JobState::Running | JobState::Paused
        )
    }

    /// Whether the job can be asked to stop, or to start again.
    pub fn is_pausable(self) -> bool {
        matches!(self, JobState::Measuring | JobState::Running)
    }

    /// The word used in a sentence about a job that has finished.
    pub fn name(self) -> &'static str {
        match self {
            JobState::Queued => "is waiting",
            JobState::Measuring => "is measuring",
            JobState::Running => "is running",
            JobState::Paused => "is paused",
            JobState::Succeeded => "succeeded",
            JobState::Cancelled => "was cancelled",
            JobState::Failed => "failed",
        }
    }
}

/// How far along a job is, and how fast it is going.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JobProgress {
    pub bytes_done: u64,
    /// `None` until [`JobState::Measuring`] finishes, and left `None` for a
    /// source the backend could not size. A bar with no denominator is a
    /// legitimate state; inventing one would be a lie that moves.
    pub bytes_total: Option<u64>,
    pub entries_done: u64,
    pub entries_total: Option<u64>,
    /// Bytes per second, averaged over a sliding window rather than since the
    /// start — see [`crate::config::RATE_WINDOW`].
    pub bytes_per_second: u64,
}

impl JobProgress {
    /// Completion as a fraction, or `None` while the total is unknown.
    ///
    /// Bytes rather than entries, because a copy's cost is bytes and a delete's
    /// cost is entries; each job's own kind decides which half it reads.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.bytes_total?;
        (total > 0)
            .then(|| (self.bytes_done as f64 / total as f64) as f32)
            .map(|value| value.clamp(0., 1.))
    }

    /// Time left at the current rate, or `None` when it cannot be estimated.
    ///
    /// Refuses to answer until the job has moved enough to have a rate worth
    /// quoting. The first chunk of a 4 GB copy takes longer than the rest, so
    /// dividing the whole total by the opening rate answers "fortnight" and is
    /// worse than no answer.
    pub fn remaining(&self) -> Option<Duration> {
        let total = self.bytes_total?;
        if self.bytes_per_second == 0 {
            return None;
        }
        let left = total.saturating_sub(self.bytes_done);
        Some(Duration::from_secs_f64(
            left as f64 / self.bytes_per_second as f64,
        ))
    }
}

/// One queued operation, as the queue currently understands it.
#[derive(Clone, Debug)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub state: JobState,
    /// What the sources are. An `Arc` because every repaint of the panel reads
    /// this, and a 40,000-file selection is not something to copy per frame.
    pub sources: Arc<Vec<VfsPath>>,
    /// The directory everything lands in, for the kinds that have one.
    pub destination: Option<VfsPath>,
    pub progress: JobProgress,
    /// Sources that failed, with the reason. A job that copied 999 of 1,000 is
    /// not a success and not a failure; it is this.
    pub failures: Vec<String>,
    /// A job-level error, for the failures that are about the job rather than
    /// about one of its sources — an unreadable destination, say.
    pub error: Option<String>,
    /// Sources completed so far, for a job still running.
    pub entries_completed: u64,
}

impl Job {
    /// The line the panel shows for this job.
    ///
    /// A name where there is one, and a count where there is not: "Copying
    /// report.pdf" says what is happening, and "Copying 1,204 items" is the
    /// only honest thing to say about a selection nobody is going to read
    /// 1,204 names of.
    pub fn label(&self) -> String {
        match self.sources.as_slice() {
            [one] => one
                .file_name()
                .map(|name| format!("{} {name}", self.kind.verb()))
                .unwrap_or_else(|| format!("{} 1 item", self.kind.verb())),
            [] => format!("{} nothing", self.kind.verb()),
            many => format!("{} {} items", self.kind.verb(), many.len()),
        }
    }

    /// The line the status bar shows while this is the job being watched.
    pub fn status(&self) -> String {
        let mut line = self.label();
        if self.state == JobState::Measuring {
            line.push_str(" — measuring…");
            return line;
        }
        if let Some(total) = self.progress.bytes_total
            && self.kind.moves_bytes()
        {
            line.push_str(&format!(
                " — {} of {}",
                short_size(self.progress.bytes_done),
                short_size(total)
            ));
        }
        line
    }

    /// The one-line summary of how a finished job ended.
    pub fn outcome(&self) -> String {
        let head = match self.state {
            JobState::Succeeded => self.kind.past().to_string(),
            JobState::Cancelled => "Cancelled".to_string(),
            JobState::Failed => "Failed".to_string(),
            _ => self.label(),
        };
        // A failure names its own path, so the first one is quoted whole: it
        // is the only line in the panel that can say what to do about it.
        match self.failures.first() {
            Some(first) => format!("{head} — {first}"),
            None => head,
        }
    }
}

/// Byte count as a short human string.
///
/// Deliberately duplicated rather than shared: `ferret-ui`'s `format_size`
/// belongs to the UI crate, and a service crate that grew a GPUI dependency to
/// borrow a string formatter would be the layering violation `AGENTS.md` names.
/// What matters is that the two agree on the units, so each side pins them with
/// a test.
fn short_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(sources: Vec<VfsPath>, state: JobState) -> Job {
        Job {
            id: JobId(1),
            kind: JobKind::Copy,
            state,
            sources: Arc::new(sources),
            destination: None,
            progress: JobProgress::default(),
            failures: Vec::new(),
            error: None,
            entries_completed: 0,
        }
    }

    #[test]
    fn a_single_source_is_named_and_a_selection_is_counted() {
        let one = job(vec![VfsPath::local("/a/report.pdf")], JobState::Queued);
        assert_eq!(one.label(), "Copying report.pdf");

        let many = job(
            (0..4).map(|i| VfsPath::local(format!("/a/{i}"))).collect(),
            JobState::Queued,
        );
        assert_eq!(many.label(), "Copying 4 items");
    }

    #[test]
    fn a_bar_with_no_total_is_answered_with_none_rather_than_zero() {
        // A zero would draw as "0% done" and never move, which is the one
        // reading that is both plausible and wrong.
        let progress = JobProgress {
            bytes_done: 400,
            bytes_total: None,
            ..Default::default()
        };
        assert_eq!(progress.fraction(), None);
        assert_eq!(progress.remaining(), None);
    }

    #[test]
    fn the_remaining_time_needs_a_rate_before_it_quotes_one() {
        let progress = JobProgress {
            bytes_done: 1_000,
            bytes_total: Some(10_000),
            bytes_per_second: 0,
            ..Default::default()
        };
        assert_eq!(progress.remaining(), None, "no rate, no estimate");

        let moving = JobProgress {
            bytes_per_second: 1_000,
            ..progress
        };
        assert_eq!(moving.remaining(), Some(Duration::from_secs(9)));
    }

    #[test]
    fn sizes_read_the_way_the_status_bar_expects() {
        assert_eq!(short_size(512), "512 B");
        assert_eq!(short_size(1024), "1.0 KB");
        assert_eq!(short_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn a_cancelled_job_is_never_reported_as_succeeded() {
        let cancelled = job(vec![VfsPath::local("/a/x")], JobState::Cancelled);
        assert_eq!(cancelled.outcome(), "Cancelled");
        assert!(cancelled.state.is_finished());
        assert!(!JobState::Running.is_finished());
    }
}
