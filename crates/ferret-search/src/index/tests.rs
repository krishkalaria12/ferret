//! Index behaviour that has to survive the filesystem moving underneath it.
//!
//! These run against real temporary trees rather than a fake: the whole point
//! of the index is that it agrees with a disk it does not control, and a mock
//! disk agrees with itself by construction.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::{
    CaseMode, ContentQuery, EntryKindFilter, HybridSearchService, MatchKey, NameQuery,
    PatternSyntax, ResultSource, SearchEvent, SearchFilters, SearchGeneration, SearchLimits,
    SearchMatch, SearchRequest, SearchResult, SearchScope, SearchScopeKind, SearchService,
    SearchSession,
};

/// A crawl root and an index directory that is *not* inside it.
///
/// Separate on purpose: an index living inside its own crawl root is the
/// feedback loop [`is_excluded`] exists to prevent, and a test that put them
/// together would be testing that guard rather than the behaviour it is about.
struct Fixture {
    root: PathBuf,
    index: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "ferret-index-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("tree");
        let index = base.join("store");
        fs::create_dir_all(&root).unwrap();
        Self { root, index }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    fn open(&self) -> Index {
        Index::open(&self.index).unwrap()
    }

    fn crawled(&self) -> Index {
        let index = self.open();
        crawl_once(&index, std::slice::from_ref(&self.root)).unwrap();
        index
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = fs::remove_dir_all(base);
        }
    }
}

