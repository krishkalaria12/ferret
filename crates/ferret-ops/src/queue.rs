//! The queue itself: what is waiting, what is running, and how the UI finds
//! out.
//!
//! ## One worker slot at a time
//!
//! [`config::MAX_CONCURRENT_JOBS`], which is one. Two concurrent copies of large
//! trees on one disk make both slower without finishing either sooner, and the
//! queue's promise is that a job is *visible* — a second one silently sharing
//! the disk makes the first one's throughput number a lie.
//!
//! The queue lives in `FerretState` rather than on a view, so a job survives
//! navigating away, switching tabs and closing the panel. There is nothing to
//! keep alive, because nothing here is attached to a window; a copy runs on
//! Ferret's blocking pool and the only thing that could take it down is the
//! process.
//!
//! ## How work gets claimed
//!
//! There is no dispatcher loop and no wake-up channel, which is the important
//! part. [`Queue::claim_ready`] starts as many jobs as the limit allows and
//! returns. Each job, when it settles, calls it again. An enqueue calls it too.
//!
//! A loop with a channel would need every path that changes the queue to
//! remember to send, and a forgotten send is a queue that silently stops — the
//! failure mode is invisible until somebody wonders why their paste never
//! happened. Re-entering a function that decides what to start next cannot be
//! forgotten, because the only way to start a job *is* to call it.
//!
//! ## Why the state is behind a lock and not an `Entity`
//!
//! `ferret-ops` is GPUI-free, and a queue that needed a `Context` to be
//! observable would be a queue no service crate could use. So the state is a
//! plain mutex and observation is a callback. The lock guards a few dozen
//! [`Job`]s and is held for no I/O at all — every filesystem call happens with
//! the lock released, which is the rule that keeps a 40,000-entry measurement
//! from freezing whatever is repainting.

use std::sync::Arc;

use parking_lot::Mutex;

use ferret_platform::Platform;
use ferret_vfs::{Vfs, VfsPath};

use crate::config::{HISTORY_LIMIT, MAX_CONCURRENT_JOBS};
use crate::errors::{OpsError, Result};
use crate::execute::{self, Handle};
use crate::job::{Job, JobId, JobKind, JobProgress, JobState, Transfer};

/// What the queue tells its observers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A job was added, and the panel should show it.
    Added { id: JobId },
    /// A job's state or progress moved. The common case, and deliberately
    /// carries no payload: the panel re-reads [`Queue::job`] rather than
    /// trusting a copy that could already be a frame stale.
    Changed { id: JobId },
}

/// What a caller wants done. The queue decides when.
///
/// The pairs are already named and placed by the time a request gets here. That
/// is the whole reason [`Transfer`] exists: the name a drop settles on — "file
/// 2" beside an existing one — can only be decided where the listing is
/// readable, and a queue that took bare sources would have to decide it again
/// somewhere it cannot see.
#[derive(Clone, Debug)]
pub struct Request {
    pub kind: JobKind,
    pub transfers: Vec<Transfer>,
}

impl Request {
    /// Copy each source into `destination` under its own name.
    pub fn copy(sources: Vec<VfsPath>, destination: VfsPath) -> Self {
        Self::copy_named(
            sources.into_iter().map(|source| (source, None)).collect(),
            destination,
        )
    }

    /// Copy each source into `destination` under a name the caller has already
    /// settled. `None` in the pair means "its own name".
    pub fn copy_named(sources: Vec<(VfsPath, Option<String>)>, destination: VfsPath) -> Self {
        Self {
            kind: JobKind::Copy,
            transfers: join(sources, destination),
        }
    }

    pub fn move_to(sources: Vec<VfsPath>, destination: VfsPath) -> Self {
        Self {
            kind: JobKind::Move,
            transfers: join(
                sources.into_iter().map(|source| (source, None)).collect(),
                destination,
            ),
        }
    }

