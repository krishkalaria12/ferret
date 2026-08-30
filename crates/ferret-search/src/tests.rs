use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
