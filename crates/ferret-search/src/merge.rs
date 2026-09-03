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
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll, Wake, Waker};

use crate::config::EVENT_BUFFER;
use crate::index::{Index, IndexPlan, IndexSearchService};
use crate::live::{InProcessSearchService, validate_and_normalize};
use crate::sink::EventSink;
use crate::{
    Result, SearchError, SearchEvent, SearchFinished, SearchRequest, SearchResultId, SearchService,
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
    // Started before either engine: a compile error in the query has to be
    // reported as a failed start, not as an empty merged stream.
    let indexed = index.start(request.clone())?;
    let scanned = live.start(request.clone())?;

    merge_sessions(indexed, scanned, request)
}

fn merge_sessions(
    indexed: SearchSession,
    scanned: SearchSession,
    request: SearchRequest,
) -> Result<SearchSession> {
    let generation = request.generation;
    let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
    let cancelled = Arc::new(AtomicBool::new(false));
    let limits = request.limits;
    let session = SearchSession::new(generation, events_rx, Arc::clone(&cancelled));
    session.add_dependent(&indexed);
    session.add_dependent(&scanned);

    let task_cancelled = Arc::clone(&cancelled);
    std::thread::Builder::new()
        .name("ferret-search-merge".into())
        .spawn(move || {
            let sink = EventSink::new(generation, events_tx, task_cancelled, limits);
            run(sink, indexed, scanned);
        })
        .map_err(|error| {
            session.cancel();
            SearchError::DaemonTransport {
                message: format!("couldn't start the search merge coordinator: {error}"),
            }
        })?;
    Ok(session)
}

/// Shared between the two pumps: the one sink, the dedup set and the counters.
struct Merge {
    sink: EventSink,
    seen: HashSet<SearchResultId>,
    output_stats: SearchStats,
    source_stats: [SearchStats; 2],
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
    let mut merge = Merge {
        sink,
        seen: HashSet::new(),
        output_stats: SearchStats::default(),
        source_stats: [SearchStats::default(); 2],
        any_complete: false,
        failure: None,
    };

    // The coordinator is deliberately outside the bounded filesystem pool.
    // Both producers may block on their bounded channels; consuming either
    // receiver from the same pool could otherwise occupy the last worker and
    // prevent the other producer from ever making progress.
    let mut open = [true, true];
    let mut prefer_indexed = true;
    while open[0] || open[1] {
        if merge.sink.is_cancelled() {
            break;
        }
        let Some((source, event)) = next_from_either(
            open[0].then_some(&indexed),
            open[1].then_some(&scanned),
            prefer_indexed,
        ) else {
            break;
        };
        prefer_indexed = source != 0;
        let Some(event) = event else {
            open[source] = false;
            continue;
        };
        let terminal = event_kind_terminal(&event);
        if !pump_event(&mut merge, source, event) {
            indexed.cancel();
            scanned.cancel();
            break;
        }
        if terminal {
            open[source] = false;
        }
    }

    let mut stats = merge.combined_stats();
    let _ = merge.sink.flush(&mut stats);
    if merge.sink.is_cancelled() {
        let _ = merge
            .sink
            .finish(SearchFinished::Cancelled { generation, stats });
    } else if merge.any_complete || stats.truncated {
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

impl Merge {
    fn combined_stats(&self) -> SearchStats {
        SearchStats {
            roots_done: self.source_stats.iter().map(|stats| stats.roots_done).sum(),
            roots_total: self
                .source_stats
                .iter()
                .map(|stats| stats.roots_total)
                .sum(),
            entries_seen: self
                .source_stats
                .iter()
                .map(|stats| stats.entries_seen)
                .sum(),
            matches_sent: self.output_stats.matches_sent,
            skipped: self.source_stats.iter().map(|stats| stats.skipped).sum(),
            truncated: self.output_stats.truncated
                || self.source_stats.iter().any(|stats| stats.truncated),
        }
    }
}

fn pump_event(merge: &mut Merge, source: usize, event: SearchEvent) -> bool {
    if merge.sink.is_cancelled() {
        return false;
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
                let mut stats = merge.output_stats;
                let keep_going = merge.sink.push(result, &mut stats);
                merge.output_stats = stats;
                if !keep_going {
                    return false;
                }
            }
        }
        SearchEvent::Progress(progress) => {
            // Producer progress is cumulative, so replace its last
            // snapshot. Adding progress and final snapshots double-counts
            // every entry observed before the final update.
            merge.source_stats[source] = progress.stats;
        }
        SearchEvent::Warning(warning) => {
            if !merge.sink.warning(warning.subject, warning.message) {
                return false;
            }
        }
        SearchEvent::Finished(finished) => {
            match finished {
                SearchFinished::Complete { stats, .. } => {
                    merge.any_complete = true;
                    merge.source_stats[source] = stats;
                }
                // One engine failing is survivable and common — an index
                // being rebuilt, a root that vanished. The message is kept
                // in case the *other* engine also fails, which is the only
                // case the user needs to hear about.
                SearchFinished::Failed { message, .. } => merge.failure = Some(message),
                SearchFinished::Cancelled { .. } => {}
            }
        }
    }
    true
}

