//! Answering a query from the stores instead of from the disk.
//!
//! Two things this engine is careful *not* to do. It never answers a query
//! whose filters disagree with the policy the crawl ran under — a corpus that
//! was never allowed to contain hidden or ignored files cannot be asked
//! whether it has any. And it never trusts a stored row about a file's
//! existence: every row that reaches the UI has been re-stat-ed, so a result
//! the user clicks is one the filesystem still had a moment ago.
//!
//! What it does trust the index for is *candidacy* — which few thousand of
//! several million paths are worth looking at. That is the entire performance
//! claim, and it is the only claim that survives the index being slightly out
//! of date.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use ferret_vfs::{EntryKind, Metadata, VfsPath};
use grep_matcher::{Match, Matcher as _};
use grep_searcher::{Searcher, Sink, SinkMatch};
use smol_str::SmolStr;

use super::db::{Db, FileRow};
use super::{Index, policy_matches};
use crate::config::{
    CONTENT_CANDIDATE_PAGE_SIZE, EVENT_BUFFER, INDEX_CANDIDATE_PAGE_SIZE, MAX_RANGES_PER_PREVIEW,
};
use crate::live::{
    ContentMatcher, NameMatcher, build_content_searcher, display_column, extension_matches,
    kind_matches, preview, strip_line_ending, validate_and_normalize,
};
use crate::sink::EventSink;
use crate::{
    MatchKey, PatternSyntax, Result, ResultSource, SearchEvent, SearchFinished, SearchMatch,
    SearchProgress, SearchRequest, SearchResult, SearchResultId, SearchService, SearchSession,
    SearchStats,
};

/// What the index can contribute to one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexPlan {
    /// The index covers the request's filters and every scope root listed.
    /// The caller may still merge a live scan in for freshness, but it is
    /// allowed to skip it.
    Covered,
    /// The index can contribute rows but not all of them — an uncovered root,
    /// a still-crawling one, or a content query it cannot express. A live scan
    /// must run alongside.
    Partial,
    /// Nothing here can help; use the live scan alone.
    None,
}

#[derive(Clone)]
pub struct IndexSearchService {
    index: Arc<Index>,
}

impl IndexSearchService {
    pub fn new(index: Arc<Index>) -> Self {
        Self { index }
    }

    /// Decides, before any work is done, how much of a request the index owns.
    ///
    /// Called by [`crate::HybridSearchService`] rather than being folded into
    /// `start`, because the answer determines whether a *second* engine has to
    /// run — and that decision has to be made once, up front, not discovered
    /// halfway through a result stream.
    pub fn plan(&self, request: &SearchRequest) -> IndexPlan {
        if !policy_matches(&request.filters) {
            return IndexPlan::None;
        }
        let status = self.index.status();
        if !status.available {
            return IndexPlan::None;
        }
        let content_covered = match &request.content {
            // Regex and whole-word are matcher semantics a tokenized index
            // cannot express. The index can still narrow by *name*, so this is
            // partial rather than nothing.
            Some(query) => {
                query.syntax == PatternSyntax::Literal
                    && !query.whole_word
                    && self.index.content().supports_literal(&query.pattern)
            }
            None => true,
        };
        let all_roots_ready = request.scope.roots.iter().all(|root| status.covers(root));
        match (all_roots_ready, content_covered) {
            (true, true) => IndexPlan::Covered,
            (false, _) if !any_root_indexed(&status, &request.scope.roots) => IndexPlan::None,
            _ => IndexPlan::Partial,
        }
    }
}

fn any_root_indexed(status: &crate::IndexStatus, roots: &[VfsPath]) -> bool {
    roots.iter().any(|root| {
        status
            .roots
            .iter()
            .any(|indexed| root.starts_with(&indexed.path) || indexed.path.starts_with(root))
    })
}

impl SearchService for IndexSearchService {
    fn start(&self, mut request: SearchRequest) -> Result<SearchSession> {
        validate_and_normalize(&mut request)?;
        let name_matcher = if request.name.pattern.trim().is_empty() {
            None
        } else {
            Some(NameMatcher::compile(&request)?)
        };
        let content_matcher = request
            .content
            .as_ref()
            .map(ContentMatcher::compile)
            .transpose()?;
        let generation = request.generation;
        let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
        let cancelled = Arc::new(AtomicBool::new(false));
        let index = Arc::clone(&self.index);
        let task_cancelled = Arc::clone(&cancelled);

        let pool = ferret_vfs::pool::pool().expect("Ferret's worker pool is always available");
        pool.spawn(move || {
            let sink = EventSink::new(generation, events_tx, task_cancelled, request.limits);
            run(&index, request, name_matcher, content_matcher, sink);
        });
        Ok(SearchSession::new(generation, events_rx, cancelled))
    }
}

