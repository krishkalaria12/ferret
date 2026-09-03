use std::sync::{Arc, Mutex, atomic::AtomicBool};

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
    dependents: Arc<Mutex<Vec<CancellationHandle>>>,
}

#[derive(Clone)]
pub(crate) struct CancellationHandle {
    cancelled: Arc<AtomicBool>,
    events: Receiver<SearchEvent>,
}

impl CancellationHandle {
    fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.events.close();
    }
}

#[derive(Clone)]
pub(crate) struct DependencyRegistrar {
    parent_cancelled: Arc<AtomicBool>,
    dependents: Arc<Mutex<Vec<CancellationHandle>>>,
}

impl DependencyRegistrar {
    pub(crate) fn add(&self, dependent: CancellationHandle) {
        let mut dependents = self
            .dependents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        dependents.push(dependent);
        if self
            .parent_cancelled
            .load(std::sync::atomic::Ordering::Acquire)
        {
            dependents.last().expect("just pushed").cancel();
        }
    }
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
            dependents: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn cancellation_handle(&self) -> CancellationHandle {
        CancellationHandle {
            cancelled: Arc::clone(&self.cancelled),
            events: self.events.clone(),
        }
    }

    pub(crate) fn dependency_registrar(&self) -> DependencyRegistrar {
        DependencyRegistrar {
            parent_cancelled: Arc::clone(&self.cancelled),
            dependents: Arc::clone(&self.dependents),
        }
    }

    pub(crate) fn add_dependent(&self, dependent: &SearchSession) {
        self.dependency_registrar()
            .add(dependent.cancellation_handle());
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
        let dependents = self
            .dependents
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for dependent in dependents.iter() {
            dependent.cancel();
        }
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
