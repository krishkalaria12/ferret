//! What Ferret itself holds: one object that is always a working search.
//!
//! The UI must not have to know whether a daemon exists, whether it is a
//! compatible build, or whether its index has finished crawling. It calls
//! `start` and gets a session. Everything else — launching the helper, the
//! version handshake, giving up after repeated failures, dropping to a live
//! walk — happens behind this type, and the only thing that leaks out is
//! [`FerretSearchService::unavailable_reason`], which exists so the status
//! line can say *why* a search is slow rather than leaving it a mystery.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, atomic::AtomicBool};

use crate::config::EVENT_BUFFER;
use crate::ipc::client::DaemonClient;
use crate::sink::EventSink;
use crate::{
    InProcessSearchService, IndexStatus, Result, SearchError, SearchEvent, SearchFinished,
    SearchProgress, SearchRequest, SearchResultId, SearchService, SearchSession, SearchStats,
};

pub struct FerretSearchService {
    daemon: DaemonClient,
    fallback: InProcessSearchService,
}

impl Default for FerretSearchService {
    fn default() -> Self {
        Self::new()
    }
}

impl FerretSearchService {
    pub fn new() -> Self {
        Self {
            daemon: DaemonClient::new(),
            fallback: InProcessSearchService,
        }
    }

    /// What the daemon has indexed, or `None` when there is no usable daemon.
    pub fn index_status(&self) -> Option<IndexStatus> {
        self.daemon.status()
    }

    pub fn unavailable_reason(&self) -> Option<String> {
        self.daemon.unavailable_reason()
    }

    /// Stops the helper if one is running. Never starts one.
    ///
    /// For the index controls in settings — pause, rebuild, delete all mean
    /// stopping the process that holds the stores open first — and for the
    /// end-to-end test, which must not leave a daemon behind on its idle timer.
    pub fn stop_daemon(&self) -> bool {
        self.daemon.shutdown()
    }
}

impl SearchService for FerretSearchService {
    fn start(&self, request: SearchRequest) -> Result<SearchSession> {
        match self.daemon.start(request.clone()) {
            Ok(session) => resilient_session(session, request, |request| {
                InProcessSearchService.start(request)
            }),
            // Every daemon-side failure is a *latency* problem, never a
            // correctness one: the live walk answers the same question from
            // the filesystem. Propagating the error instead would turn a
            // missing helper binary into a search box that does nothing.
            Err(_) => self.fallback.start(request),
        }
    }
}

fn resilient_session<F>(
    primary: SearchSession,
    request: SearchRequest,
    fallback_start: F,
) -> Result<SearchSession>
where
    F: FnOnce(SearchRequest) -> Result<SearchSession> + Send + 'static,
{
    let generation = request.generation;
    let limits = request.limits;
    let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
    let cancelled = Arc::new(AtomicBool::new(false));
    let session = SearchSession::new(generation, events_rx, Arc::clone(&cancelled));
    session.add_dependent(&primary);
    let registrar = session.dependency_registrar();

    std::thread::Builder::new()
        .name("ferret-search-fallback".into())
        .spawn(move || {
            let sink = EventSink::new(generation, events_tx, cancelled, limits);
            run_resilient(sink, primary, request, fallback_start, registrar);
        })
        .map_err(|error| {
            session.cancel();
            SearchError::DaemonTransport {
                message: format!("couldn't start the search fallback coordinator: {error}"),
            }
        })?;
    Ok(session)
}

