//! The one thread pool Ferret owns for filesystem work.
//!
//! Three pools already want to exist in this process: smol's blocking pool
//! (behind every `Vfs` method), GPUI's background executor, and rayon's
//! implicit global pool. Each sizes itself to the core count independently, so
//! letting a fourth appear — or letting rayon's global one be used by accident
//! — oversubscribes the machine and makes every measurement noise (`docs/audit.html`
//! §7).
//!
//! So there is exactly one, built once, and everything data-parallel runs on
//! it: resolving metadata during a listing, sorting a listing on a header
//! click, and the tree copy and delete in [`crate::local`]. Never build one per
//! listing or per operation.

use crate::config::worker_threads;
use std::sync::{Arc, OnceLock};

static POOL: OnceLock<Arc<rayon::ThreadPool>> = OnceLock::new();

/// Ferret's pool, built on first use.
///
/// Falls back to rayon's global pool only if construction fails, which it does
/// not in practice — a degraded pool is better than refusing to list a
/// directory.
pub fn pool() -> Option<&'static Arc<rayon::ThreadPool>> {
    POOL.get_or_init(|| {
        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(worker_threads())
                .thread_name(|i| format!("ferret-worker-{i}"))
                .build()
                .unwrap_or_else(|_| {
                    // Named so it is obvious in a profiler which one this is.
                    rayon::ThreadPoolBuilder::new()
                        .build()
                        .expect("a default rayon pool always builds")
                }),
        )
    });
    POOL.get()
}

/// Run `f` on Ferret's pool.
///
/// Everything data-parallel goes through here rather than calling a `par_*`
/// method directly, because a bare `par_iter` runs on rayon's *global* pool —
/// which is the second pool this module exists to prevent.
pub fn install<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    match pool() {
        Some(pool) => pool.install(f),
        None => f(),
    }
}

/// How jwalk should parallelise, on Ferret's pool rather than rayon's global
/// one.
///
/// `busy_timeout` is left at jwalk's default rather than `None`: the pool is
/// shared with the sort and with metadata resolution, so "there is at least
/// one free thread" is exactly the assumption that does not hold here, and the
/// deadlock check is what turns that into a slow walk instead of a hang.
pub fn parallelism() -> jwalk::Parallelism {
    match pool() {
        Some(pool) => jwalk::Parallelism::RayonExistingPool {
            pool: Arc::clone(pool),
            busy_timeout: None,
        },
        None => jwalk::Parallelism::RayonDefaultPool {
            busy_timeout: std::time::Duration::from_secs(1),
        },
    }
}
