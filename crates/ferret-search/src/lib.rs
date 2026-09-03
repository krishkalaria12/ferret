//! Filename and content search for Ferret.
//!
//! This crate is deliberately GPUI-free. The UI consumes generation-tagged,
//! bounded event streams and does not know which engine produced them.
//!
//! Three engines implement one [`SearchService`] contract:
//!
//! | Engine | Answers | Lives |
//! | --- | --- | --- |
//! | [`InProcessSearchService`] | Anything, by walking the filesystem now | In Ferret, and in the daemon |
//! | [`IndexSearchService`] | Indexed roots, from SQLite and Tantivy | In the daemon only |
//! | [`HybridSearchService`] | Both, merged and deduplicated | In the daemon |
//!
//! and [`ipc::client::DaemonClient`] is a fourth that forwards to a supervised
//! `ferret-searchd` over a local socket. The index is a *performance layer*,
//! never the sole truth: an unavailable, stale, rebuilding or
//! protocol-mismatched daemon degrades to the live walk, which is why
//! [`InProcessSearchService`] has to stay a real, maintained implementation
//! rather than a stub.

pub mod config;
pub mod index;
pub mod ipc;

mod errors;
mod live;
mod merge;
mod service;
mod session;
mod sink;
mod types;

pub use errors::{Result, SearchError};
pub use index::{Index, IndexSearchService, Indexer};
pub use live::InProcessSearchService;
pub use merge::HybridSearchService;
pub use service::{FerretSearchService, default_index_roots, shared};
pub use session::SearchSession;
pub use types::{
    CaseMode, ContentQuery, EntryKindFilter, IndexStatus, IndexedRoot, MatchKey, NameMatchMode,
    NameQuery, PatternSyntax, ResultSource, RootState, SearchBatch, SearchEvent, SearchFilters,
    SearchFinished, SearchGeneration, SearchLimits, SearchMatch, SearchProgress, SearchRequest,
    SearchResult, SearchResultId, SearchScope, SearchScopeKind, SearchStats, SearchWarning,
    TextRange,
};

/// Starts an independently cancellable search session.
///
/// Implementations may be in-process or daemon-backed. Consumers must still
/// compare event generations, because cancellation and an already-sent batch
/// can race at a channel boundary.
pub trait SearchService: Send + Sync + 'static {
    fn start(&self, request: SearchRequest) -> Result<SearchSession>;
}

#[cfg(test)]
mod tests;
