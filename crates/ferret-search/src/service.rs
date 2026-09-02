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
use std::sync::OnceLock;

use crate::ipc::client::DaemonClient;
use crate::{
    InProcessSearchService, IndexStatus, Result, SearchRequest, SearchService, SearchSession,
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
            Ok(session) => Ok(session),
            // Every daemon-side failure is a *latency* problem, never a
            // correctness one: the live walk answers the same question from
            // the filesystem. Propagating the error instead would turn a
            // missing helper binary into a search box that does nothing.
            Err(_) => self.fallback.start(request),
        }
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
