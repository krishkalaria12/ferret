use ferret_vfs::{EntryKind, Metadata, VfsPath};
use smol_str::SmolStr;

use crate::config::DEFAULT_MAX_RESULTS;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SearchGeneration(pub u64);

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub generation: SearchGeneration,
    pub scope: SearchScope,
    pub name: NameQuery,
    pub content: Option<ContentQuery>,
    pub filters: SearchFilters,
    pub limits: SearchLimits,
}

#[derive(Clone, Debug)]
pub struct SearchScope {
    pub kind: SearchScopeKind,
    /// Explicit even for Computer: Windows has volumes, not one `/` root.
    pub roots: Vec<VfsPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScopeKind {
    CurrentFolder,
    Home,
    Computer,
    Custom,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternSyntax {
    Literal,
    Regex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameMatchMode {
    Substring,
    Fuzzy,
}

#[derive(Clone, Debug)]
pub struct ContentQuery {
    pub pattern: String,
    pub syntax: PatternSyntax,
    pub case: CaseMode,
    pub whole_word: bool,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryKindFilter {
    #[default]
    All,
    Files,
    Folders,
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchBatch {
    pub generation: SearchGeneration,
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub id: SearchResultId,
    pub path: VfsPath,
    pub name: SmolStr,
    pub kind: EntryKind,
    pub metadata: Option<Metadata>,
    pub source: ResultSource,
    pub matched: SearchMatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SearchResultId {
    pub path: VfsPath,
    pub match_key: MatchKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MatchKey {
    Path,
    Content {
        line: u64,
        byte_start: u64,
        byte_end: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultSource {
    Live,
    Index,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchStats {
    pub roots_done: usize,
    pub roots_total: usize,
    pub entries_seen: u64,
    pub matches_sent: u64,
    pub skipped: u64,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchProgress {
    pub generation: SearchGeneration,
    pub stats: SearchStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchWarning {
    pub generation: SearchGeneration,
    pub subject: VfsPath,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
