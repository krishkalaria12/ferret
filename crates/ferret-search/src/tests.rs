use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferret_vfs::{EntryKind, MountId, VfsPath};

use super::*;

fn tempdir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ferret-search-{tag}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn request(root: &Path, pattern: &str) -> SearchRequest {
    SearchRequest {
        generation: SearchGeneration(42),
        scope: SearchScope {
            kind: SearchScopeKind::CurrentFolder,
            roots: vec![VfsPath::from(root)],
        },
        name: NameQuery::substring(pattern),
        content: None,
        filters: SearchFilters::default(),
        limits: SearchLimits::default(),
    }
}

fn content_request(root: &Path, pattern: &str) -> SearchRequest {
    let mut request = request(root, "");
    request.content = Some(ContentQuery {
        pattern: pattern.into(),
        syntax: PatternSyntax::Literal,
        case: CaseMode::Smart,
        whole_word: false,
    });
    request
}

fn collect(session: &SearchSession) -> Vec<SearchEvent> {
    smol::block_on(async {
        let mut events = Vec::new();
        while let Some(event) = session.next().await {
            let terminal = matches!(event, SearchEvent::Finished(_));
            events.push(event);
            if terminal {
                break;
            }
        }
        events
    })
}

fn results(events: &[SearchEvent]) -> Vec<&SearchResult> {
    events
        .iter()
        .filter_map(|event| match event {
            SearchEvent::Batch(batch) => Some(batch.results.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn rejects_invalid_requests_before_starting_work() {
    let service = InProcessSearchService;
    let root = tempdir("invalid");

    let mut empty = request(&root, "");
    assert!(matches!(
        service.start(empty.clone()),
        Err(SearchError::EmptyQuery)
    ));

    empty.name.pattern = "x".into();
    empty.scope.roots.clear();
    assert!(matches!(service.start(empty), Err(SearchError::EmptyScope)));

    let mut zero = request(&root, "x");
    zero.limits.max_results = 0;
    assert!(matches!(
        service.start(zero),
        Err(SearchError::ZeroResultLimit)
    ));

    fs::remove_dir_all(root).ok();
}

#[test]
fn smart_case_only_becomes_sensitive_for_uppercase_queries() {
    assert!(!CaseMode::Smart.is_sensitive("readme"));
    assert!(CaseMode::Smart.is_sensitive("Readme"));
    assert!(CaseMode::Sensitive.is_sensitive("readme"));
    assert!(!CaseMode::Insensitive.is_sensitive("README"));
}

#[test]
fn streams_filename_matches_with_the_request_generation() {
    let root = tempdir("generation");
    fs::write(root.join("README.md"), "hello").unwrap();
    fs::write(root.join("other.txt"), "hello").unwrap();

    let session = InProcessSearchService
        .start(request(&root, "readme"))
        .unwrap();
    let events = collect(&session);
    assert!(!events.is_empty());
    assert!(
        events
            .iter()
            .all(|event| event.generation() == SearchGeneration(42))
    );
    let found = results(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "README.md");
    assert_eq!(found[0].source, ResultSource::Live);
    assert!(matches!(
        events.last(),
        Some(SearchEvent::Finished(SearchFinished::Complete { .. }))
    ));

    fs::remove_dir_all(root).ok();
}

#[test]
fn filters_kind_and_normalized_extensions() {
    let root = tempdir("filters");
    fs::create_dir(root.join("match-folder")).unwrap();
    fs::write(root.join("match.RS"), "").unwrap();
    fs::write(root.join("match.txt"), "").unwrap();

    let mut files = request(&root, "match");
    files.filters.kind = EntryKindFilter::Files;
    files.filters.extensions = vec![".RS".into(), "rs".into()];
    let session = InProcessSearchService.start(files).unwrap();
    let events = collect(&session);
    let found = results(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "match.RS");
    assert_eq!(found[0].kind, EntryKind::File);

    let mut folders = request(&root, "match");
    folders.filters.kind = EntryKindFilter::Folders;
    let session = InProcessSearchService.start(folders).unwrap();
    let events = collect(&session);
    assert_eq!(results(&events)[0].name, "match-folder");

    fs::remove_dir_all(root).ok();
}

#[test]
fn respects_ignore_and_hidden_policy() {
    let root = tempdir("ignore");
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(root.join("ignored.txt"), "").unwrap();
    fs::write(root.join("visible.txt"), "").unwrap();
    fs::write(root.join(".hidden.txt"), "").unwrap();

    let session = InProcessSearchService.start(request(&root, "txt")).unwrap();
    let events = collect(&session);
    let names: Vec<_> = results(&events)
        .iter()
        .map(|result| result.name.as_str())
        .collect();
    assert_eq!(names, ["visible.txt"]);

    let mut inclusive = request(&root, "txt");
    inclusive.filters.respect_ignore = false;
    inclusive.filters.include_hidden = true;
    let session = InProcessSearchService.start(inclusive).unwrap();
    let events = collect(&session);
    let mut names: Vec<_> = results(&events)
        .iter()
        .map(|result| result.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, [".hidden.txt", "ignored.txt", "visible.txt"]);

    fs::remove_dir_all(root).ok();
}

#[test]
fn caps_results_and_finishes_terminally() {
    let root = tempdir("cap");
    for index in 0..20 {
        fs::write(root.join(format!("match-{index}.txt")), "").unwrap();
    }
    let mut capped = request(&root, "match");
    capped.limits.max_results = 3;

    let session = InProcessSearchService.start(capped).unwrap();
    let events = collect(&session);
    assert_eq!(results(&events).len(), 3);
    assert!(matches!(
        events.last(),
        Some(SearchEvent::Finished(SearchFinished::Complete {
            stats: SearchStats {
                truncated: true,
                ..
            },
            ..
        }))
    ));
    assert!(
        smol::block_on(session.next()).is_none(),
        "Finished must be terminal"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn remote_roots_warn_without_assuming_local_path_syntax() {
    let mut remote = request(Path::new("/unused"), "needle");
    remote.scope.roots = vec![VfsPath::remote(MountId(7), "/srv/data")];

    let session = InProcessSearchService.start(remote).unwrap();
    let events = collect(&session);
    assert!(events.iter().any(|event| matches!(
        event,
        SearchEvent::Warning(SearchWarning {
            subject: VfsPath::Remote {
                mount: MountId(7),
                ..
            },
            ..
        })
    )));
    assert!(matches!(events.last(), Some(SearchEvent::Finished(_))));
}

#[test]
fn cancellation_closes_the_observable_stream() {
    let root = tempdir("cancel");
    for index in 0..1_000 {
        fs::write(root.join(format!("match-{index}.txt")), "").unwrap();
    }

    let session = InProcessSearchService
        .start(request(&root, "match"))
        .unwrap();
    session.cancel();
    assert!(session.is_cancelled());
    assert!(smol::block_on(session.next()).is_none());

    fs::remove_dir_all(root).ok();
}

#[test]
fn streams_one_content_result_per_matching_line() {
    let root = tempdir("content-lines");
    fs::write(
        root.join("notes.txt"),
        "first needle needle\nno match\nthird needle\n",
    )
    .unwrap();

    let session = InProcessSearchService
        .start(content_request(&root, "needle"))
        .unwrap();
    let events = collect(&session);
    let found = results(&events);
    assert_eq!(found.len(), 2);
    assert!(matches!(
        &found[0].matched,
        SearchMatch::Content {
            line: 1,
            column: 7,
            preview,
            ranges,
        } if preview == "first needle needle" && ranges.len() == 2
    ));
    assert!(matches!(
        &found[1].matched,
        SearchMatch::Content { line: 3, .. }
    ));

    fs::remove_dir_all(root).ok();
}

#[test]
fn content_previews_do_not_panic_on_multibyte_context() {
    let root = tempdir("content-unicode-preview");
    let line = format!("{}needle", "😀".repeat(100));
    fs::write(root.join("unicode.txt"), line).unwrap();

    let events = collect(
        &InProcessSearchService
            .start(content_request(&root, "needle"))
            .unwrap(),
    );
    let found = results(&events);
    assert_eq!(found.len(), 1);
    let SearchMatch::Content { preview, .. } = &found[0].matched else {
        panic!("expected a content match");
    };
    assert!(preview.contains("needle"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn combines_filename_and_regex_content_constraints() {
    let root = tempdir("content-filters");
    fs::write(root.join("target.rs"), "alpha 42\nALPHA 7\n").unwrap();
    fs::write(root.join("other.rs"), "alpha 42\n").unwrap();

    let mut request = content_request(&root, r"alpha \d+");
    request.name = NameQuery::substring("target");
    request.content.as_mut().unwrap().syntax = PatternSyntax::Regex;
    let events = collect(&InProcessSearchService.start(request).unwrap());
    let found = results(&events);
    assert_eq!(found.len(), 2, "smart case should match alpha and ALPHA");
    assert!(found.iter().all(|result| result.name == "target.rs"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn skips_binary_files_and_bounds_long_previews() {
    let root = tempdir("content-bounds");
    let mut long_line = vec![b'x'; config::MAX_PREVIEW_BYTES * 4];
    long_line.extend_from_slice(b"needle\n");
    fs::write(root.join("long.txt"), long_line).unwrap();
    fs::write(root.join("binary.dat"), b"needle\0after").unwrap();

    let events = collect(
        &InProcessSearchService
            .start(content_request(&root, "needle"))
            .unwrap(),
    );
    let found = results(&events);
    assert_eq!(found.len(), 1);
    let SearchMatch::Content { preview, .. } = &found[0].matched else {
        panic!("expected a content match");
    };
    assert!(preview.len() <= config::MAX_PREVIEW_BYTES);

    fs::remove_dir_all(root).ok();
}

// Linux filesystems accept arbitrary non-NUL filename bytes. macOS rejects
// this fixture with EILSEQ before Ferret can search it.
#[cfg(target_os = "linux")]
#[test]
fn content_search_preserves_non_utf8_local_paths() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let root = tempdir("content-non-utf8");
    let path = root.join(OsString::from_vec(b"native-\xff.txt".to_vec()));
    fs::write(&path, b"find this needle\n").unwrap();

    let events = collect(
        &InProcessSearchService
            .start(content_request(&root, "needle"))
            .unwrap(),
    );
    let found = results(&events);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path.as_local(), Some(path.as_path()));
    assert!(matches!(found[0].matched, SearchMatch::Content { .. }));

    fs::remove_dir_all(root).ok();
}

#[test]
#[ignore = "measurement; creates a temporary broad content-search corpus"]
fn parallel_content_search_cost() {
    let root = tempdir("parallel-cost");
    let body = format!("{}needle\n", "x".repeat(32 * 1024));
    for index in 0..2_000 {
        fs::write(root.join(format!("source-{index}.txt")), &body).unwrap();
    }
    let request = content_request(&root, "needle");
    let parallel_threads = config::live_search_threads();

    let measure = |threads| {
        let started = Instant::now();
        let events = collect(
            &InProcessSearchService
                .start_for_measurement(request.clone(), threads)
                .unwrap(),
        );
        assert_eq!(results(&events).len(), 2_000);
        started.elapsed()
    };
    let _ = measure(1);
    let _ = measure(parallel_threads);
    let mut serial = [Duration::ZERO; 3];
    let mut parallel = [Duration::ZERO; 3];
    for index in 0..3 {
        serial[index] = measure(1);
        parallel[index] = measure(parallel_threads);
    }
    serial.sort_unstable();
    parallel.sort_unstable();
    eprintln!(
        "live content search: serial {:?}, {} workers {:?}",
        serial[1], parallel_threads, parallel[1]
    );

    fs::remove_dir_all(root).ok();
}