fn request(root: &Path, name: &str) -> SearchRequest {
    SearchRequest {
        generation: SearchGeneration(7),
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

fn content_request(root: &Path, pattern: &str, whole_word: bool) -> SearchRequest {
    let mut request = request(root, "");
    request.content = Some(ContentQuery {
        pattern: pattern.into(),
        syntax: PatternSyntax::Literal,
        case: CaseMode::Smart,
        whole_word,
    });
    request
}

fn results(session: SearchSession) -> Vec<SearchResult> {
    let mut collected = Vec::new();
    while let Some(event) = session.next_blocking() {
        match event {
            SearchEvent::Batch(batch) => collected.extend(batch.results),
            SearchEvent::Finished(_) => break,
            _ => {}
        }
    }
    collected
}

fn names(results: &[SearchResult]) -> Vec<String> {
    let mut names: Vec<String> = results.iter().map(|r| r.name.to_string()).collect();
    names.sort();
    names
}

fn apply_change(index: &Index, root_path: &Path, changed_path: &Path) {
    let db = Db::open_writer(&index.db_path).unwrap();
    let root = register(&db, &[root_path.to_path_buf()]).unwrap().remove(0);
    let mut content = index.content().writer().unwrap();
    let stop = AtomicBool::new(false);
    crawl::apply_change(&db, &mut content, &root, changed_path, &stop).unwrap();
    content.commit().unwrap();
    index.content().reload();
}

#[test]
fn a_crawl_makes_names_findable_without_touching_the_tree() {
    let fixture = Fixture::new("names");
    fixture.write("alpha/needle.txt", "x");
    fixture.write("beta/haystack.txt", "x");
    fixture.write("beta/needle-two.md", "x");
    let index = fixture.crawled();

    let found = results(
        IndexSearchService::new(Arc::new(index))
            .start(request(&fixture.root, "needle"))
            .unwrap(),
    );

    assert_eq!(names(&found), vec!["needle-two.md", "needle.txt"]);
    assert!(
        found.iter().all(|r| r.source == ResultSource::Index),
        "the index engine must label its own rows"
    );
}

#[test]
fn a_pattern_too_short_for_a_trigram_still_answers() {
    // FTS5's trigram tokenizer has nothing to look up below three characters.
    // The fallback is a bounded `LIKE` over the scope, and it has to produce
    // the same answer rather than silently none.
    let fixture = Fixture::new("short");
    fixture.write("zz.txt", "x");
    fixture.write("other.txt", "x");
    let index = fixture.crawled();

    let found = results(
        IndexSearchService::new(Arc::new(index))
            .start(request(&fixture.root, "zz"))
            .unwrap(),
    );

    assert_eq!(names(&found), vec!["zz.txt"]);
}

#[test]
fn a_file_deleted_since_the_crawl_is_not_offered() {
    // The row is still in SQLite: nothing has reconciled yet. It must not
    // reach the UI, because a result that cannot be opened is worse than a
    // slower search.
    let fixture = Fixture::new("stale");
    let doomed = fixture.write("ghost.txt", "x");
    fixture.write("real.txt", "x");
    let index = fixture.crawled();
    fs::remove_file(&doomed).unwrap();

    let found = results(
        IndexSearchService::new(Arc::new(index))
            .start(request(&fixture.root, ".txt"))
            .unwrap(),
    );

    assert_eq!(names(&found), vec!["real.txt"]);
}

#[test]
fn indexed_results_use_the_current_entry_kind() {
    let fixture = Fixture::new("kind-change");
    let path = fixture.write("changed.txt", "x");
    let index = Arc::new(fixture.crawled());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();

    let mut files = request(&fixture.root, "changed");
    files.filters.kind = EntryKindFilter::Files;
    let file_results = results(
        IndexSearchService::new(Arc::clone(&index))
            .start(files)
            .unwrap(),
    );
    assert!(file_results.is_empty());

    let mut folders = request(&fixture.root, "changed");
    folders.filters.kind = EntryKindFilter::Folders;
    let folder_results = results(
        IndexSearchService::new(Arc::clone(&index))
            .start(folders)
            .unwrap(),
    );
    assert_eq!(names(&folder_results), vec!["changed.txt"]);
}

#[test]
fn reconciliation_removes_what_a_missed_event_left_behind() {
    let fixture = Fixture::new("reconcile");
    let doomed = fixture.write("ghost.txt", "x");
    let index = fixture.crawled();
    fs::remove_file(&doomed).unwrap();

    // No watcher involved: this is the sweep that has to converge on its own.
    crawl_once(&index, std::slice::from_ref(&fixture.root)).unwrap();

    let db = index.reader().unwrap();
    assert!(
        db.file(doomed.to_str().unwrap()).unwrap().is_none(),
        "a swept root must not keep a row for a path it no longer has"
    );
}

#[test]
fn content_search_reports_the_line_the_match_is_on() {
    let fixture = Fixture::new("content");
    fixture.write("src/one.rs", "fn first() {}\nfn parse_query() {}\n");
    fixture.write("src/two.rs", "nothing here\n");
    let index = fixture.crawled();

    let found = results(
        IndexSearchService::new(Arc::new(index))
            .start(content_request(&fixture.root, "parse_query", false))
            .unwrap(),
    );

    assert_eq!(found.len(), 1, "{found:#?}");
    let SearchMatch::Content {
        line,
        column,
        preview,
        ranges,
    } = &found[0].matched
    else {
        panic!("expected a content match, got {:?}", found[0].matched);
    };
    assert_eq!(*line, 2, "the snippet is read from the file, not the index");
    assert_eq!(*column, 4);
    assert_eq!(preview.as_str(), "fn parse_query() {}");
    assert_eq!(ranges.len(), 1);
    assert!(found[0].rank.is_some(), "indexed rows carry their ranking");
}

#[test]
fn a_body_edited_since_indexing_does_not_produce_a_phantom_match() {
    // Tantivy still has the old document, so the path is still a candidate.
    // The snippet pass re-reads the file, finds nothing, and the row drops —
    // which is the index correcting itself at the only moment it matters.
    let fixture = Fixture::new("phantom");
    let path = fixture.write("notes.txt", "the quick brown fox\n");
    let index = fixture.crawled();
    fs::write(&path, "nothing of the sort\n").unwrap();

    let found = results(
        IndexSearchService::new(Arc::new(index))
            .start(content_request(&fixture.root, "quick", false))
            .unwrap(),
    );

    assert!(found.is_empty(), "{found:#?}");
}

#[test]
fn the_index_declines_a_query_its_crawl_policy_cannot_answer() {
    let fixture = Fixture::new("policy");
    fixture.write("visible.txt", "x");
    let index = Arc::new(fixture.crawled());
    let service = IndexSearchService::new(Arc::clone(&index));

    let mut hidden = request(&fixture.root, "txt");
    hidden.filters.include_hidden = true;

    assert_eq!(
        service.plan(&request(&fixture.root, "txt")),
        IndexPlan::Covered
    );
    assert_eq!(
        service.plan(&hidden),
        IndexPlan::None,
        "a corpus that excluded hidden entries cannot be asked about them"
    );

    let mut same_filesystem = request(&fixture.root, "txt");
    same_filesystem.filters.same_filesystem = true;
    assert_eq!(
        service.plan(&same_filesystem),
        IndexPlan::None,
        "the index does not record filesystem traversal boundaries"
    );
}

#[test]
fn watcher_updates_do_not_insert_hidden_files() {
    let fixture = Fixture::new("watch-hidden");
    let index = fixture.crawled();
    let hidden = fixture.write(".secret.txt", "not searchable");

    apply_change(&index, &fixture.root, &hidden);

    assert!(
        index
            .reader()
            .unwrap()
            .file(hidden.to_str().unwrap())
            .unwrap()
            .is_none(),
        "incremental updates must use the crawl's hidden-file policy"
    );
}

#[test]
fn changing_gitignore_reapplies_policy_to_the_governed_subtree() {
    let fixture = Fixture::new("watch-ignore");
    let ignored = fixture.write("build/output.txt", "was initially visible");
    let index = fixture.crawled();
    assert!(
        index
            .reader()
            .unwrap()
            .file(ignored.to_str().unwrap())
            .unwrap()
            .is_some()
    );

    let ignore_file = fixture.write(".gitignore", "build/\n");
    apply_change(&index, &fixture.root, &ignore_file);

    assert!(
        index
            .reader()
            .unwrap()
            .file(ignored.to_str().unwrap())
            .unwrap()
            .is_none(),
        "a changed ignore file must evict rows it now excludes"
    );
}

#[test]
fn deleting_gitignore_reincludes_the_governed_subtree() {
    let fixture = Fixture::new("watch-ignore-delete");
    let ignore_file = fixture.write(".gitignore", "build/\n");
    let previously_ignored = fixture.write("build/output.txt", "searchable again");
    let index = fixture.crawled();
    assert!(
        index
            .reader()
            .unwrap()
            .file(previously_ignored.to_str().unwrap())
            .unwrap()
            .is_none()
    );

    std::fs::remove_file(&ignore_file).unwrap();
    apply_change(&index, &fixture.root, &ignore_file);

    assert!(
        index
            .reader()
            .unwrap()
            .file(previously_ignored.to_str().unwrap())
            .unwrap()
            .is_some(),
        "removing an ignore file must immediately restore newly visible rows"
    );
}

#[test]
fn sibling_watcher_changes_share_one_reindex_scope() {
    let fixture = Fixture::new("watch-sibling-batch");
    let first = fixture.write("src/first.txt", "first");
    let second = fixture.write("src/second.txt", "second");
    let root = fixture.root.clone();

    let mut scopes = [first, second]
        .iter()
        .filter_map(|path| crawl::reindex_scope(&root, path))
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();

    assert_eq!(scopes, vec![root.join("src")]);
}

#[test]
fn watcher_updates_inside_an_ignored_directory_stay_excluded() {
    let fixture = Fixture::new("watch-ignored-child");
    fixture.write(".gitignore", "ignored/\n");
    let index = fixture.crawled();
    let ignored = fixture.write("ignored/new.txt", "not searchable");

    apply_change(&index, &fixture.root, &ignored);

    assert!(
        index
            .reader()
            .unwrap()
            .file(ignored.to_str().unwrap())
            .unwrap()
            .is_none(),
        "starting an incremental walk inside an ignored tree must not bypass ancestor rules"
    );
}

#[test]
fn subtree_reindex_keeps_depth_relative_to_the_indexed_root() {
    let fixture = Fixture::new("subtree-depth");
    let nested = fixture.write("one/two/three.txt", "x");
    let index = fixture.crawled();
    let db = Db::open_writer(&index.db_path).unwrap();
    let root = register(&db, std::slice::from_ref(&fixture.root))
        .unwrap()
        .remove(0);
    let mut content = index.content().writer().unwrap();
    let stop = AtomicBool::new(false);

    crawl::reindex_subtree(
        &db,
        &mut content,
        &root,
        &fixture.root.join("one/two"),
        &stop,
    )
    .unwrap();

    let row = db.file(nested.to_str().unwrap()).unwrap().unwrap();
    assert_eq!(row.depth, 3, "depth is measured from the indexed root");
}

#[test]
fn a_content_query_the_index_cannot_express_still_runs_both_engines_once() {
    // Whole-word is a matcher semantic the tokenized index cannot express, so
    // the planner runs the live scan alongside. Both engines find the same
    // match in the same file at the same offset; exactly one row must come
    // out, and it is the deduplication contract that guarantees it.
    let fixture = Fixture::new("dedup");
    fixture.write("src/lib.rs", "let total = count + 1;\nlet counted = 2;\n");
    let index = Arc::new(fixture.crawled());

    assert_eq!(
        IndexSearchService::new(Arc::clone(&index)).plan(&content_request(
            &fixture.root,
            "count",
            true
        )),
        IndexPlan::Partial
    );

    let found = results(
        HybridSearchService::new(Some(index))
            .start(content_request(&fixture.root, "count", true))
            .unwrap(),
    );

    assert_eq!(found.len(), 1, "one match, one row: {found:#?}");
    assert!(matches!(
        found[0].id.match_key,
        MatchKey::Content { line: 1, .. }
    ));
}

#[test]
fn a_hybrid_with_no_index_is_still_a_working_search() {
    let fixture = Fixture::new("fallback");
    fixture.write("findme.txt", "x");

    let found = results(
        HybridSearchService::new(None)
            .start(request(&fixture.root, "findme"))
            .unwrap(),
    );

    assert_eq!(names(&found), vec!["findme.txt"]);
    assert!(found.iter().all(|r| r.source == ResultSource::Live));
}

#[test]
fn a_subtree_range_covers_children_and_excludes_similar_siblings() {
    let separator = std::path::MAIN_SEPARATOR;
    let (lower, upper) = super::db::subtree_range(&format!("{separator}foo{separator}bar"));

    let child = format!("{separator}foo{separator}bar{separator}baz");
    let sibling = format!("{separator}foo{separator}barn{separator}baz");
    assert!(child >= lower && child < upper);
    assert!(
        !(sibling >= lower && sibling < upper),
        "a name that merely starts with the same text is not inside it"
    );
}
