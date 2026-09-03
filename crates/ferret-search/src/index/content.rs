//! The persistent content index.
//!
//! Tantivy owns exactly one job here: turning a literal query into a *ranked
//! list of paths* out of a corpus far too large to open file by file. It
//! deliberately does not own the snippet. Bodies are indexed but not stored,
//! and the small head of the ranking is re-read from disk with the same
//! `grep-searcher` the live path uses.
//!
//! That split buys three things at the cost of one file open per displayed
//! row. The index stays roughly a third the size of a stored-body index. Line
//! and column numbers are real byte offsets into the file as it exists *now*,
//! not as it was when indexed. And an indexed row and a live row are built by
//! the same code, so they carry identical [`crate::MatchKey`]s and the merge
//! can deduplicate them without knowing which engine produced either.
//!
//! The last one is the load-bearing property: a snippet generated from a
//! stored copy of the body would disagree with a live snippet of the same
//! match in exactly the cases that matter — a file edited since it was
//! indexed — and the merge would show the row twice.

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, Value as _};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term, doc,
};

use crate::config::{CONTENT_COMMIT_BATCH, CONTENT_WRITER_HEAP};
use crate::{Result, SearchError};

/// The document shape. Part of the on-disk contract: changing it means the
/// stored index no longer opens, which is why [`super::db::SCHEMA_VERSION`]
/// covers both stores together.
#[derive(Clone, Copy)]
pub(crate) struct Fields {
    /// The exact path, as an untokenized term. Both the identity used to
    /// delete a stale document and the value read back for a hit.
    path: Field,
    /// The filename, tokenized. Indexed alongside the body so that a query
    /// matching the name ranks above one matching only a passing mention in a
    /// large file.
    name: Field,
    /// The body. Indexed, never stored — see the module comment.
    body: Field,
}

pub(crate) struct ContentIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
}

/// One content hit, in the order the index ranked it.
pub(crate) struct RankedPath {
    pub path: String,
    /// Zero-based position in the ranking, not the raw BM25 score. The score
    /// is meaningless across queries and would make [`crate::SearchResult`]
    /// carry a float it could not compare; the position is what the UI
    /// actually shows.
    pub rank: u32,
}

impl ContentIndex {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|error| SearchError::IndexOpen {
            path: ferret_vfs::VfsPath::from(dir),
            message: error.to_string(),
        })?;
        let mut schema = Schema::builder();
        let fields = Fields {
            path: schema.add_text_field("path", STRING | STORED),
            name: schema.add_text_field("name", TEXT),
            body: schema.add_text_field("body", TEXT),
        };
        let directory =
            tantivy::directory::MmapDirectory::open(dir).map_err(|error| open_error(dir, error))?;
        let index = Index::open_or_create(directory, schema.build())
            .map_err(|error| open_error(dir, error))?;
        let reader = index
            .reader_builder()
            // The writer lives on the indexer thread and the reader on every
            // query thread. Reloading on commit is what makes a file indexed
            // seconds ago findable without the two threads coordinating.
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|error| open_error(dir, error))?;
        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    pub(crate) fn writer(&self) -> Result<ContentWriter> {
        let writer = self
            .index
            .writer::<TantivyDocument>(CONTENT_WRITER_HEAP)
            .map_err(write_error)?;
        Ok(ContentWriter {
            writer,
            fields: self.fields,
            uncommitted: 0,
        })
    }

    /// Makes everything committed so far visible to queries.
    ///
    /// The reload policy already does this on its own schedule, but "on its
    /// own schedule" is not a contract a caller can hold: a file indexed a
    /// moment ago has to be findable now, and a test that crawls and then
    /// searches would otherwise be racing the policy's timer.
    pub(crate) fn reload(&self) {
        let _ = self.reader.reload();
    }

    pub(crate) fn document_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Whether the tokenizer can represent this literal without losing it.
    ///
    /// Punctuation-only literals parse to an empty Tantivy query. They still
    /// have perfectly useful live-search semantics, so the planner must not
    /// classify them as fully covered by the index.
    pub(crate) fn supports_literal(&self, literal: &str) -> bool {
        self.parse_literal(literal)
            .is_ok_and(|query| query.is_some())
    }

    /// Prepares a stable ranked search snapshot.
    ///
    /// The query is always a phrase: quoting it means a single word becomes a
    /// term query and a multi-word literal becomes a phrase query, which is
    /// what a person typing `fn parse_query` into a search box means. Anything
    /// that is not a plain literal — a regex, a whole-word toggle the
    /// tokenizer cannot express — is not sent here at all; the planner routes
    /// it to the live scan.
    pub(crate) fn prepare_search(&self, literal: &str) -> Result<Option<ContentSearch>> {
        let Some(query) = self.parse_literal(literal)? else {
            return Ok(None);
        };
        Ok(Some(ContentSearch {
            searcher: self.reader.searcher(),
            query,
            path_field: self.fields.path,
        }))
    }

    fn parse_literal(&self, literal: &str) -> Result<Option<Box<dyn Query>>> {
        let parser = QueryParser::for_index(&self.index, vec![self.fields.body, self.fields.name]);
        let escaped = literal.replace('\\', "\\\\").replace('"', "\\\"");
        let query = parser
            .parse_query(&format!("\"{escaped}\""))
            .map_err(|error| SearchError::IndexQuery {
                message: error.to_string(),
            })?;
        let mut has_terms = false;
        query.query_terms(&mut |_term, _position_needed| has_terms = true);
        Ok(has_terms.then_some(query))
    }
}

