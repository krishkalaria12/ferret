use std::sync::{Arc, atomic::AtomicBool};

use async_channel::Receiver;

use crate::{SearchEvent, SearchGeneration};

/// One running query and the only owner of its observable event stream.
///
/// The receiver stays private so cloning it cannot accidentally keep output
/// alive after the UI drops the session. Drop and explicit cancellation have
/// identical semantics.
pub struct SearchSession {
    generation: SearchGeneration,
    events: Receiver<SearchEvent>,
    cancelled: Arc<AtomicBool>,
}

impl SearchSession {
    pub(crate) fn new(
        generation: SearchGeneration,
        events: Receiver<SearchEvent>,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            generation,
            events,
            cancelled,
        }
    }

    pub fn generation(&self) -> SearchGeneration {
        self.generation
    }

    pub async fn next(&self) -> Option<SearchEvent> {
        self.events.recv().await.ok()
    }

    /// The same event stream for a caller that has no async runtime.
    ///
    /// The daemon is one: it forwards a session straight onto a socket from a
    /// thread of its own, and pulling in an executor to await a channel that
    /// already has a blocking receiver would be a runtime for nothing.
    pub fn next_blocking(&self) -> Option<SearchEvent> {
        self.events.recv_blocking().ok()
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.events.close();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for SearchSession {
    fn drop(&mut self) {
        self.cancel();
    }
}