fn run(
    index: &Index,
    request: SearchRequest,
    name_matcher: Option<NameMatcher>,
    content_matcher: Option<ContentMatcher>,
    mut sink: EventSink,
) {
    let generation = sink.generation();
    if !sink.send(SearchEvent::Started { generation }) {
        return;
    }
    let mut stats = SearchStats {
        roots_total: request.scope.roots.len(),
        ..SearchStats::default()
    };
    let db = match index.reader() {
        Ok(db) => db,
        Err(error) => {
            let _ = sink.finish(SearchFinished::Failed {
                generation,
                message: error.to_string(),
            });
            return;
        }
    };

    let query = Query {
        request: &request,
        name_matcher: name_matcher.as_ref(),
        content_matcher: content_matcher.as_ref(),
    };
    let completed = if content_matcher.is_some() {
        content_results(index, &db, &query, &mut sink, &mut stats)
    } else {
        name_results(&db, &query, &mut sink, &mut stats)
    };

    let _ = sink.flush(&mut stats);
    if sink.is_cancelled() {
        let _ = sink.finish(SearchFinished::Cancelled { generation, stats });
    } else if let Err(error) = completed {
        let _ = sink.finish(SearchFinished::Failed {
            generation,
            message: error.to_string(),
        });
    } else {
        let _ = sink.finish(SearchFinished::Complete { generation, stats });
    }
}

struct Query<'a> {
    request: &'a SearchRequest,
    name_matcher: Option<&'a NameMatcher>,
    content_matcher: Option<&'a ContentMatcher>,
}

impl Query<'_> {
    /// The literal the trigram index can be asked for, if any.
    ///
    /// A regex name pattern has no literal to look up, so the index falls back
    /// to a bounded range scan of the scope and applies the regex in Rust —
    /// still orders of magnitude less work than walking the tree.
    fn trigram_pattern(&self) -> Option<&str> {
        (self.request.name.syntax == PatternSyntax::Literal)
            .then(|| self.request.name.pattern.trim())
            .filter(|pattern| !pattern.is_empty())
    }

    fn row_shape_matches(&self, row: &FileRow) -> bool {
        if !extension_matches(Path::new(&row.path), &self.request.filters.extensions) {
            return false;
        }
        if self
            .request
            .filters
            .max_depth
            .is_some_and(|max| row.depth as usize > max)
        {
            return false;
        }
        true
    }

    fn row_matches(&self, row: &FileRow, kind: EntryKind) -> bool {
        kind_matches(self.request.filters.kind, kind) && self.row_shape_matches(row)
    }
}

fn name_results(
    db: &Db,
    query: &Query<'_>,
    sink: &mut EventSink,
    stats: &mut SearchStats,
) -> Result<()> {
    for root in &query.request.scope.roots {
        if sink.is_cancelled() {
            return Ok(());
        }
        let Some(scope) = root.as_local().and_then(Path::to_str) else {
            stats.skipped += 1;
            stats.roots_done += 1;
            continue;
        };
        let mut after_path: Option<String> = None;
        loop {
            if sink.is_cancelled() {
                return Ok(());
            }
            let candidates = db.candidate_page(
                scope,
                query.trigram_pattern(),
                false,
                after_path.as_deref(),
                INDEX_CANDIDATE_PAGE_SIZE,
            )?;
            let page_len = candidates.len();
            for row in candidates {
                after_path = Some(row.path.clone());
                if sink.is_cancelled() {
                    return Ok(());
                }
                stats.entries_seen += 1;
                if !query.row_shape_matches(&row) {
                    continue;
                }
                let Some(ranges) = query
                    .name_matcher
                    .map(|matcher| matcher.find(&row.name).map(|range| vec![range]))
                    .unwrap_or_else(|| Some(Vec::new()))
                else {
                    continue;
                };
                let Some((metadata, kind)) = current_metadata(&row) else {
                    continue;
                };
                if !query.row_matches(&row, kind) {
                    continue;
                }
                let path = VfsPath::local(&row.path);
                let result = SearchResult {
                    id: SearchResultId {
                        path: path.clone(),
                        match_key: MatchKey::Path,
                    },
                    path,
                    name: SmolStr::new(&row.name),
                    kind,
                    metadata: Some(metadata),
                    source: ResultSource::Index,
                    rank: None,
                    matched: SearchMatch::Filename { ranges },
                };
                if !sink.push(result, stats) {
                    return Ok(());
                }
            }
            if page_len < INDEX_CANDIDATE_PAGE_SIZE {
                break;
            }
        }
        stats.roots_done += 1;
        let progress = SearchProgress {
            generation: sink.generation(),
            stats: *stats,
        };
        if !sink.send(SearchEvent::Progress(progress)) {
            return Ok(());
        }
    }
    Ok(())
}