    pub fn trash(sources: Vec<VfsPath>) -> Self {
        Self {
            kind: JobKind::Trash,
            transfers: sources.into_iter().map(Transfer::away).collect(),
        }
    }

    pub fn delete(sources: Vec<VfsPath>) -> Self {
        Self {
            kind: JobKind::Delete,
            transfers: sources.into_iter().map(Transfer::away).collect(),
        }
    }
}

/// Pair each source with its destination, inventing a name only when the
/// caller did not settle one.
pub fn join(sources: Vec<(VfsPath, Option<String>)>, destination: VfsPath) -> Vec<Transfer> {
    sources
        .into_iter()
        .map(|(source, name)| {
            let name = name.unwrap_or_else(|| source.file_name().unwrap_or_default().to_string());
            Transfer::to(source, destination.join(&name))
        })
        .collect()
}

/// A job plus the handle its running copy reads.
///
/// The two live together because a Pause or a Cancel has to find *this* job's
/// handle. A parallel list indexed by position is the version of this that
/// works until a job is cleared, and then it silently pauses the wrong one.
struct Entry {
    job: Job,
    /// `Some` exactly while the job is being carried out.
    handle: Option<Arc<Handle>>,
}

/// One registered observer, with the id that unregisters it.
type Subscriber = (u64, Arc<dyn Fn(&Event) + Send + Sync>);

struct Inner {
    entries: Mutex<Vec<Entry>>,
    next_id: Mutex<u64>,
    observers: Mutex<Vec<Subscriber>>,
    next_observer: Mutex<u64>,
}

/// The operations queue.
///
/// Cheap to clone; every clone is the same queue.
#[derive(Clone)]
pub struct Queue {
    fs: Arc<dyn Vfs>,
    platform: Arc<dyn Platform>,
    inner: Arc<Inner>,
}

impl Queue {
    pub fn new(fs: Arc<dyn Vfs>, platform: Arc<dyn Platform>) -> Self {
        Self {
            fs,
            platform,
            inner: Arc::new(Inner {
                entries: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
                observers: Mutex::new(Vec::new()),
                next_observer: Mutex::new(1),
            }),
        }
    }

    /// Accept a job.
    ///
    /// Refusals happen here, synchronously, because a queue that accepts a job
    /// and fails it later is worse than one that says no: the user has already
    /// watched it sit in the list. What *is* deferred is the measurement —
    /// that is work, and work belongs to the worker.
    pub fn enqueue(&self, request: Request) -> Result<JobId> {
        if request.transfers.is_empty() {
            return Err(OpsError::NothingToDo);
        }
        if let Some(crossing) = request.transfers.iter().find_map(|transfer| {
            let destination = transfer.destination.as_ref()?;
            (!same_backend(&transfer.source, destination))
                .then(|| (transfer.source.clone(), destination.clone()))
        }) {
            return Err(OpsError::CrossBackend {
                from: crossing.0,
                to: crossing.1,
            });
        }
        if request.kind == JobKind::Trash
            && let Some(remote) = request
                .transfers
                .iter()
                .find(|transfer| !transfer.source.is_local())
                .map(|transfer| transfer.source.clone())
        {
            // There is no trash on a server. The UI splits a mixed selection
            // before it gets here; this is the check that says so if it ever
            // does not.
            return Err(OpsError::Unsupported {
                path: remote.clone(),
                capability: "move to the Trash".to_string(),
            });
        }

        let id = JobId::next(self.next_id());
        let label = Job::describe(
            request.kind,
            &request
                .transfers
                .iter()
                .map(|transfer| transfer.source.clone())
                .collect::<Vec<_>>(),
        );
        let job = Job {
            id,
            kind: request.kind,
            state: JobState::Queued,
            transfers: Arc::new(request.transfers),
            progress: JobProgress::default(),
            failures: Vec::new(),
            error: None,
            entries_completed: 0,
            label,
        };
        {
            let mut entries = self.inner.entries.lock();
            entries.push(Entry { job, handle: None });
            trim(&mut entries);
        }
        self.emit(Event::Added { id });
        self.claim_ready();
        Ok(id)
    }

