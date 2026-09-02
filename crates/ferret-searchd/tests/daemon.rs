//! The one test that runs the real thing: a real socket, two real processes.
//!
//! Everything else about the protocol is unit-tested in `ferret-search` against
//! in-memory buffers, which is where the *disagreement* cases belong. What only
//! a live daemon can prove is the part no buffer models — that a client which
//! finds nothing listening launches the helper, waits for it to bind, completes
//! the handshake, and gets indexed results back.
//!
//! It is a single `#[test]` on purpose. The whole thing mutates process
//! environment, which two tests running in parallel in one binary cannot
//! safely share.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferret_search::{
    FerretSearchService, NameQuery, RootState, SearchEvent, SearchFilters, SearchGeneration,
    SearchLimits, SearchRequest, SearchResult, SearchScope, SearchScopeKind, SearchService as _,
    SearchSession,
};

/// How long the daemon is given to finish crawling a handful of files.
///
/// Generous because a cold start also builds two empty stores; the test polls,
/// so a fast machine does not wait for it.
const CRAWL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a watcher event is given to reach the stores.
///
/// Well above the indexer's debounce plus its idle poll, because what is being
/// asserted is that the path exists at all — not how quickly.
const WATCH_TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn a_launched_daemon_indexes_a_tree_and_answers_over_the_socket() {
    let fixture = Fixture::new();
    fixture.write("alpha/report-draft.md", "nothing in particular\n");
    fixture.write("beta/report-final.md", "the quick brown fox\n");
    fixture.write("beta/unrelated.txt", "no match here\n");

    // Everything the daemon and the client agree on comes from these, and
    // between them they keep the test off the developer's real socket, real
    // index and real home directory.
    //
    // SAFETY: single-threaded, before any other thread in this binary exists;
    // the test is alone in its own integration-test process.
    unsafe {
        std::env::set_var("FERRET_SEARCHD", env!("CARGO_BIN_EXE_ferret-searchd"));
        std::env::set_var("FERRET_SEARCH_RUNTIME_DIR", &fixture.runtime);
        std::env::set_var("FERRET_SEARCH_INDEX_DIR", &fixture.index);
        std::env::set_var("FERRET_SEARCH_ROOTS", &fixture.root);
    }

    let service = FerretSearchService::new();

    // The first call is what launches the helper: nothing is listening yet.
    let status = wait_for_crawl(&service);
    assert!(status.available, "the daemon reported no usable index");
    assert!(
        status.files >= 3,
        "expected the fixture's files to be indexed, got {}",
        status.files
    );

    let by_name = collect(service.start(request(&fixture.root, "report")).unwrap());
    assert_eq!(
        names(&by_name),
        vec!["report-draft.md", "report-final.md"],
        "filename search over the socket"
    );

    let mut content = request(&fixture.root, "");
    content.content = Some(ferret_search::ContentQuery {
        pattern: "quick brown".into(),
        syntax: ferret_search::PatternSyntax::Literal,
        case: ferret_search::CaseMode::Smart,
        whole_word: false,
    });
    let by_content = collect(service.start(content).unwrap());
    assert_eq!(
        names(&by_content),
        vec!["report-final.md"],
        "content search over the socket"
    );
    assert!(
        matches!(
            &by_content[0].matched,
            ferret_search::SearchMatch::Content { line: 1, .. }
        ),
        "the snippet is read from the file: {:?}",
        by_content[0].matched
    );

    // A file created after the crawl has to become findable on its own. This
    // is the whole of the freshness contract as a user experiences it: nobody
    // asks Ferret to re-index, so a watcher that does not reach the stores is
    // indistinguishable from no index at all.
    fixture.write("gamma/created-after-the-crawl.md", "later\n");
    let appeared = wait_for_name(&service, &fixture.root, "created-after-the-crawl");
    assert!(
        appeared,
        "the watcher never made a newly created file searchable"
    );

    assert!(
        service.unavailable_reason().is_none(),
        "the client should not have given up on a daemon it just used"
    );

    // Otherwise it would sit on its idle timer for half an hour holding a
    // socket and an index inside a directory this test is about to delete.
    assert!(service.stop_daemon(), "the daemon accepted a shutdown");
    wait_for_exit(&fixture.runtime);
}

/// Polls the indexed search until a name shows up, or gives up.
///
/// Polling rather than sleeping for the debounce: the interval that has to be
/// waited out is the watcher's, and a test that hard-codes it would be
/// asserting the constant rather than the behaviour.
fn wait_for_name(service: &FerretSearchService, root: &Path, name: &str) -> bool {
    let deadline = Instant::now() + WATCH_TIMEOUT;
    while Instant::now() < deadline {
        if !collect(service.start(request(root, name)).unwrap()).is_empty() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// Waits for the daemon to release the socket, so the fixture can be removed.
fn wait_for_exit(runtime: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // The listener reclaims its name on drop, so the socket file
        // disappearing is the daemon having actually exited rather than
        // merely having been asked to.
        if !runtime.join("searchd.sock").exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Polls the daemon until every root has finished its first crawl.
fn wait_for_crawl(service: &FerretSearchService) -> ferret_search::IndexStatus {
    let deadline = Instant::now() + CRAWL_TIMEOUT;
    let mut last = ferret_search::IndexStatus::default();
    while Instant::now() < deadline {
        if let Some(status) = service.index_status() {
            let ready = !status.roots.is_empty()
                && status
                    .roots
                    .iter()
                    .all(|root| root.state == RootState::Ready);
            last = status;
            if ready && last.files >= 3 {
                return last;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("the daemon did not finish crawling in {CRAWL_TIMEOUT:?}; last status: {last:?}");
}

fn request(root: &Path, name: &str) -> SearchRequest {
    SearchRequest {
        generation: SearchGeneration(1),
        scope: SearchScope {
            kind: SearchScopeKind::CurrentFolder,
            roots: vec![ferret_vfs::VfsPath::from(root)],
        },
        name: NameQuery::substring(name),
        content: None,
        filters: SearchFilters::default(),
        limits: SearchLimits::default(),
    }
}

fn collect(session: SearchSession) -> Vec<SearchResult> {
    let mut results = Vec::new();
    while let Some(event) = session.next_blocking() {
        match event {
            SearchEvent::Batch(batch) => results.extend(batch.results),
            SearchEvent::Finished(finished) => {
                assert!(
                    !matches!(finished, ferret_search::SearchFinished::Failed { .. }),
                    "{finished:?}"
                );
                break;
            }
            _ => {}
        }
    }
    results
}

fn names(results: &[SearchResult]) -> Vec<String> {
    let mut names: Vec<String> = results.iter().map(|r| r.name.to_string()).collect();
    names.sort();
    names.dedup();
    names
}

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    index: PathBuf,
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "ferret-searchd-e2e-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fixture = Self {
            root: base.join("tree"),
            index: base.join("index"),
            runtime: base.join("run"),
            base,
        };
        std::fs::create_dir_all(&fixture.root).unwrap();
        fixture
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The daemon exits on its own idle timer; removing the socket it is
        // listening on is enough to keep the developer's temp directory clean
        // without the test having to manage another process's lifetime.
        let _ = std::fs::remove_dir_all(&self.base);
    }
}
