use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use async_channel::Sender;
use ferret_vfs::{EntryKind, Metadata, VfsPath};
use grep_matcher::{Match, Matcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};
use smol_str::SmolStr;

use crate::config::{
    CONTENT_REGEX_SIZE_LIMIT, CONTENT_SEARCH_HEAP_LIMIT, EVENT_BUFFER, MAX_CONTENT_PATTERN_BYTES,
    MAX_PATTERN_BYTES, MAX_PREVIEW_BYTES, MAX_RANGES_PER_PREVIEW, live_search_threads,
};
use crate::sink::EventSink;
use crate::{
    CaseMode, ContentQuery, EntryKindFilter, MatchKey, NameMatchMode, PatternSyntax, Result,
    ResultSource, SearchError, SearchEvent, SearchFinished, SearchMatch, SearchProgress,
    SearchRequest, SearchResult, SearchResultId, SearchSession, SearchStats, TextRange,
};

/// The always-available search path: a current filesystem walk with no index.
///
/// Indexed and daemon-backed services will implement the same [`crate::SearchService`]
/// contract. Keeping this implementation real is what lets a corrupt or
/// rebuilding index degrade to slower search instead of no search.
#[derive(Clone, Copy, Debug, Default)]
pub struct InProcessSearchService;

impl InProcessSearchService {
    fn start_with_threads(
        self,
        mut request: SearchRequest,
        walker_threads: usize,
    ) -> Result<SearchSession> {
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
        let task_cancelled = Arc::clone(&cancelled);

        let pool = ferret_vfs::pool::pool().expect("Ferret's worker pool is always available");
        pool.spawn(move || {
            run(
                request,
                name_matcher,
                content_matcher,
                walker_threads,
                events_tx,
                task_cancelled,
            )
        });

        Ok(SearchSession::new(generation, events_rx, cancelled))
    }

    #[cfg(test)]
    pub(crate) fn start_for_measurement(
        self,
        request: SearchRequest,
        walker_threads: usize,
    ) -> Result<SearchSession> {
        self.start_with_threads(request, walker_threads)
    }
}

impl crate::SearchService for InProcessSearchService {
    fn start(&self, request: SearchRequest) -> Result<SearchSession> {
        (*self).start_with_threads(request, live_search_threads())
    }
}