fn content_results(
    index: &Index,
    db: &Db,
    query: &Query<'_>,
    sink: &mut EventSink,
    stats: &mut SearchStats,
) -> Result<()> {
    let literal = query
        .request
        .content
        .as_ref()
        .map(|content| content.pattern.clone())
        .unwrap_or_default();
    let Some(search) = index.content().prepare_search(&literal)? else {
        // The planner classifies this as partial so the live engine supplies
        // the matches the tokenizer cannot represent.
        stats.roots_done = stats.roots_total;
        return Ok(());
    };
    let matcher = query
        .content_matcher
        .expect("content_results is only reached with a compiled content matcher");
    let mut searcher = build_content_searcher();

    let mut offset = 0;
    loop {
        if sink.is_cancelled() {
            return Ok(());
        }
        let page = search.page(offset, CONTENT_CANDIDATE_PAGE_SIZE)?;
        for hit in page.paths {
            if sink.is_cancelled() {
                return Ok(());
            }
            stats.entries_seen += 1;
            let path = VfsPath::local(&hit.path);
            if !query
                .request
                .scope
                .roots
                .iter()
                .any(|root| path.starts_with(root))
            {
                continue;
            }
            let Some(row) = db.file(&hit.path)? else {
                continue;
            };
            if !query.row_shape_matches(&row) {
                continue;
            }
            if let Some(name_matcher) = query.name_matcher
                && name_matcher.find(&row.name).is_none()
            {
                continue;
            }
            let Some((metadata, kind)) = current_metadata(&row) else {
                continue;
            };
            if !query.row_matches(&row, kind) {
                continue;
            }
            if !send_matches(
                &mut searcher,
                matcher,
                &row,
                &hit,
                kind,
                metadata,
                sink,
                stats,
            ) {
                return Ok(());
            }
        }
        offset = page.next_offset;
        if page.exhausted {
            break;
        }
    }
    stats.roots_done = stats.roots_total;
    Ok(())
}

/// Runs the content matcher over one candidate file and turns every hit into a
/// row identical in shape to the live path's.
#[allow(clippy::too_many_arguments)]
fn send_matches(
    searcher: &mut Searcher,
    matcher: &ContentMatcher,
    row: &FileRow,
    hit: &super::content::RankedPath,
    kind: EntryKind,
    metadata: Metadata,
    event_sink: &mut EventSink,
    stats: &mut SearchStats,
) -> bool {
    let mut keep_searching = true;
    let result_sink = ResultSink {
        matcher: &matcher.matcher,
        row,
        kind,
        rank: hit.rank,
        metadata,
        event_sink,
        stats,
        keep_searching: &mut keep_searching,
    };
    let _ = searcher.search_path(&matcher.matcher, &row.path, result_sink);
    keep_searching
}

struct ResultSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    row: &'a FileRow,
    kind: EntryKind,
    rank: u32,
    metadata: Metadata,
    event_sink: &'a mut EventSink,
    stats: &'a mut SearchStats,
    keep_searching: &'a mut bool,
}

impl Sink for ResultSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        let line = strip_line_ending(matched.bytes());
        let mut matches: Vec<Match> = Vec::new();
        self.matcher
            .find_iter(line, |found| {
                matches.push(found);
                matches.len() < MAX_RANGES_PER_PREVIEW
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(first) = matches.first().copied() else {
            return Ok(true);
        };
        let line_number = matched.line_number().unwrap_or(0);
        let line_start = matched.absolute_byte_offset();
        let (preview_text, ranges) = preview(line, &matches);
        let path = VfsPath::local(&self.row.path);
        let result = SearchResult {
            id: SearchResultId {
                path: path.clone(),
                match_key: MatchKey::Content {
                    line: line_number,
                    byte_start: line_start + first.start() as u64,
                    byte_end: line_start + first.end() as u64,
                },
            },
            path,
            name: SmolStr::new(&self.row.name),
            kind: self.kind,
            metadata: Some(self.metadata),
            source: ResultSource::Index,
            rank: Some(self.rank),
            matched: SearchMatch::Content {
                line: line_number,
                column: display_column(line, first),
                preview: preview_text,
                ranges,
            },
        };
        *self.keep_searching = self.event_sink.push(result, self.stats);
        Ok(*self.keep_searching)
    }
}

/// Re-reads a candidate's metadata, or `None` if it is gone.
fn current_metadata(row: &FileRow) -> Option<(Metadata, EntryKind)> {
    let metadata = std::fs::symlink_metadata(&row.path).ok()?;
    let kind = if metadata.file_type().is_dir() {
        EntryKind::Dir
    } else if metadata.file_type().is_file() {
        EntryKind::File
    } else if metadata.file_type().is_symlink() {
        let target = std::fs::metadata(&row.path).ok();
        EntryKind::Symlink {
            target_kind: target.map(|target| {
                if target.is_dir() {
                    ferret_vfs::SymlinkTarget::Dir
                } else {
                    ferret_vfs::SymlinkTarget::File
                }
            }),
        }
    } else {
        EntryKind::Other
    };
    Some((
        Metadata {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            hidden: row.hidden,
            name_hidden: row.hidden,
            platform_hidden: false,
        },
        kind,
    ))
}
