use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

use async_channel::Sender;
use ferret_vfs::{EntryKind, Metadata, VfsPath};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use smol_str::SmolStr;

use crate::config::{EVENT_BUFFER, MAX_PATTERN_BYTES, RESULT_BATCH_SIZE};
use crate::{
    EntryKindFilter, MatchKey, NameMatchMode, PatternSyntax, Result, ResultSource, SearchBatch,
    SearchError, SearchEvent, SearchFinished, SearchGeneration, SearchLimits, SearchMatch,
    SearchProgress, SearchRequest, SearchResult, SearchResultId, SearchSession, SearchStats,
    SearchWarning, TextRange,
};

/// The always-available search path: a current filesystem walk with no index.
///
/// Indexed and daemon-backed services will implement the same [`crate::SearchService`]
/// contract. Keeping this implementation real is what lets a corrupt or
/// rebuilding index degrade to slower search instead of no search.
#[derive(Clone, Copy, Debug, Default)]
pub struct InProcessSearchService;

impl crate::SearchService for InProcessSearchService {
    fn start(&self, mut request: SearchRequest) -> Result<SearchSession> {
        validate_and_normalize(&mut request)?;
        let matcher = NameMatcher::compile(&request)?;
        let generation = request.generation;
        let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = Arc::clone(&cancelled);

        let pool = ferret_vfs::pool::pool().expect("Ferret's worker pool is always available");
        pool.spawn(move || run(request, matcher, events_tx, task_cancelled));

        Ok(SearchSession::new(generation, events_rx, cancelled))
    }
}

fn validate_and_normalize(request: &mut SearchRequest) -> Result<()> {
    if request.scope.roots.is_empty() {
        return Err(SearchError::EmptyScope);
    }
    if request.name.pattern.trim().is_empty() && request.content.is_none() {
        return Err(SearchError::EmptyQuery);
    }
    if request.name.pattern.len() > MAX_PATTERN_BYTES {
        return Err(SearchError::PatternTooLong {
            max_bytes: MAX_PATTERN_BYTES,
            actual_bytes: request.name.pattern.len(),
        });
    }
    if request.limits.max_results == 0 {
        return Err(SearchError::ZeroResultLimit);
    }
    if request.content.is_some() {
        return Err(SearchError::UnsupportedContentSearch);
    }
    if request.name.mode != NameMatchMode::Substring {
        return Err(SearchError::UnsupportedNameMode { mode: "fuzzy" });
    }

    for extension in &mut request.filters.extensions {
        let normalized = extension.trim().trim_start_matches('.').to_lowercase();
        *extension = SmolStr::new(normalized);
    }
    request
        .filters
        .extensions
        .retain(|extension| !extension.is_empty());
    request.filters.extensions.sort_unstable();
    request.filters.extensions.dedup();
    Ok(())
}

struct NameMatcher {
    regex: Regex,
}

impl NameMatcher {
    fn compile(request: &SearchRequest) -> Result<Self> {
        let pattern = match request.name.syntax {
            PatternSyntax::Literal => regex::escape(&request.name.pattern),
            PatternSyntax::Regex => request.name.pattern.clone(),
        };
        let sensitive = request.name.case.is_sensitive(&request.name.pattern);
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!sensitive)
            .build()
            .map_err(|error| SearchError::InvalidNamePattern {
                pattern: request.name.pattern.clone(),
                message: error.to_string(),
            })?;
        Ok(Self { regex })
    }

    fn find(&self, name: &str) -> Option<TextRange> {
        self.regex.find(name).map(|matched| TextRange {
            start: matched.start(),
            end: matched.end(),
        })
    }
}