pub(crate) fn validate_and_normalize(request: &mut SearchRequest) -> Result<()> {
    if request.scope.roots.is_empty() {
        return Err(SearchError::EmptyScope);
    }
    if request
        .content
        .as_ref()
        .is_some_and(|query| query.pattern.trim().is_empty())
    {
        request.content = None;
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
    if let Some(content) = &request.content
        && content.pattern.len() > MAX_CONTENT_PATTERN_BYTES
    {
        return Err(SearchError::ContentPatternTooLong {
            max_bytes: MAX_CONTENT_PATTERN_BYTES,
            actual_bytes: content.pattern.len(),
        });
    }
    if request.limits.max_results == 0 {
        return Err(SearchError::ZeroResultLimit);
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

pub(crate) struct NameMatcher {
    regex: Regex,
}

impl NameMatcher {
    pub(crate) fn compile(request: &SearchRequest) -> Result<Self> {
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

    pub(crate) fn find(&self, name: &str) -> Option<TextRange> {
        self.regex.find(name).map(|matched| TextRange {
            start: matched.start(),
            end: matched.end(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct ContentMatcher {
    pub(crate) matcher: RegexMatcher,
}

impl ContentMatcher {
    pub(crate) fn compile(query: &ContentQuery) -> Result<Self> {
        let mut builder = RegexMatcherBuilder::new();
        builder
            .fixed_strings(query.syntax == PatternSyntax::Literal)
            .word(query.whole_word)
            .line_terminator(Some(b'\n'))
            .size_limit(CONTENT_REGEX_SIZE_LIMIT);
        match query.case {
            CaseMode::Smart => {
                builder.case_smart(true);
            }
            CaseMode::Sensitive => {
                builder.case_insensitive(false);
            }
            CaseMode::Insensitive => {
                builder.case_insensitive(true);
            }
        }
        let matcher =
            builder
                .build(&query.pattern)
                .map_err(|error| SearchError::InvalidContentPattern {
                    pattern: query.pattern.clone(),
                    message: error.to_string(),
                })?;
        Ok(Self { matcher })
    }
}

fn run(
    request: SearchRequest,
    name_matcher: Option<NameMatcher>,
    content_matcher: Option<ContentMatcher>,
    walker_threads: usize,
    events: Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let generation = request.generation;
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_events = events.clone();
    let sink = EventSink::new(generation, events, cancelled, request.limits);
    if !sink.send(SearchEvent::Started { generation }) {
        return;
    }
    let shared = Mutex::new(ParallelState {
        sink,
        stats: SearchStats {
            roots_total: request.scope.roots.len(),
            ..SearchStats::default()
        },
    });
    let stop = AtomicBool::new(false);

    'roots: for root in &request.scope.roots {
        if stop_requested(&stop, &worker_cancelled, &worker_events) {
            break;
        }
        let Some(local_root) = root.as_local() else {
            let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
            state.stats.skipped += 1;
            if !state
                .sink
                .warning(root.clone(), "Remote search is not available yet.")
            {
                break;
            }
            state.stats.roots_done += 1;
            continue;
        };

        let mut builder = WalkBuilder::new(local_root);
        builder
            .threads(walker_threads.max(1))
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

        let entries_seen = AtomicU64::new(0);
        let skipped = AtomicU64::new(0);
        // `ignore` owns traversal for search (D-34). Its callback factory
        // gives each bounded worker a private Searcher, while the mutex below
        // covers only result batching and counters—not filesystem I/O.
        builder.build_parallel().run(|| {
            let mut searcher = content_matcher.as_ref().map(|_| build_content_searcher());
            let shared = &shared;
            let stop = &stop;
            let entries_seen = &entries_seen;
            let skipped = &skipped;
            let worker_cancelled = &worker_cancelled;
            let worker_events = &worker_events;
            let request = &request;
            let name_matcher = &name_matcher;
            let content_matcher = &content_matcher;
            let context = WalkContext {
                root: root.clone(),
                request,
                name_matcher: name_matcher.as_ref(),
                content_matcher: content_matcher.as_ref(),
                shared,
                stop,
                entries_seen,
                skipped,
            };
            Box::new(move |walked| {
                if stop_requested(stop, worker_cancelled, worker_events) {
                    return WalkState::Quit;
                }
                context.process(walked, searcher.as_mut())
            })
        });

        let mut state = shared.lock().unwrap_or_else(|error| error.into_inner());
        state.stats.entries_seen += entries_seen.load(Ordering::Relaxed);
        state.stats.skipped += skipped.load(Ordering::Relaxed);
        state.stats.roots_done += 1;
        let mut stats = state.stats;
        if !state.sink.flush(&mut stats) {
            stop.store(true, Ordering::Release);
            break 'roots;
        }
        state.stats = stats;
        if !state
            .sink
            .send(SearchEvent::Progress(SearchProgress { generation, stats }))
        {
            stop.store(true, Ordering::Release);
            break 'roots;
        }
    }

    let mut state = shared
        .into_inner()
        .unwrap_or_else(|error| error.into_inner());
    let mut stats = state.stats;
    if state.sink.is_cancelled() {
        let _ = state
            .sink
            .finish(SearchFinished::Cancelled { generation, stats });
    } else {
        let _ = state.sink.flush(&mut stats);
        let _ = state
            .sink
            .finish(SearchFinished::Complete { generation, stats });
    }
}

struct ParallelState {
    sink: EventSink,
    stats: SearchStats,
}

impl ParallelState {
    fn push(&mut self, result: SearchResult) -> bool {
        let mut stats = self.stats;
        let keep_searching = self.sink.push(result, &mut stats);
        self.stats = stats;
        keep_searching
    }
}

struct WalkContext<'a> {
    root: VfsPath,
    request: &'a SearchRequest,
    name_matcher: Option<&'a NameMatcher>,
    content_matcher: Option<&'a ContentMatcher>,
    shared: &'a Mutex<ParallelState>,
    stop: &'a AtomicBool,
    entries_seen: &'a AtomicU64,
    skipped: &'a AtomicU64,
}

impl WalkContext<'_> {
    fn process(
        &self,
        walked: std::result::Result<ignore::DirEntry, ignore::Error>,
        searcher: Option<&mut Searcher>,
    ) -> WalkState {
        let entry = match walked {
            Ok(entry) => entry,
            Err(error) => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                let mut state = self.lock_state();
                if !state.sink.warning(self.root.clone(), error.to_string()) {
                    self.stop.store(true, Ordering::Release);
                    return WalkState::Quit;
                }
                return WalkState::Continue;
            }
        };
        if entry.depth() == 0 {
            return WalkState::Continue;
        }
        self.entries_seen.fetch_add(1, Ordering::Relaxed);

        let Some(file_type) = entry.file_type() else {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return WalkState::Continue;
        };
        let kind = entry_kind(file_type);
        if !kind_matches(self.request.filters.kind, kind)
            || !extension_matches(entry.path(), &self.request.filters.extensions)
        {
            return WalkState::Continue;
        }
        let entry_path = entry.path().to_path_buf();
        let Some(name) = entry_path.file_name().and_then(|name| name.to_str()) else {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return WalkState::Continue;
        };
        let name_range = match self.name_matcher {
            Some(matcher) => {
                let Some(range) = matcher.find(name) else {
                    return WalkState::Continue;
                };
                Some(range)
            }
            None => None,
        };
        let metadata = metadata_for(&entry_path, name);
        let name = SmolStr::new(name);
        let path = VfsPath::from(entry_path.clone());

        if let Some(content_matcher) = self.content_matcher {
            if kind != EntryKind::File {
                return WalkState::Continue;
            }
            let searcher = searcher.expect("content workers create a private searcher");
            let content_sink = ContentFileSink {
                matcher: &content_matcher.matcher,
                template: ResultTemplate {
                    path,
                    name,
                    kind,
                    metadata,
                },
                shared: self.shared,
                stop: self.stop,
            };
            if let Err(error) =
                searcher.search_path(&content_matcher.matcher, &entry_path, content_sink)
            {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                let mut state = self.lock_state();
                if !state.sink.warning(
                    VfsPath::from(entry_path),
                    format!("Couldn't search contents: {error}"),
                ) {
                    self.stop.store(true, Ordering::Release);
                    return WalkState::Quit;
                }
            }
        } else if let Some(range) = name_range {
            let mut state = self.lock_state();
            if !state.push(SearchResult {
                id: SearchResultId {
                    path: path.clone(),
                    match_key: MatchKey::Path,
                },
                path,
                name,
                kind,
                metadata,
                source: ResultSource::Live,
                rank: None,
                matched: SearchMatch::Filename {
                    ranges: vec![range],
                },
            }) {
                self.stop.store(true, Ordering::Release);
            }
        }
        if self.stop.load(Ordering::Acquire) {
            WalkState::Quit
        } else {
            WalkState::Continue
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ParallelState> {
        self.shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

pub(crate) fn build_content_searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .bom_sniffing(true)
        .heap_limit(Some(CONTENT_SEARCH_HEAP_LIMIT))
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build()
}

fn stop_requested(stop: &AtomicBool, cancelled: &AtomicBool, events: &Sender<SearchEvent>) -> bool {
    stop.load(Ordering::Acquire) || cancelled.load(Ordering::Acquire) || events.is_closed()
}

struct ResultTemplate {
    path: VfsPath,
    name: SmolStr,
    kind: EntryKind,
    metadata: Option<Metadata>,
}

struct ContentFileSink<'a> {
    matcher: &'a RegexMatcher,
    template: ResultTemplate,
    shared: &'a Mutex<ParallelState>,
    stop: &'a AtomicBool,
}

impl Sink for ContentFileSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        if self.stop.load(Ordering::Acquire) {
            return Ok(false);
        }
        {
            let state = self
                .shared
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.sink.is_cancelled() {
                self.stop.store(true, Ordering::Release);
                return Ok(false);
            }
        }
        let line = strip_line_ending(matched.bytes());
        let mut matches = Vec::new();
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
        let absolute_line_start = matched.absolute_byte_offset();
        let byte_start = absolute_line_start + first.start() as u64;
        let byte_end = absolute_line_start + first.end() as u64;
        let (preview, ranges) = preview(line, &matches);
        let result = SearchResult {
            id: SearchResultId {
                path: self.template.path.clone(),
                match_key: MatchKey::Content {
                    line: line_number,
                    byte_start,
                    byte_end,
                },
            },
            path: self.template.path.clone(),
            name: self.template.name.clone(),
            kind: self.template.kind,
            metadata: self.template.metadata,
            source: ResultSource::Live,
            rank: None,
            matched: SearchMatch::Content {
                line: line_number,
                column: display_column(line, first),
                preview,
                ranges,
            },
        };
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.push(result) {
            self.stop.store(true, Ordering::Release);
            return Ok(false);
        }
        Ok(true)
    }
}

pub(crate) fn preview(line: &[u8], matches: &[Match]) -> (SmolStr, Vec<TextRange>) {
    let first = matches[0];
    let context_before = MAX_PREVIEW_BYTES / 3;
    let start = first.start().saturating_sub(context_before);
    let end = line.len().min(start.saturating_add(MAX_PREVIEW_BYTES));
    let bytes = &line[start..end];
    let preview = String::from_utf8_lossy(bytes);
    let ranges = matches
        .iter()
        .filter_map(|found| {
            let raw_start = found.start().max(start);
            let raw_end = found.end().min(end);
            (raw_start <= raw_end && raw_start < end).then(|| TextRange {
                start: String::from_utf8_lossy(&line[start..raw_start]).len(),
                end: String::from_utf8_lossy(&line[start..raw_end]).len(),
            })
        })
        .collect();
    (SmolStr::new(preview.as_ref()), ranges)
}

pub(crate) fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes)
}

pub(crate) fn display_column(line: &[u8], found: Match) -> u64 {
    String::from_utf8_lossy(&line[..found.start()])
        .chars()
        .count() as u64
        + 1
}

pub(crate) fn entry_kind(file_type: std::fs::FileType) -> EntryKind {
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

pub(crate) fn kind_matches(filter: EntryKindFilter, kind: EntryKind) -> bool {
    match filter {
        EntryKindFilter::All => true,
        EntryKindFilter::Files => !kind.is_dir_like(),
        EntryKindFilter::Folders => kind.is_dir_like(),
    }
}

pub(crate) fn extension_matches(path: &Path, extensions: &[SmolStr]) -> bool {
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

pub(crate) fn metadata_for(path: &Path, name: &str) -> Option<Metadata> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    Some(Metadata {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        hidden: name.starts_with('.'),
    })
}