    /// Every job, oldest first, for the panel.
    pub fn jobs(&self) -> Vec<Job> {
        self.inner
            .entries
            .lock()
            .iter()
            .map(|entry| entry.job.clone())
            .collect()
    }

    /// One job, or `None` once it has been cleared.
    pub fn job(&self, id: JobId) -> Option<Job> {
        self.inner
            .entries
            .lock()
            .iter()
            .find(|entry| entry.job.id == id)
            .map(|entry| entry.job.clone())
    }

    /// Whether anything is waiting or running — what the status-bar segment
    /// keys off, so the common frame pays for one scan rather than a clone of
    /// every job.
    pub fn is_active(&self) -> bool {
        self.inner
            .entries
            .lock()
            .iter()
            .any(|entry| !entry.job.state.is_finished())
    }

    /// The job worth putting in the status bar: the one in flight, or else the
    /// first thing still waiting.
    ///
    /// In flight first, because a bar that follows the *queue* while something
    /// is already running reports on work that has not started.
    pub fn current(&self) -> Option<Job> {
        let entries = self.inner.entries.lock();
        entries
            .iter()
            .find(|entry| matches!(entry.job.state, JobState::Measuring | JobState::Running))
            .or_else(|| entries.iter().find(|entry| !entry.job.state.is_finished()))
            .map(|entry| entry.job.clone())
    }

    /// How many jobs are waiting behind the current one.
    pub fn pending(&self) -> usize {
        self.inner
            .entries
            .lock()
            .iter()
            .filter(|entry| entry.job.state == JobState::Queued)
            .count()
    }

    /// Stop a job, now or before it starts.
    pub fn cancel(&self, id: JobId) -> Result<()> {
        enum Action {
            Settle(JobState),
            AskHandle,
            AlreadyFinished(JobState),
        }

        let action = {
            let mut entries = self.inner.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.job.id == id) else {
                return Err(OpsError::UnknownJob);
            };
            match entry.job.state {
                JobState::Cancelled => Action::Settle(JobState::Cancelled),
                state if state.is_finished() => Action::AlreadyFinished(state),
                // Nothing has touched it, so it can be settled outright rather
                // than waiting for a worker to pick it up and decline.
                JobState::Queued => Action::Settle(JobState::Cancelled),
                _ => match entry.handle.as_ref() {
                    Some(handle) => {
                        handle.cancel();
                        Action::AskHandle
                    }
                    // Measuring: the walk is still going and has no handle to
                    // read, but it polls, so the state can be settled here.
                    None => Action::Settle(JobState::Cancelled),
                },
            }
        };

