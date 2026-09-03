//! The one place a [`SearchEvent`] reaches a consumer.
//!
//! Batching, the result cap, the warning cap and the "has this generation been
//! cancelled" question are the same for the live walker, the indexed engine
//! and the merge that joins them. They live here rather than three times over,
//! because a cap that is enforced in two of three producers is not a cap.

use std::sync::{Arc, atomic::AtomicBool};

use async_channel::Sender;
use ferret_vfs::VfsPath;

use crate::config::{MAX_WARNINGS, RESULT_BATCH_SIZE};
use crate::{
    SearchBatch, SearchEvent, SearchFinished, SearchGeneration, SearchLimits, SearchResult,
    SearchStats, SearchWarning,
};

pub(crate) struct EventSink {
    generation: SearchGeneration,
    events: Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
    pending: Vec<SearchResult>,
    limits: SearchLimits,
    warnings_sent: usize,
}

impl EventSink {
    pub(crate) fn new(
        generation: SearchGeneration,
        events: Sender<SearchEvent>,
        cancelled: Arc<AtomicBool>,
        limits: SearchLimits,
    ) -> Self {
        Self {
            generation,
            events,
            cancelled,
            pending: Vec::with_capacity(RESULT_BATCH_SIZE),
            limits,
            warnings_sent: 0,
        }
    }

    pub(crate) fn generation(&self) -> SearchGeneration {
        self.generation
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire) || self.events.is_closed()
    }

    /// Sends one event, or reports that nobody is listening any more.
    ///
    /// `false` always means "stop producing": either the session was cancelled
    /// or its receiver is gone. Callers must not treat it as a transient
    /// failure to retry.
    pub(crate) fn send(&self, event: SearchEvent) -> bool {
        !self.is_cancelled() && self.events.send_blocking(event).is_ok()
    }

    pub(crate) fn warning(&mut self, subject: VfsPath, message: impl Into<String>) -> bool {
        if self.warnings_sent >= MAX_WARNINGS {
            return !self.is_cancelled();
        }
        self.warnings_sent += 1;
        self.send(SearchEvent::Warning(SearchWarning {
            generation: self.generation,
            subject,
            message: message.into(),
        }))
    }

    pub(crate) fn push(&mut self, result: SearchResult, stats: &mut SearchStats) -> bool {
        let retained = stats.matches_sent as usize + self.pending.len();
        if retained >= self.limits.max_results {
            stats.truncated = true;
            return false;
        }
        self.pending.push(result);
        if self.pending.len() >= RESULT_BATCH_SIZE {
            self.flush(stats)
        } else {
            true
        }
    }

    pub(crate) fn flush(&mut self, stats: &mut SearchStats) -> bool {
        if self.pending.is_empty() {
            return !self.is_cancelled();
        }
        let results = std::mem::take(&mut self.pending);
        stats.matches_sent += results.len() as u64;
        self.send(SearchEvent::Batch(SearchBatch {
            generation: self.generation,
            results,
        }))
    }

    pub(crate) fn finish(&self, finished: SearchFinished) -> bool {
        self.send(SearchEvent::Finished(finished))
    }
}