fn run(
    request: SearchRequest,
    matcher: NameMatcher,
    events: Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let generation = request.generation;
    let mut sink = EventSink::new(generation, events, cancelled, request.limits);
    if !sink.send(SearchEvent::Started { generation }) {
        return;
    }

    let mut stats = SearchStats {
        roots_total: request.scope.roots.len(),
        ..SearchStats::default()
    };

    'roots: for root in &request.scope.roots {
        if sink.is_cancelled() {
            break;
        }
        let Some(local_root) = root.as_local() else {
            stats.skipped += 1;
            if !sink.warning(root.clone(), "Remote filename search is not available yet.") {
                break;
            }
            stats.roots_done += 1;
            continue;
        };

        let mut builder = WalkBuilder::new(local_root);
        builder
            .hidden(!request.filters.include_hidden)
            .ignore(request.filters.respect_ignore)
            .git_ignore(request.filters.respect_ignore)
            .git_global(request.filters.respect_ignore)
            .git_exclude(request.filters.respect_ignore)
            // A user can place `.gitignore` rules in any searched tree; Power
            // Search's "respect ignore files" promise is not conditional on
            // repository discovery.
            .require_git(false)
            .parents(request.filters.respect_ignore)
            .follow_links(request.filters.follow_symlinks)
            .same_file_system(request.filters.same_filesystem)
            .max_depth(request.filters.max_depth);

        // Deliberately serial inside one Ferret worker-pool job. `ignore`
        // owns search traversal per D-34, but its parallel walker creates a
        // private thread pool. That would violate the repository's one-pool
        // rule before the daemon has a scheduler of its own.
        for walked in builder.build() {
            if sink.is_cancelled() {
                break 'roots;
            }
            let entry = match walked {
                Ok(entry) => entry,
                Err(error) => {
                    stats.skipped += 1;
                    if !sink.warning(root.clone(), error.to_string()) {
                        break 'roots;
                    }
                    continue;
                }
            };
            // Keep the root visible to `ignore` so it can load ignore files
            // from that directory, but never return the scope itself as a
            // search result.
            if entry.depth() == 0 {
                continue;
            }
            stats.entries_seen += 1;

            let Some(file_type) = entry.file_type() else {
                stats.skipped += 1;
                continue;
            };
            let kind = entry_kind(file_type);
            if !kind_matches(request.filters.kind, kind)
                || !extension_matches(entry.path(), &request.filters.extensions)
            {
                continue;
            }

            let Some(name) = entry.path().file_name().and_then(|name| name.to_str()) else {
                stats.skipped += 1;
                continue;
            };
            let Some(range) = matcher.find(name) else {
                continue;
            };

            let metadata = metadata_for(entry.path(), name);
            let name = SmolStr::new(name);
            let path = VfsPath::from(entry.into_path());
            let result = SearchResult {
                id: SearchResultId {
                    path: path.clone(),
                    match_key: MatchKey::Path,
                },
                path,
                name,
                kind,
                metadata,
                source: ResultSource::Live,
                matched: SearchMatch::Filename {
                    ranges: vec![range],
                },
            };
            if !sink.push(result, &mut stats) {
                break 'roots;
            }
        }

        stats.roots_done += 1;
        if !sink.flush(&mut stats)
            || !sink.send(SearchEvent::Progress(SearchProgress { generation, stats }))
        {
            break;
        }
    }

    if sink.is_cancelled() {
        let _ = sink.finish(SearchFinished::Cancelled { generation, stats });
    } else {
        let _ = sink.flush(&mut stats);
        let _ = sink.finish(SearchFinished::Complete { generation, stats });
    }
}

fn entry_kind(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_dir() {
        EntryKind::Dir
    } else if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_symlink() {
        EntryKind::Symlink { target_kind: None }
    } else {
        EntryKind::Other
    }
}

fn kind_matches(filter: EntryKindFilter, kind: EntryKind) -> bool {
    match filter {
        EntryKindFilter::All => true,
        EntryKindFilter::Files => !kind.is_dir_like(),
        EntryKindFilter::Folders => kind.is_dir_like(),
    }
}

fn extension_matches(path: &Path, extensions: &[SmolStr]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };
    let extension = extension.to_lowercase();
    extensions
        .binary_search_by(|candidate| candidate.as_str().cmp(&extension))
        .is_ok()
}

fn metadata_for(path: &Path, name: &str) -> Option<Metadata> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(Metadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        hidden: name.starts_with('.'),
    })
}

struct EventSink {
    generation: SearchGeneration,
    events: Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
    pending: Vec<SearchResult>,
    limits: SearchLimits,
}

impl EventSink {
    fn new(
        generation: SearchGeneration,
        events: Sender<SearchEvent>,
        cancelled: Arc<AtomicBool>,
        limits: SearchLimits,
    ) -> Self {
        Self {
            generation,
            events,
            cancelled,
            pending: Vec::with_capacity(RESULT_BATCH_SIZE),
            limits,
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire) || self.events.is_closed()
    }

    fn send(&self, event: SearchEvent) -> bool {
        !self.is_cancelled() && self.events.send_blocking(event).is_ok()
    }

    fn warning(&self, subject: VfsPath, message: impl Into<String>) -> bool {
        self.send(SearchEvent::Warning(SearchWarning {
            generation: self.generation,
            subject,
            message: message.into(),
        }))
    }

    fn push(&mut self, result: SearchResult, stats: &mut SearchStats) -> bool {
        let retained = stats.matches_sent as usize + self.pending.len();
        if retained >= self.limits.max_results {
            stats.truncated = true;
            return false;
        }
        self.pending.push(result);
        if self.pending.len() >= RESULT_BATCH_SIZE {
            self.flush(stats)
        } else {
            true
        }
    }

    fn flush(&mut self, stats: &mut SearchStats) -> bool {
        if self.pending.is_empty() {
            return !self.is_cancelled();
        }
        let results = std::mem::take(&mut self.pending);
        stats.matches_sent += results.len() as u64;
        self.send(SearchEvent::Batch(SearchBatch {
            generation: self.generation,
            results,
        }))
    }

    fn finish(&self, finished: SearchFinished) -> bool {
        self.send(SearchEvent::Finished(finished))
    }
}