fn run_resilient<F>(
    mut sink: EventSink,
    primary: SearchSession,
    request: SearchRequest,
    fallback_start: F,
    registrar: crate::session::DependencyRegistrar,
) where
    F: FnOnce(SearchRequest) -> Result<SearchSession>,
{
    let generation = sink.generation();
    if !sink.send(SearchEvent::Started { generation }) {
        return;
    }
    let mut seen = std::collections::HashSet::<SearchResultId>::new();
    let mut output_stats = SearchStats::default();
    let mut primary_stats = SearchStats::default();

    let primary_failure = loop {
        let Some(event) = primary.next_blocking() else {
            if sink.is_cancelled() {
                return;
            }
            break "the search index helper closed its result stream unexpectedly".to_string();
        };
        match event {
            SearchEvent::Started { .. } => {}
            SearchEvent::Batch(batch) => {
                if !forward_batch(&mut sink, &mut seen, &mut output_stats, batch.results) {
                    primary.cancel();
                    finish_complete(&mut sink, &mut output_stats, primary_stats);
                    return;
                }
            }
            SearchEvent::Progress(progress) => {
                primary_stats = progress.stats;
                send_progress(&sink, output_stats, primary_stats);
            }
            SearchEvent::Warning(warning) => {
                if !sink.warning(warning.subject, warning.message) {
                    return;
                }
            }
            SearchEvent::Finished(SearchFinished::Complete { stats, .. }) => {
                finish_complete(&mut sink, &mut output_stats, stats);
                return;
            }
            SearchEvent::Finished(SearchFinished::Cancelled { stats, .. }) => {
                let mut stats = combined_fallback_stats(output_stats, primary_stats, stats);
                let _ = sink.flush(&mut stats);
                let _ = sink.finish(SearchFinished::Cancelled { generation, stats });
                return;
            }
            SearchEvent::Finished(SearchFinished::Failed { message, .. }) => break message,
        }
    };

    let warning_subject = request
        .scope
        .roots
        .first()
        .cloned()
        .expect("validated searches have at least one root");
    if !sink.warning(
        warning_subject,
        format!("The search index stopped responding; Ferret continued with a live scan. {primary_failure}"),
    ) {
        return;
    }

    let fallback = match fallback_start(request) {
        Ok(fallback) => fallback,
        Err(error) => {
            let _ = sink.finish(SearchFinished::Failed {
                generation,
                message: format!(
                    "the search index failed ({primary_failure}), and the live fallback could not start: {error}"
                ),
            });
            return;
        }
    };
    registrar.add(fallback.cancellation_handle());
    while let Some(event) = fallback.next_blocking() {
        match event {
            SearchEvent::Started { .. } => {}
            SearchEvent::Batch(batch) => {
                if !forward_batch(&mut sink, &mut seen, &mut output_stats, batch.results) {
                    fallback.cancel();
                    finish_complete(
                        &mut sink,
                        &mut output_stats,
                        combined_fallback_stats(
                            SearchStats::default(),
                            primary_stats,
                            SearchStats::default(),
                        ),
                    );
                    return;
                }
            }
            SearchEvent::Progress(progress) => {
                send_progress(
                    &sink,
                    output_stats,
                    combined_fallback_stats(SearchStats::default(), primary_stats, progress.stats),
                );
            }
            SearchEvent::Warning(warning) => {
                if !sink.warning(warning.subject, warning.message) {
                    return;
                }
            }
            SearchEvent::Finished(SearchFinished::Complete { stats, .. }) => {
                let work_stats =
                    combined_fallback_stats(SearchStats::default(), primary_stats, stats);
                finish_complete(&mut sink, &mut output_stats, work_stats);
                return;
            }
            SearchEvent::Finished(SearchFinished::Cancelled { stats, .. }) => {
                let mut stats = combined_fallback_stats(output_stats, primary_stats, stats);
                let _ = sink.flush(&mut stats);
                let _ = sink.finish(SearchFinished::Cancelled { generation, stats });
                return;
            }
            SearchEvent::Finished(SearchFinished::Failed { message, .. }) => {
                let _ = sink.finish(SearchFinished::Failed {
                    generation,
                    message: format!(
                        "the search index failed ({primary_failure}), and the live fallback also failed: {message}"
                    ),
                });
                return;
            }
        }
    }
    if !sink.is_cancelled() {
        let _ = sink.finish(SearchFinished::Failed {
            generation,
            message: format!(
                "the search index failed ({primary_failure}), and the live fallback closed its result stream unexpectedly"
            ),
        });
    }
}

fn forward_batch(
    sink: &mut EventSink,
    seen: &mut std::collections::HashSet<SearchResultId>,
    stats: &mut SearchStats,
    results: Vec<crate::SearchResult>,
) -> bool {
    for result in results {
        if seen.insert(result.id.clone()) && !sink.push(result, stats) {
            return false;
        }
    }
    true
}

fn send_progress(sink: &EventSink, output: SearchStats, mut work: SearchStats) {
    work.matches_sent = output.matches_sent;
    work.truncated |= output.truncated;
    let _ = sink.send(SearchEvent::Progress(SearchProgress {
        generation: sink.generation(),
        stats: work,
    }));
}

fn finish_complete(sink: &mut EventSink, output: &mut SearchStats, mut work: SearchStats) {
    let _ = sink.flush(output);
    work.matches_sent = output.matches_sent;
    work.truncated |= output.truncated;
    let _ = sink.finish(SearchFinished::Complete {
        generation: sink.generation(),
        stats: work,
    });
}

fn combined_fallback_stats(
    output: SearchStats,
    primary: SearchStats,
    fallback: SearchStats,
) -> SearchStats {
    SearchStats {
        // These describe logical scope completion, so the successful live
        // retry replaces rather than adds to the failed daemon attempt.
        roots_done: fallback.roots_done,
        roots_total: fallback.roots_total.max(primary.roots_total),
        entries_seen: primary.entries_seen + fallback.entries_seen,
        matches_sent: output.matches_sent,
        skipped: primary.skipped + fallback.skipped,
        truncated: output.truncated || primary.truncated || fallback.truncated,
    }
}

/// Ferret's one search service, built on first use.
///
/// Shared rather than constructed per query because the supervision state is
/// the point: the launch-failure count that stops Ferret relaunching a broken
/// helper on every keystroke only means anything if every query consults the
/// same counter. A per-query service would forget, and try again forever.
pub fn shared() -> &'static FerretSearchService {
    static SERVICE: OnceLock<FerretSearchService> = OnceLock::new();
    SERVICE.get_or_init(FerretSearchService::new)
}

