//! Ferret's per-user search index helper.
//!
//! It owns the persistent stores and nothing else: no window, no settings, no
//! privileges beyond the user who launched it. Ferret starts it on demand and
//! it exits on its own once nothing has connected for a while.
//!
//! Every failure here is designed to be survivable by the *client*, because
//! Ferret searches live when this process is absent. So a store that will not
//! open is not a reason to refuse to start — the daemon serves live results
//! and reports an unavailable index, which is strictly better than a Ferret
//! that relaunches a crashing helper on every keystroke.

use std::sync::Arc;

use anyhow::Context as _;
use ferret_search::ipc::server::{SearchHost, ServeOutcome, serve};
use ferret_search::{
    HybridSearchService, Index, IndexStatus, Indexer, SearchRequest, SearchService as _,
    SearchSession, default_index_roots, ipc,
};

fn main() -> anyhow::Result<()> {
    let dir = ipc::endpoint::index_dir().context("locating the search index directory")?;

    // Held for its `Drop`, which stops the indexer thread and lets it close
    // the SQLite writer cleanly. Bound to a name rather than `_`, which would
    // drop it immediately and leave the daemon serving an index nothing is
    // maintaining.
    let (index, _indexer) = match Index::open_or_rebuild(&dir) {
        Ok(index) => {
            let index = Arc::new(index);
            let indexer = Indexer::start(Arc::clone(&index), default_index_roots())
                .context("starting the indexer")?;
            (Some(index), Some(indexer))
        }
        Err(_) => (None, None),
    };

    let host = Arc::new(Daemon {
        service: HybridSearchService::new(index.clone()),
        index,
    });
    match serve(host).context("serving the search socket")? {
        // A duplicate launch is the normal outcome of two Ferret windows
        // racing to start the helper, not an error worth a non-zero exit.
        ServeOutcome::AlreadyRunning
        | ServeOutcome::IdleShutdown
        | ServeOutcome::ShutdownRequested => Ok(()),
    }
}

struct Daemon {
    service: HybridSearchService,
    index: Option<Arc<Index>>,
}

impl SearchHost for Daemon {
    fn start(&self, request: SearchRequest) -> ferret_search::Result<SearchSession> {
        self.service.start(request)
    }

    fn status(&self) -> IndexStatus {
        self.index
            .as_ref()
            .map(|index| index.status())
            .unwrap_or_default()
    }
}