        match action {
            Action::Settle(state) => {
                self.update(id, |job| job.state = state);
            }
            Action::AskHandle => {
                // The handle decides. Writing `Cancelled` here would claim a
                // state the bytes on disk do not match yet: the copy stops
                // within a chunk, removes what it wrote, and *then* settles.
                // `Paused` is the honest interim — the job is on its way out and
                // the panel shows a stop that has been asked for.
                self.update(id, |job| job.state = JobState::Paused);
            }
            Action::AlreadyFinished(state) => {
                return Err(OpsError::AlreadySettled {
                    state: state.name(),
                });
            }
        }
        Ok(())
    }

    /// Stop everything and take the queued jobs out of the list.
    ///
    /// For a panic button. Queued jobs are *removed* rather than marked
    /// cancelled, because a cancelled row for work that never started is a lie
    /// about work that was never done.
    pub fn cancel_all(&self) {
        let mut entries = self.inner.entries.lock();
        entries.retain_mut(|entry| match entry.job.state {
            JobState::Queued => false,
            state if state.is_finished() => true,
            _ => {
                if let Some(handle) = entry.handle.as_ref() {
                    handle.cancel();
                }
                entry.job.state = JobState::Paused;
                true
            }
        });
    }

    /// Hold a job where it is, or let it go again.
    ///
    /// Returns whether the job is now paused, so the caller can render the
    /// button without a second read.
    pub fn set_paused(&self, id: JobId, paused: bool) -> Result<bool> {
        let mut entries = self.inner.entries.lock();
        let Some(entry) = entries.iter_mut().find(|entry| entry.job.id == id) else {
            return Err(OpsError::UnknownJob);
        };
        let Some(handle) = entry.handle.clone() else {
            // Queued or finished: there is nothing to hold, and saying so is
            // better than flipping a state that will be overwritten the moment
            // the worker claims it.
            return Ok(entry.job.state == JobState::Paused);
        };
        let now_paused = if paused {
            handle.pause()
        } else {
            handle.resume();
            false
        };
        if entry.job.state.is_pausable() {
            entry.job.state = if now_paused {
                JobState::Paused
            } else {
                JobState::Running
            };
        }
        Ok(now_paused)
    }

    /// Take the finished jobs out of the list.
    pub fn clear_finished(&self) {
        let mut entries = self.inner.entries.lock();
        entries.retain(|entry| !entry.job.state.is_finished());
    }

    /// Take one finished job out of the list.
    pub fn clear(&self, id: JobId) {
        let mut entries = self.inner.entries.lock();
        if let Some(index) = entries.iter().position(|entry| entry.job.id == id)
            && entries[index].job.state.is_finished()
        {
            entries.remove(index);
        }
    }

    /// Ask to be told about changes.
    ///
    /// A callback rather than a channel: the consumer is a GPUI view that has
    /// to be woken on its own thread, and a `Fn` the queue calls from a worker
    /// is the only shape that does not also force a buffering policy on
    /// something whose events are all "re-read the state". The returned token
    /// unregisters on drop, so a closed window stops being called.
    pub fn observe(&self, observer: impl Fn(&Event) + Send + Sync + 'static) -> Observer {
        let id = {
            let mut next = self.inner.next_observer.lock();
            let id = *next;
            *next += 1;
            id
        };
        self.inner.observers.lock().push((id, Arc::new(observer)));
        Observer {
            inner: Arc::clone(&self.inner),
            id,
        }
    }

    /// Apply `edit` to one job, then tell the observers.
    fn update(&self, id: JobId, edit: impl FnOnce(&mut Job)) {
        {
            let mut entries = self.inner.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.job.id == id) {
                edit(&mut entry.job);
            }
        }
        self.emit(Event::Changed { id });
    }

    fn emit(&self, event: Event) {
        // The `Arc`s are cloned out before any call, and the lock is dropped
        // first. An observer that enqueues — which a retry button plausibly
        // could — would otherwise re-enter this lock, and the call happens on a
        // copy thread where a deadlock is a hang rather than an error.
        let observers: Vec<Subscriber> = self.inner.observers.lock().iter().cloned().collect();
        for (_, observer) in observers {
            observer(&event);
        }
    }

    fn next_id(&self) -> u64 {
        let mut next = self.inner.next_id.lock();
        let id = *next;
        *next += 1;
        id
    }

    /// Start as many queued jobs as the limit allows.
    ///
    /// Called from [`Queue::enqueue`] and again by every job as it settles, so
    /// nothing has to remember to nudge the queue.
    fn claim_ready(&self) {
        for _ in 0..MAX_CONCURRENT_JOBS {
            let Some((job, handle)) = self.claim_one() else {
                return;
            };
            self.spawn_job(job, handle);
        }
    }

    /// Move the oldest queued job to `Measuring` and hand back its handle.
    fn claim_one(&self) -> Option<(Job, Arc<Handle>)> {
        let mut entries = self.inner.entries.lock();
        let active = entries
            .iter()
            .filter(|entry| entry.handle.is_some())
            .count();
        if active >= MAX_CONCURRENT_JOBS {
            return None;
        }
        let entry = entries
            .iter_mut()
            .find(|entry| entry.job.state == JobState::Queued)?;
        entry.job.state = JobState::Measuring;
        let handle = Handle::new();
        entry.handle = Some(handle.clone());
        let id = entry.job.id;
        drop(entries);
        self.emit(Event::Changed { id });
        Some((self.job(id)?, handle))
    }

    /// Carry out one job, then let the next one start.
    fn spawn_job(&self, job: Job, handle: Arc<Handle>) {
        let queue = self.clone();
        let id = job.id;
        smol::spawn(async move {
            let total = match execute::measure_job(&job, &handle).await {
                Ok(total) => total,
                Err(error) => {
                    queue.settle(id, &handle, execute::Outcome::failed(error));
                    queue.claim_ready();
                    return;
                }
            };
            if queue
                .inner
                .entries
                .lock()
                .iter()
                .any(|entry| entry.job.id == id && entry.job.state == JobState::Cancelled)
            {
                queue.settle(
                    id,
                    &handle,
                    execute::Outcome::cancelled(total, handle.final_progress()),
                );
                queue.claim_ready();
                return;
            }
            queue.update(id, |job| {
                job.state = JobState::Running;
                job.progress.bytes_total = total.map(|total| total.bytes);
                job.progress.entries_total = total.map(|total| total.entries);
            });

            let outcome =
                execute::run(&queue.fs, &queue.platform, &job, &handle, total, &queue).await;
            queue.settle(id, &handle, outcome);
            queue.claim_ready();
        })
        .detach();
    }

    /// The id of the job currently being carried out.
    fn running_id(&self) -> Option<JobId> {
        self.inner
            .entries
            .lock()
            .iter()
            .find(|entry| entry.handle.is_some())
            .map(|entry| entry.job.id)
    }

    /// Record how a job ended, and let go of its handle.
    fn settle(&self, id: JobId, handle: &Arc<Handle>, outcome: execute::Outcome) {
        {
            let mut entries = self.inner.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.job.id == id) {
                entry.job.state = outcome.state;
                entry.job.progress = outcome.progress;
                entry.job.failures = outcome.failures;
                entry.job.error = outcome.error;
                entry.job.entries_completed = outcome.entries_completed;
                entry.handle = None;
            }
        }
        let _ = handle;
        self.emit(Event::Changed { id });
    }
}

