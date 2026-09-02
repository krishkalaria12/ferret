//! The planner: which engines run, and how their two streams become one.
//!
//! The interesting decision is not *how* to merge but *whether to merge at
//! all*. Running both engines always would throw away the entire latency win —
//! a live walk of a home directory costs seconds whatever the index says. So
//! [`IndexSearchService::plan`] is asked first, and only a scope the index
//! cannot fully answer pays for a second engine.
//!
//! When both do run, deduplication rests on one property that the rest of this
//! crate is arranged to preserve: **an indexed row and a live row for the same
//! match are byte-identical in their [`SearchResultId`]**. Filename matches key
//! on the path alone. Content matches key on the path plus the line and byte
//! offsets — and the indexed path produces those by re-reading the file with
//! the same `grep-searcher` configuration the live path uses, precisely so the
//! two agree. A snippet reconstructed from a stored copy of the body would not,
//! and the same match would appear twice.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::config::EVENT_BUFFER;
use crate::index::{Index, IndexPlan, IndexSearchService};
use crate::live::{InProcessSearchService, validate_and_normalize};
use crate::sink::EventSink;
use crate::{
    Result, SearchEvent, SearchFinished, SearchRequest, SearchResultId, SearchService,
    SearchSession, SearchStats,
};

/// The service the daemon hosts: indexed first, live where the index cannot
/// answer, one deduplicated stream out.
pub struct HybridSearchService {
    index: Option<IndexSearchService>,
    live: InProcessSearchService,
}

impl HybridSearchService {
    /// Builds a planner over an optional index.
    ///
    /// `None` is not a degraded mode to apologise for — it is the shape this
    /// service takes in-process inside Ferret when no daemon is available, and
    /// it must produce exactly the same results, only slower.
    pub fn new(index: Option<Arc<Index>>) -> Self {
        Self {
            index: index.map(IndexSearchService::new),
            live: InProcessSearchService,
        }
    }
}

impl SearchService for HybridSearchService {
    fn start(&self, mut request: SearchRequest) -> Result<SearchSession> {
        validate_and_normalize(&mut request)?;
        let Some(index) = &self.index else {
            return self.live.start(request);
        };
        match index.plan(&request) {
            // Nothing the index holds can help: hidden or ignored entries were
            // asked for, or the scope is outside every indexed root.
            IndexPlan::None => self.live.start(request),
            // The index owns this scope outright. Skipping the walk here is
            // the whole feature; freshness rests on the watcher, on
            // reconciliation, and on every row being re-stat-ed before it is
            // sent.
            IndexPlan::Covered => index.start(request),
            IndexPlan::Partial => merged(index, &self.live, request),
        }
    }
}

fn merged(
    index: &IndexSearchService,
    live: &InProcessSearchService,
    request: SearchRequest,
) -> Result<SearchSession> {
    let generation = request.generation;
    // Started before either engine: a compile error in the query has to be
    // reported as a failed start, not as an empty merged stream.
    let indexed = index.start(request.clone())?;
    let scanned = live.start(request.clone())?;

    let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
    let cancelled = Arc::new(AtomicBool::new(false));
    let task_cancelled = Arc::clone(&cancelled);
    let limits = request.limits;

    let pool = ferret_vfs::pool::pool().expect("Ferret's worker pool is always available");
    pool.spawn(move || {
        let sink = EventSink::new(generation, events_tx, task_cancelled, limits);
        run(sink, indexed, scanned);
    });
    Ok(SearchSession::new(generation, events_rx, cancelled))
}

/// Shared between the two pumps: the one sink, the dedup set and the counters.
struct Merge {
    sink: EventSink,
    seen: HashSet<SearchResultId>,
    stats: SearchStats,
    /// Whether either engine got as far as completing. One of them failing is
    /// survivable — an index being rebuilt, a root that vanished — and the
    /// merged stream only reports a failure when neither succeeded.
    any_complete: bool,
    failure: Option<String>,
}

fn run(sink: EventSink, indexed: SearchSession, scanned: SearchSession) {
    let generation = sink.generation();
    if !sink.send(SearchEvent::Started { generation }) {
        return;
    }
    let merge = std::sync::Mutex::new(Merge {
        sink,
        seen: HashSet::new(),
        stats: SearchStats::default(),
        any_complete: false,
        failure: None,
    });

    // Both engines run at once. The index answers in milliseconds and the walk
    // in seconds, so the visible effect is that indexed rows land immediately
    // and the walk fills in what the index did not have — rather than the user
    // waiting for the slower of the two before seeing anything.
    rayon::join(|| pump(&merge, &indexed), || pump(&merge, &scanned));

    let mut merge = merge
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    let mut stats = merge.stats;
    let _ = merge.sink.flush(&mut stats);
    if merge.sink.is_cancelled() {
        let _ = merge
            .sink
            .finish(SearchFinished::Cancelled { generation, stats });
    } else if merge.any_complete {
        let _ = merge
            .sink
            .finish(SearchFinished::Complete { generation, stats });
    } else {
        let _ = merge.sink.finish(SearchFinished::Failed {
            generation,
            message: merge
                .failure
                .unwrap_or_else(|| "the search stopped before producing a result".into()),
        });
    }
}

fn pump(merge: &std::sync::Mutex<Merge>, session: &SearchSession) {
    while let Some(event) = session.next_blocking() {
        let mut merge = merge.lock().unwrap_or_else(|error| error.into_inner());
        if merge.sink.is_cancelled() {
            return;
        }
        match event {
            // Both sub-engines announce themselves and both finish; the merged
            // stream has exactly one of each, emitted by `run`.
            SearchEvent::Started { .. } => {}
            SearchEvent::Batch(batch) => {
                for result in batch.results {
                    if !merge.seen.insert(result.id.clone()) {
                        continue;
                    }
                    let mut stats = merge.stats;
                    let keep_going = merge.sink.push(result, &mut stats);
                    merge.stats = stats;
                    if !keep_going {
                        return;
                    }
                }
            }
            SearchEvent::Progress(progress) => {
                // Entry counts add up across engines; `matches_sent` does not,
                // because the merge is the only thing that knows how many
                // survived deduplication and the sink already maintains it.
                merge.stats.entries_seen += progress.stats.entries_seen;
                merge.stats.skipped += progress.stats.skipped;
                merge.stats.truncated |= progress.stats.truncated;
            }
            SearchEvent::Warning(warning) => {
                if !merge.sink.warning(warning.subject, warning.message) {
                    return;
                }
            }
            SearchEvent::Finished(finished) => {
                match finished {
                    SearchFinished::Complete { stats, .. } => {
                        merge.any_complete = true;
                        merge.stats.entries_seen += stats.entries_seen;
                        merge.stats.skipped += stats.skipped;
                        merge.stats.truncated |= stats.truncated;
                    }
                    // One engine failing is survivable and common — an index
                    // being rebuilt, a root that vanished. The message is kept
                    // in case the *other* engine also fails, which is the only
                    // case the user needs to hear about.
                    SearchFinished::Failed { message, .. } => merge.failure = Some(message),
                    SearchFinished::Cancelled { .. } => {}
                }
                return;
            }
        }
    }
}
