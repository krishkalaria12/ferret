//! Filename and content search for Ferret.
//!
//! This crate is deliberately GPUI-free. The UI consumes generation-tagged,
//! bounded event streams and does not know whether a result came from the live
//! fallback implemented today or the persistent index/daemon added later.

pub mod config;

mod errors;
mod live;
mod session;
mod types;

pub use errors::{Result, SearchError};
pub use live::InProcessSearchService;
pub use session::SearchSession;
pub use types::{
    CaseMode, ContentQuery, EntryKindFilter, MatchKey, NameMatchMode, NameQuery, PatternSyntax,
    ResultSource, SearchBatch, SearchEvent, SearchFilters, SearchFinished, SearchGeneration,
    SearchLimits, SearchMatch, SearchProgress, SearchRequest, SearchResult, SearchResultId,
    SearchScope, SearchScopeKind, SearchStats, SearchWarning, TextRange,
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