fn event_kind_terminal(event: &SearchEvent) -> bool {
    matches!(event, SearchEvent::Finished(_))
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Waits for either session without dedicating another worker to each stream.
fn next_from_either(
    indexed: Option<&SearchSession>,
    scanned: Option<&SearchSession>,
    prefer_indexed: bool,
) -> Option<(usize, Option<SearchEvent>)> {
    match (indexed, scanned) {
        (Some(indexed), None) => Some((0, indexed.next_blocking())),
        (None, Some(scanned)) => Some((1, scanned.next_blocking())),
        (None, None) => None,
        (Some(indexed), Some(scanned)) => {
            let mut indexed_next = pin!(indexed.next());
            let mut scanned_next = pin!(scanned.next());
            let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
            let mut context = Context::from_waker(&waker);
            loop {
                let first = if prefer_indexed { 0 } else { 1 };
                for source in [first, 1 - first] {
                    let polled = if source == 0 {
                        indexed_next.as_mut().poll(&mut context)
                    } else {
                        scanned_next.as_mut().poll(&mut context)
                    };
                    if let Poll::Ready(event) = polled {
                        return Some((source, event));
                    }
                }
                std::thread::park();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use ferret_vfs::VfsPath;

    use super::*;
    use crate::{
        NameQuery, SearchFilters, SearchGeneration, SearchLimits, SearchProgress, SearchScope,
        SearchScopeKind,
    };

    fn request() -> SearchRequest {
        SearchRequest {
            generation: SearchGeneration(9),
            scope: SearchScope {
                kind: SearchScopeKind::CurrentFolder,
                roots: vec![VfsPath::local("/tmp")],
            },
            name: NameQuery::substring("needle"),
            content: None,
            filters: SearchFilters::default(),
            limits: SearchLimits::default(),
        }
    }

    fn fake_session() -> (
        async_channel::Sender<SearchEvent>,
        SearchSession,
        Arc<AtomicBool>,
    ) {
        let (sender, receiver) = async_channel::bounded(EVENT_BUFFER);
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            sender,
            SearchSession::new(SearchGeneration(9), receiver, Arc::clone(&cancelled)),
            cancelled,
        )
    }

    #[test]
    fn cancelling_merge_immediately_cancels_both_children() {
        let (_indexed_tx, indexed, indexed_cancelled) = fake_session();
        let (_scanned_tx, scanned, scanned_cancelled) = fake_session();
        let merged = merge_sessions(indexed, scanned, request()).unwrap();

        merged.cancel();

        assert!(indexed_cancelled.load(Ordering::Acquire));
        assert!(scanned_cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn cumulative_progress_is_not_added_to_terminal_stats() {
        let (indexed_tx, indexed, _indexed_cancelled) = fake_session();
        let (scanned_tx, scanned, _scanned_cancelled) = fake_session();
        indexed_tx
            .send_blocking(SearchEvent::Progress(SearchProgress {
                generation: SearchGeneration(9),
                stats: SearchStats {
                    entries_seen: 4,
                    ..SearchStats::default()
                },
            }))
            .unwrap();
        indexed_tx
            .send_blocking(SearchEvent::Finished(SearchFinished::Complete {
                generation: SearchGeneration(9),
                stats: SearchStats {
                    entries_seen: 5,
                    ..SearchStats::default()
                },
            }))
            .unwrap();
        scanned_tx
            .send_blocking(SearchEvent::Finished(SearchFinished::Complete {
                generation: SearchGeneration(9),
                stats: SearchStats {
                    entries_seen: 7,
                    ..SearchStats::default()
                },
            }))
            .unwrap();

        let merged = merge_sessions(indexed, scanned, request()).unwrap();
        let terminal = loop {
            let event = merged.next_blocking().expect("merged event");
            if let SearchEvent::Finished(finished) = event {
                break finished;
            }
        };

        assert!(matches!(
            terminal,
            SearchFinished::Complete {
                stats: SearchStats {
                    entries_seen: 12,
                    ..
                },
                ..
            }
        ));
    }
}