/// One immutable reader snapshot and parsed query, paged in rank order.
pub(crate) struct ContentSearch {
    searcher: Searcher,
    query: Box<dyn Query>,
    path_field: Field,
}

impl ContentSearch {
    pub(crate) fn page(&self, offset: usize, limit: usize) -> Result<RankedPage> {
        debug_assert!(limit > 0);
        let hits = self
            .searcher
            .search(
                &self.query,
                &TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_score(),
            )
            .map_err(|error| SearchError::IndexQuery {
                message: error.to_string(),
            })?;
        let hit_count = hits.len();
        let mut paths = Vec::with_capacity(hit_count);
        for (rank, (_score, address)) in hits.into_iter().enumerate() {
            let Ok(document) = self.searcher.doc::<TantivyDocument>(address) else {
                continue;
            };
            let Some(path) = document
                .get_first(self.path_field)
                .and_then(|value| value.as_str())
            else {
                continue;
            };
            paths.push(RankedPath {
                path: path.to_owned(),
                rank: offset.saturating_add(rank).min(u32::MAX as usize) as u32,
            });
        }
        Ok(RankedPage {
            paths,
            next_offset: offset.saturating_add(hit_count),
            exhausted: hit_count < limit,
        })
    }
}

pub(crate) struct RankedPage {
    pub paths: Vec<RankedPath>,
    pub next_offset: usize,
    pub exhausted: bool,
}

pub(crate) struct ContentWriter {
    writer: IndexWriter<TantivyDocument>,
    fields: Fields,
    uncommitted: usize,
}

impl ContentWriter {
    pub(crate) fn add(&mut self, path: &str, name: &str, body: &str) -> Result<()> {
        self.writer
            .add_document(doc!(
                self.fields.path => path,
                self.fields.name => name,
                self.fields.body => body,
            ))
            .map_err(write_error)?;
        self.uncommitted += 1;
        Ok(())
    }

    /// Removes a path's document.
    ///
    /// Deleting before adding is how a changed file is replaced: Tantivy has
    /// no update, so an add without the delete leaves two documents for one
    /// path and the same file appears twice in every later result.
    pub(crate) fn delete(&mut self, path: &str) {
        self.writer
            .delete_term(Term::from_field_text(self.fields.path, path));
        self.uncommitted += 1;
    }

    pub(crate) fn commit_if_due(&mut self) -> Result<bool> {
        if self.uncommitted < CONTENT_COMMIT_BATCH {
            return Ok(false);
        }
        self.commit().map(|()| true)
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        if self.uncommitted == 0 {
            return Ok(());
        }
        self.writer.commit().map_err(write_error)?;
        self.uncommitted = 0;
        Ok(())
    }
}

fn open_error(dir: &Path, error: impl std::fmt::Display) -> SearchError {
    SearchError::IndexOpen {
        path: ferret_vfs::VfsPath::from(dir),
        message: error.to_string(),
    }
}

fn write_error(error: impl std::fmt::Display) -> SearchError {
    SearchError::IndexWrite {
        message: error.to_string(),
    }
}

/// Whether a file's bytes should enter the content index.
///
/// A null byte in the first block is ripgrep's own binary heuristic and it is
/// the right one here: an index of a binary's byte soup is large, useless and
/// slow to search. Invalid UTF-8 that is *not* binary — Latin-1 source, a
/// UTF-16 file — is indexed lossily rather than skipped, because a lossy
/// index still finds the file and the live path still produces the exact
/// snippet.
pub(crate) fn indexable_text(bytes: &[u8]) -> Option<String> {
    const BINARY_SNIFF_BYTES: usize = 8 * 1024;
    if bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("ferret-content-test-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn punctuation_only_literals_require_live_search() {
        let dir = TestDir::new();
        let index = ContentIndex::open(&dir.0).unwrap();

        assert!(index.supports_literal("searchable words"));
        assert!(!index.supports_literal("... ??? :::"));
    }

    #[test]
    fn ranked_pages_continue_with_global_ordinals() {
        let dir = TestDir::new();
        let index = ContentIndex::open(&dir.0).unwrap();
        let mut writer = index.writer().unwrap();
        for ordinal in 0..5 {
            writer
                .add(
                    &format!("/tmp/document-{ordinal}"),
                    &format!("document-{ordinal}"),
                    "common search term",
                )
                .unwrap();
        }
        writer.commit().unwrap();
        index.reload();

        let search = index.prepare_search("common").unwrap().unwrap();
        let first = search.page(0, 2).unwrap();
        let second = search.page(first.next_offset, 2).unwrap();
        let third = search.page(second.next_offset, 2).unwrap();

        assert_eq!(
            first.paths.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            second.paths.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            third.paths.iter().map(|hit| hit.rank).collect::<Vec<_>>(),
            vec![4]
        );
        assert!(!first.exhausted);
        assert!(!second.exhausted);
        assert!(third.exhausted);
    }
}