/// Unregisters an observer when dropped.
pub struct Observer {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.inner.observers.lock().retain(|(id, _)| *id != self.id);
    }
}

/// Drop the oldest finished jobs once the list is over its limit.
///
/// The queue outlives navigation, and quitting is not the only way it grows: a
/// person copying files all afternoon would otherwise accumulate a list nobody
/// will ever scroll back through. A running job is never dropped — a queue that
/// forgets what it is copying is worse than one that grows.
fn trim(entries: &mut Vec<Entry>) {
    let mut excess = entries.len().saturating_sub(HISTORY_LIMIT);
    if excess == 0 {
        return;
    }
    entries.retain(|entry| {
        if excess > 0 && entry.job.state.is_finished() {
            excess -= 1;
            return false;
        }
        true
    });
}

/// Whether two paths can be renamed or copied between directly.
fn same_backend(a: &VfsPath, b: &VfsPath) -> bool {
    match (a, b) {
        (VfsPath::Local(_), VfsPath::Local(_)) => true,
        (VfsPath::Remote { mount: x, .. }, VfsPath::Remote { mount: y, .. }) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod queue_tests;

/// The seam the executor publishes through, so `execute` needs no knowledge of
/// the queue that owns it.
impl execute::ProgressSink for Queue {
    fn publish(&self, progress: JobProgress) {
        // The job this is for is the one the worker is holding, and the worker
        // only ever publishes for the job it claimed — so the id comes from the
        // queue's own single active entry rather than being threaded through
        // every call.
        if let Some(id) = self.running_id() {
            self.update(id, |job| job.progress = progress);
        }
    }
}
