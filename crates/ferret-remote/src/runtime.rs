//! The bridge between Ferret's executors and the one `russh` needs (R-D03).
//!
//! `russh` is built on Tokio; Ferret runs GPUI's executors and smol. Rather
//! than let that boundary happen by accident, it lives here and obeys four
//! rules:
//!
//! 1. **One runtime**, created on the first connection and never before, so a
//!    user who never opens a remote folder pays nothing.
//! 2. **Two worker threads, fixed.** This work waits on a socket. Sizing it
//!    from the core count would put a fourth machine-sized pool beside
//!    `ferret_vfs::pool`, smol's blocking pool and GPUI's executor.
//! 3. **Nothing crosses in the other direction.** Session objects live inside
//!    the runtime; the rest of Ferret holds a handle that sends requests.
//! 4. **No `block_on`, anywhere.** That is what would freeze a frame. Callers
//!    `await` a channel; the work happens on the runtime.

use crate::config::RUNTIME_THREADS;
use crate::{RemoteError, Result};
use parking_lot::Mutex;
use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

/// The shared runtime, built on first use.
fn runtime_handle() -> tokio::runtime::Handle {
    let slot = RUNTIME.get_or_init(|| Mutex::new(None));
    let mut runtime = slot.lock();
    let runtime = runtime.get_or_insert_with(|| {
        Builder::new_multi_thread()
            .worker_threads(RUNTIME_THREADS)
            .thread_name("ferret-remote")
            // I/O and timers only: no CPU work is ever scheduled here, so
            // nothing else needs enabling.
            .enable_io()
            .enable_time()
            .build()
            .expect("the remote runtime is the only thing that can start a connection")
    });
    runtime.handle().clone()
}

/// Run `future` on the remote runtime and await its result from whichever
/// executor called — GPUI's, smol's, or a test's.
///
/// The `oneshot` is the whole bridge. The caller parks on a channel receive,
/// which every executor can do; the work happens on Tokio, which is what
/// `russh` requires.
pub async fn spawn<F, T>(future: F) -> Result<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut task = AbortOnDrop(Some(runtime_handle().spawn(future)));
    let result = task
        .0
        .as_mut()
        .expect("task is present until its result is taken")
        .await
        .map_err(|_| RemoteError::Cancelled)?;
    task.0 = None;
    Ok(result)
}

struct AbortOnDrop<T>(Option<tokio::task::JoinHandle<T>>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Spawn work that reports through its own channel rather than a return value.
///
/// Used by streaming operations: a listing sends batches as they arrive and
/// ends when the receiver is dropped, which is how cancel-by-drop reaches
/// across the runtime boundary.
pub fn detach<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    runtime_handle().spawn(future);
}

/// Whether the runtime has been started yet. Startup cost is only paid on the
/// first connection, and this is how a test proves it.
pub fn is_started() -> bool {
    RUNTIME.get().is_some_and(|slot| slot.lock().is_some())
}

/// Stop the remote runtime after all sessions have been dropped.
///
/// This is a process-lifecycle operation, not something a view calls. Ferret's
/// quit hook and integration tests use it after closing sessions; a later
/// connection can start a fresh runtime.
pub fn shutdown() {
    let Some(slot) = RUNTIME.get() else { return };
    if let Some(runtime) = slot.lock().take() {
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn work_crosses_from_smol_to_tokio_and_back() {
        // The shape every `Vfs` method uses: a smol executor awaiting a result
        // computed on Tokio, with no `block_on` in between.
        let answer = smol::block_on(async {
            spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                40 + 2
            })
            .await
        });
        assert_eq!(answer.unwrap(), 42);
        assert!(is_started());
    }

    #[test]
    fn a_dropped_caller_is_a_cancellation_not_a_failure() {
        let (tx, rx) = futures::channel::oneshot::channel::<()>();
        detach(async move {
            // Nobody is listening for this; it must not panic.
            drop(rx);
            let _ = tx;
        });
    }

    #[test]
    fn dropping_the_caller_aborts_remote_work() {
        struct MarksDrop(Arc<AtomicBool>);
        impl Drop for MarksDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        smol::block_on(async {
            let marker = dropped.clone();
            let caller = smol::spawn(async move {
                let _ = spawn(async move {
                    let _guard = MarksDrop(marker);
                    futures::future::pending::<()>().await;
                })
                .await;
            });
            smol::Timer::after(std::time::Duration::from_millis(10)).await;
            caller.cancel().await;
            smol::Timer::after(std::time::Duration::from_millis(10)).await;
        });
        assert!(dropped.load(Ordering::Acquire));
    }
}