/// The roots indexed unless the user says otherwise.
///
/// Home only (PS-Q04). Not the system root: it is mostly files nobody
/// searches for, it is where Full Disk Access denials cluster, and indexing it
/// would multiply the store for no recall a person would notice. Not removable
/// or network volumes either — an index of a disk that is usually absent is
/// stale by construction.
pub fn default_index_roots() -> Vec<PathBuf> {
    if let Some(roots) = std::env::var_os(ROOTS_OVERRIDE) {
        return std::env::split_paths(&roots)
            .filter(|root| !root.as_os_str().is_empty())
            .collect();
    }
    dirs::home_dir().into_iter().collect()
}

/// Platform-separated roots to index instead of the default.
///
/// For development and for the end-to-end test: crawling the developer's whole
/// home directory to prove the socket works would take minutes and leave an
/// index nobody asked for.
const ROOTS_OVERRIDE: &str = "FERRET_SEARCH_ROOTS";

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use ferret_vfs::{EntryKind, VfsPath};
    use smol_str::SmolStr;

    use super::*;
    use crate::{
        MatchKey, NameQuery, ResultSource, SearchBatch, SearchFilters, SearchGeneration,
        SearchLimits, SearchMatch, SearchResult, SearchScope, SearchScopeKind,
    };

    fn request() -> SearchRequest {
        SearchRequest {
            generation: SearchGeneration(11),
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
            SearchSession::new(SearchGeneration(11), receiver, Arc::clone(&cancelled)),
            cancelled,
        )
    }

    fn result(path: &str, source: ResultSource) -> SearchResult {
        let path = VfsPath::local(path);
        SearchResult {
            id: SearchResultId {
                path: path.clone(),
                match_key: MatchKey::Path,
            },
            path,
            name: SmolStr::new("needle.txt"),
            kind: EntryKind::File,
            metadata: None,
            source,
            rank: None,
            matched: SearchMatch::Filename { ranges: Vec::new() },
        }
    }

    #[test]
    fn terminal_daemon_failure_falls_back_and_deduplicates_results() {
        let (primary_tx, primary, _primary_cancelled) = fake_session();
        let session = resilient_session(primary, request(), move |_request| {
            let (fallback_tx, fallback, _cancelled) = fake_session();
            fallback_tx
                .send_blocking(SearchEvent::Batch(SearchBatch {
                    generation: SearchGeneration(11),
                    results: vec![
                        result("/tmp/needle.txt", ResultSource::Live),
                        result("/tmp/needle-2.txt", ResultSource::Live),
                    ],
                }))
                .unwrap();
            fallback_tx
                .send_blocking(SearchEvent::Finished(SearchFinished::Complete {
                    generation: SearchGeneration(11),
                    stats: SearchStats {
                        roots_done: 1,
                        roots_total: 1,
                        entries_seen: 5,
                        matches_sent: 2,
                        ..SearchStats::default()
                    },
                }))
                .unwrap();
            Ok(fallback)
        })
        .unwrap();

        primary_tx
            .send_blocking(SearchEvent::Batch(SearchBatch {
                generation: SearchGeneration(11),
                results: vec![result("/tmp/needle.txt", ResultSource::Index)],
            }))
            .unwrap();
        primary_tx
            .send_blocking(SearchEvent::Progress(SearchProgress {
                generation: SearchGeneration(11),
                stats: SearchStats {
                    roots_total: 1,
                    entries_seen: 3,
                    matches_sent: 1,
                    ..SearchStats::default()
                },
            }))
            .unwrap();
        primary_tx
            .send_blocking(SearchEvent::Finished(SearchFinished::Failed {
                generation: SearchGeneration(11),
                message: "socket closed".into(),
            }))
            .unwrap();

        let mut found = Vec::new();
        let mut warning = false;
        let terminal = loop {
            match session.next_blocking().expect("resilient event") {
                SearchEvent::Batch(batch) => found.extend(batch.results),
                SearchEvent::Warning(_) => warning = true,
                SearchEvent::Finished(finished) => break finished,
                _ => {}
            }
        };

        assert!(warning);
        assert_eq!(found.len(), 2);
        assert!(matches!(
            terminal,
            SearchFinished::Complete {
                stats: SearchStats {
                    entries_seen: 8,
                    matches_sent: 2,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn cancellation_reaches_a_fallback_started_after_parent_creation() {
        let (primary_tx, primary, _primary_cancelled) = fake_session();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let session = resilient_session(primary, request(), move |_request| {
            let (_events_tx, fallback, cancelled) = fake_session();
            started_tx.send(Arc::clone(&cancelled)).unwrap();
            Ok(fallback)
        })
        .unwrap();
        primary_tx
            .send_blocking(SearchEvent::Finished(SearchFinished::Failed {
                generation: SearchGeneration(11),
                message: "late failure".into(),
            }))
            .unwrap();
        let fallback_cancelled = started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("fallback started");

        session.cancel();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !fallback_cancelled.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(fallback_cancelled.load(Ordering::Acquire));
    }
}
