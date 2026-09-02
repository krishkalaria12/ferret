use ferret_vfs::{EntryKind, Metadata, VfsPath};
use smol_str::SmolStr;

use crate::config::DEFAULT_MAX_RESULTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SearchGeneration(pub u64);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchRequest {
    pub generation: SearchGeneration,
    pub scope: SearchScope,
    pub name: NameQuery,
    pub content: Option<ContentQuery>,
    pub filters: SearchFilters,
    pub limits: SearchLimits,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchScope {
    pub kind: SearchScopeKind,
    /// Explicit even for Computer: Windows has volumes, not one `/` root.
    pub roots: Vec<VfsPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchScopeKind {
    CurrentFolder,
    Home,
    Computer,
    Custom,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NameQuery {
    pub pattern: String,
    pub syntax: PatternSyntax,
    pub case: CaseMode,
    pub mode: NameMatchMode,
}

impl NameQuery {
    pub fn substring(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            syntax: PatternSyntax::Literal,
            case: CaseMode::Smart,
            mode: NameMatchMode::Substring,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PatternSyntax {
    Literal,
    Regex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CaseMode {
    Smart,
    Sensitive,
    Insensitive,
}

impl CaseMode {
    pub fn is_sensitive(self, pattern: &str) -> bool {
        match self {
            CaseMode::Smart => pattern.chars().any(char::is_uppercase),
            CaseMode::Sensitive => true,
            CaseMode::Insensitive => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NameMatchMode {
    Substring,
    Fuzzy,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ContentQuery {
    pub pattern: String,
    pub syntax: PatternSyntax,
    pub case: CaseMode,
    pub whole_word: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchFilters {
    pub kind: EntryKindFilter,
    pub extensions: Vec<SmolStr>,
    pub include_hidden: bool,
    pub respect_ignore: bool,
    pub follow_symlinks: bool,
    pub same_filesystem: bool,
    pub max_depth: Option<usize>,
}

impl Default for SearchFilters {
    fn default() -> Self {
        Self {
            kind: EntryKindFilter::All,
            extensions: Vec::new(),
            include_hidden: false,
            respect_ignore: true,
            follow_symlinks: false,
            same_filesystem: false,
            max_depth: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKindFilter {
    #[default]
    All,
    Files,
    Folders,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct SearchLimits {
    pub max_results: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchEvent {
    Started { generation: SearchGeneration },
    Batch(SearchBatch),
    Progress(SearchProgress),
    Warning(SearchWarning),
    Finished(SearchFinished),
}

impl SearchEvent {
    pub fn generation(&self) -> SearchGeneration {
        match self {
            SearchEvent::Started { generation }
            | SearchEvent::Warning(SearchWarning { generation, .. }) => *generation,
            SearchEvent::Batch(batch) => batch.generation,
            SearchEvent::Progress(progress) => progress.generation,
            SearchEvent::Finished(finished) => finished.generation(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchBatch {
    pub generation: SearchGeneration,
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchResult {
    pub id: SearchResultId,
    pub path: VfsPath,
    pub name: SmolStr,
    pub kind: EntryKind,
    pub metadata: Option<Metadata>,
    pub source: ResultSource,
    /// Zero-based position in the index's relevance ranking, or `None` for a
    /// live result.
    ///
    /// Deliberately an ordinal rather than the raw BM25 score: the score is
    /// only comparable within one query, it would make `SearchResult` carry a
    /// float and lose `Eq`, and the position is the whole of what a ranked
    /// list means to the person reading it.
    pub rank: Option<u32>,
    pub matched: SearchMatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SearchResultId {
    pub path: VfsPath,
    pub match_key: MatchKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MatchKey {
    Path,
    Content {
        line: u64,
        byte_start: u64,
        byte_end: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchMatch {
    Filename {
        ranges: Vec<TextRange>,
    },
    Content {
        line: u64,
        column: u64,
        preview: SmolStr,
        ranges: Vec<TextRange>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResultSource {
    Live,
    Index,
}

/// What the persistent index currently knows.
///
/// Two consumers, and both matter: the settings surface shows it to a person,
/// and the query planner reads [`IndexStatus::covers`] to decide whether a
/// scope can be answered from the index alone or needs a live scan merged in.
/// It is deliberately a snapshot — by the time it is read the crawler has
/// moved on, so it may only be used to choose a *faster* path, never to
/// suppress a correct one.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexStatus {
    /// `false` when the daemon is running but its stores could not be opened.
    pub available: bool,
    pub roots: Vec<IndexedRoot>,
    pub files: u64,
    pub content_documents: u64,
    pub bytes_on_disk: u64,
}

impl IndexStatus {
    /// Whether `path` lies inside a root that is crawled, reconciled and
    /// currently believed to agree with the filesystem.
    ///
    /// A root that is still crawling deliberately does not count: its rows are
    /// real, but its *absences* are not yet meaningful, and skipping the live
    /// scan on the strength of a partial index is how an indexed search comes
    /// to be quietly missing files.
    pub fn covers(&self, path: &VfsPath) -> bool {
        self.available
            && self
                .roots
                .iter()
                .any(|root| root.state == RootState::Ready && path.starts_with(&root.path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexedRoot {
    pub path: VfsPath,
    pub state: RootState,
    pub files: u64,
    /// Seconds since the Unix epoch, not a `SystemTime`: this crosses a
    /// process boundary and is shown as a relative age, so a fixed-width
    /// integer is the honest representation.
    pub last_reconciled_unix: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RootState {
    /// The first full crawl has not finished, so absences mean nothing yet.
    Crawling,
    /// Crawled and being kept fresh by the watcher and reconciliation.
    Ready,
    /// Kept but not refreshed, at the user's request.
    Paused,
    /// The store could not be read or written for this root; it is being
    /// rebuilt, and every query for it falls back to the live scan.
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchStats {
    pub roots_done: usize,
    pub roots_total: usize,
    pub entries_seen: u64,
    pub matches_sent: u64,
    pub skipped: u64,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchProgress {
    pub generation: SearchGeneration,
    pub stats: SearchStats,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchWarning {
    pub generation: SearchGeneration,
    pub subject: VfsPath,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SearchFinished {
    Complete {
        generation: SearchGeneration,
        stats: SearchStats,
    },
    Cancelled {
        generation: SearchGeneration,
        stats: SearchStats,
    },
    Failed {
        generation: SearchGeneration,
        message: String,
    },
}

impl SearchFinished {
    pub fn generation(&self) -> SearchGeneration {
        match self {
            SearchFinished::Complete { generation, .. }
            | SearchFinished::Cancelled { generation, .. }
            | SearchFinished::Failed { generation, .. } => *generation,
        }
    }
}
