//! The persistent path index: SQLite plus an FTS5 trigram view of it.
//!
//! Every column here exists because a query or a filter reads it. `name_lower`
//! is the one apparent duplicate, and it is what lets a two-character pattern —
//! too short for a trigram — be answered by a `LIKE` that the collation cannot
//! turn into a full scan of the wrong rows.
//!
//! Two connection roles, never one shared handle: the indexer holds a single
//! writer, and every query opens its own read-only connection. WAL is what
//! makes that free — readers do not block the writer and the writer does not
//! block readers, so a crawl in progress never delays a keystroke.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::{Result, RootState, SearchError};

/// The shape of the stores this build understands.
///
/// Not a tunable: it is compared on open, and a mismatch quarantines and
/// rebuilds rather than migrating, because every row is reconstructible from
/// the filesystem in minutes and a half-migrated index is not detectable.
pub(crate) const SCHEMA_VERSION: u32 = 1;

pub(crate) struct Db {
    conn: Connection,
}

/// One indexed filesystem entry.
#[derive(Clone, Debug)]
pub(crate) struct FileRow {
    pub path: String,
    pub name: String,
    pub ext: Option<String>,
    pub kind: i64,
    pub size: i64,
    /// Whole seconds since the Unix epoch. Sub-second resolution is not worth
    /// eight more bytes a row: the date column shows minutes, and the
    /// fingerprint that decides re-indexing carries the precise value.
    pub mtime: Option<i64>,
    pub hidden: bool,
    pub depth: i64,
    /// `size:mtime_nanos`. Two files with the same fingerprint are treated as
    /// unchanged; a content hash would be correct more often and would cost a
    /// full read of every file on every reconciliation, which is the thing
    /// reconciliation exists to avoid.
    pub fingerprint: String,
}

/// Whether a file's body is in the content index.
pub(crate) mod content_state {
    /// Not attempted yet.
    pub(crate) const PENDING: i64 = 0;
    /// Present in Tantivy and current as of the stored fingerprint.
    pub(crate) const INDEXED: i64 = 1;
    /// Deliberately not indexed — binary, too large, or unreadable. Recorded
    /// so a crawl does not reopen it on every pass.
    pub(crate) const SKIPPED: i64 = 2;
}

/// [`FileRow::kind`] discriminants. Part of the on-disk contract, so these are
/// written out rather than derived from `EntryKind`'s declaration order.
pub(crate) mod kind {
    pub(crate) const FILE: i64 = 0;
    pub(crate) const DIR: i64 = 1;
    pub(crate) const SYMLINK_FILE: i64 = 2;
    pub(crate) const SYMLINK_DIR: i64 = 3;
    pub(crate) const SYMLINK_BROKEN: i64 = 4;
    pub(crate) const OTHER: i64 = 5;
}

#[derive(Clone, Debug)]
pub(crate) struct RootRow {
    pub id: i64,
    pub path: String,
    pub state: RootState,
    pub last_reconciled: Option<i64>,
    pub files: u64,
}

impl Db {
    pub(crate) fn open_writer(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|error| open_error(path, error))?;
        let db = Self { conn };
        db.configure()?;
        db.migrate(path)?;
        Ok(db)
    }

    /// A read-only handle for one query.
    ///
    /// `SQLITE_OPEN_READ_ONLY` is not a nicety: it is what guarantees that a
    /// query thread can never take a write lock and stall the indexer, no
    /// matter what statement a future query builder emits.
    pub(crate) fn open_reader(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| open_error(path, error))?;
        let db = Self { conn };
        db.configure()?;
        Ok(db)
    }

    fn configure(&self) -> Result<()> {
        // WAL survives across connections and is only settable by a writer, so
        // a read-only open finds it already set and this is a no-op there.
        let _ = self.conn.pragma_update(None, "journal_mode", "WAL");
        let _ = self.conn.pragma_update(None, "synchronous", "NORMAL");
        // A reader that arrives during a commit should wait briefly rather
        // than fail the query outright.
        self.conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(write_error)
    }

    fn migrate(&self, path: &Path) -> Result<()> {
        let found: Option<u32> = self
            .conn
            .query_row(
                "SELECT value FROM sqlite_master
                 JOIN meta ON meta.key = 'schema_version'
                 WHERE sqlite_master.name = 'meta' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok());
        if let Some(found) = found
            && found != SCHEMA_VERSION
        {
            return Err(SearchError::IndexVersion {
                expected: SCHEMA_VERSION,
                found,
            });
        }
        self.conn
            .execute_batch(SCHEMA)
            .map_err(|error| SearchError::IndexOpen {
                path: ferret_vfs::VfsPath::from(path),
                message: error.to_string(),
            })?;
        self.conn
            .execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES('schema_version', ?1)",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(write_error)?;
        Ok(())
    }

    // ---- roots ---------------------------------------------------------

    pub(crate) fn upsert_root(&self, path: &str) -> Result<RootRow> {
        self.conn
            .execute(
                "INSERT INTO roots(path, state, generation) VALUES(?1, 'crawling', 0)
                 ON CONFLICT(path) DO NOTHING",
                params![path],
            )
            .map_err(write_error)?;
        self.root(path)?.ok_or_else(|| SearchError::IndexWrite {
            message: format!("the root {path} disappeared as it was being added"),
        })
    }

    pub(crate) fn root(&self, path: &str) -> Result<Option<RootRow>> {
        self.conn
            .query_row(
                "SELECT id, path, state, last_reconciled FROM roots WHERE path = ?1",
                params![path],
                root_row,
            )
            .optional()
            .map_err(query_error)
    }

    pub(crate) fn roots(&self) -> Result<Vec<RootRow>> {
        let mut statement = self
            .conn
            .prepare("SELECT id, path, state, last_reconciled FROM roots ORDER BY path")
            .map_err(query_error)?;
        let mut roots = statement
            .query_map([], root_row)
            .map_err(query_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)?;
        drop(statement);
        for root in &mut roots {
            root.files = self.file_count(root.id)?;
        }
        Ok(roots)
    }

    pub(crate) fn set_root_state(&self, id: i64, state: RootState) -> Result<()> {
        self.conn
            .execute(
                "UPDATE roots SET state = ?2 WHERE id = ?1",
                params![id, state_name(state)],
            )
            .map(|_| ())
            .map_err(write_error)
    }

    /// Starts a new sweep over a root and returns its generation stamp.
    ///
    /// Every row the sweep sees is stamped with it; the rows that still carry
    /// the previous stamp afterwards are exactly the ones the filesystem no
    /// longer has. That is the whole of reconciliation, and it is why a missed
    /// watcher event cannot permanently leave a deleted file in the index.
    pub(crate) fn begin_sweep(&self, id: i64) -> Result<i64> {
        self.conn
            .execute(
                "UPDATE roots SET generation = generation + 1 WHERE id = ?1",
                params![id],
            )
            .map_err(write_error)?;
        self.conn
            .query_row(
                "SELECT generation FROM roots WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(query_error)
    }

    /// Removes everything the sweep did not see, returning the paths so the
    /// content index can drop them too.
    pub(crate) fn finish_sweep(&self, id: i64, generation: i64) -> Result<Vec<String>> {
        let stale = {
            let mut statement = self
                .conn
                .prepare("SELECT path FROM files WHERE root_id = ?1 AND seen < ?2")
                .map_err(query_error)?;
            statement
                .query_map(params![id, generation], |row| row.get::<_, String>(0))
                .map_err(query_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(query_error)?
        };
        self.conn
            .execute(
                "DELETE FROM files WHERE root_id = ?1 AND seen < ?2",
                params![id, generation],
            )
            .map_err(write_error)?;
        self.conn
            .execute(
                "UPDATE roots SET last_reconciled = unixepoch() WHERE id = ?1",
                params![id],
            )
            .map_err(write_error)?;
        Ok(stale)
    }

    pub(crate) fn current_generation(&self, id: i64) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT generation FROM roots WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(query_error)
    }

    // ---- batching ------------------------------------------------------

    /// Opens an explicit transaction around a batch of crawl writes.
    ///
    /// Plain statements rather than `rusqlite::Transaction` because the batch
    /// spans a walker loop that also touches the content writer, and a
    /// borrowing guard would force the whole loop body into a closure for no
    /// safety this code does not already have: an unclosed batch is rolled
    /// back by SQLite when the connection drops.
    pub(crate) fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN").map_err(write_error)
    }

    pub(crate) fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT").map_err(write_error)
    }

    // ---- files ---------------------------------------------------------

    /// Inserts or refreshes one row, reporting whether its body needs
    /// re-indexing.
    ///
    /// The fingerprint comparison is the point: a reconciliation sweep visits
    /// every path in a root, and without it every sweep would re-read every
    /// file in the tree.
    pub(crate) fn upsert_file(
        &self,
        root_id: i64,
        generation: i64,
        row: &FileRow,
    ) -> Result<ContentWork> {
        let previous: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT fingerprint, content_state FROM files WHERE path = ?1",
                params![row.path],
                |sql| Ok((sql.get(0)?, sql.get(1)?)),
            )
            .optional()
            .map_err(query_error)?;
        let unchanged = previous
            .as_ref()
            .is_some_and(|(fingerprint, _)| *fingerprint == row.fingerprint);
        let content_state = if unchanged {
            previous
                .as_ref()
                .map(|(_, state)| *state)
                .unwrap_or(content_state::PENDING)
        } else {
            content_state::PENDING
        };
        self.conn
            .execute(
                "INSERT INTO files(
                     root_id, path, name, name_lower, ext, kind, size, mtime, hidden, depth,
                     fingerprint, content_state, seen)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(path) DO UPDATE SET
                     root_id = excluded.root_id,
                     name = excluded.name,
                     name_lower = excluded.name_lower,
                     ext = excluded.ext,
                     kind = excluded.kind,
                     size = excluded.size,
                     mtime = excluded.mtime,
                     hidden = excluded.hidden,
                     depth = excluded.depth,
                     fingerprint = excluded.fingerprint,
                     content_state = excluded.content_state,
                     seen = excluded.seen",
                params![
                    root_id,
                    row.path,
                    row.name,
                    row.name.to_lowercase(),
                    row.ext,
                    row.kind,
                    row.size,
                    row.mtime,
                    row.hidden as i64,
                    row.depth,
                    row.fingerprint,
                    content_state,
                    generation,
                ],
            )
            .map_err(write_error)?;
        Ok(match (previous.is_some(), unchanged, content_state) {
            (true, true, content_state::INDEXED | content_state::SKIPPED) => ContentWork::UpToDate,
            (true, _, _) => ContentWork::Replace,
            (false, _, _) => ContentWork::Add,
        })
    }

    pub(crate) fn mark_content_state(&self, path: &str, state: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE files SET content_state = ?2 WHERE path = ?1",
                params![path, state],
            )
            .map(|_| ())
            .map_err(write_error)
    }

    /// Deletes a path and everything beneath it, returning what was removed.
    pub(crate) fn delete_subtree(&self, path: &str) -> Result<Vec<String>> {
        let (lower, upper) = subtree_range(path);
        let removed = {
            let mut statement = self
                .conn
                .prepare("SELECT path FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)")
                .map_err(query_error)?;
            statement
                .query_map(params![path, lower, upper], |row| row.get::<_, String>(0))
                .map_err(query_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(query_error)?
        };
        self.conn
            .execute(
                "DELETE FROM files WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
                params![path, lower, upper],
            )
            .map_err(write_error)?;
        Ok(removed)
    }

    /// Every path the index holds beneath `dir`, excluding `dir` itself.
    pub(crate) fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let (lower, upper) = subtree_range(dir);
        let mut statement = self
            .conn
            .prepare("SELECT path FROM files WHERE path >= ?1 AND path < ?2")
            .map_err(query_error)?;
        let paths = statement
            .query_map(params![lower, upper], |row| row.get::<_, String>(0))
            .map_err(query_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)?;
        Ok(paths)
    }

    pub(crate) fn delete_path(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])
            .map(|_| ())
            .map_err(write_error)
    }

    pub(crate) fn file(&self, path: &str) -> Result<Option<FileRow>> {
        self.conn
            .query_row(FILE_SELECT_BY_PATH, params![path], file_row)
            .optional()
            .map_err(query_error)
    }

    pub(crate) fn file_count(&self, root_id: i64) -> Result<u64> {
        self.conn
            .query_row(
                "SELECT count(*) FROM files WHERE root_id = ?1",
                params![root_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(query_error)
    }

    pub(crate) fn total_files(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT count(*) FROM files", [], |row| row.get::<_, i64>(0))
            .map(|count| count as u64)
            .map_err(query_error)
    }

    /// Files whose bodies still need to reach the content index.
    pub(crate) fn pending_content(&self, root_id: i64, limit: usize) -> Result<Vec<FileRow>> {
        let mut statement = self
            .conn
            .prepare(&format!(
                "{FILE_SELECT} WHERE root_id = ?1 AND content_state = ?2 AND kind = ?3 LIMIT ?4"
            ))
            .map_err(query_error)?;
        let rows = statement
            .query_map(
                params![root_id, content_state::PENDING, kind::FILE, limit as i64],
                file_row,
            )
            .map_err(query_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)?;
        Ok(rows)
    }

    // ---- queries -------------------------------------------------------

    /// Filename candidates for one scope, using the trigram index when the
    /// pattern is long enough to have trigrams.
    ///
    /// "Candidates", not results: the caller re-applies the real matcher, the
    /// case rule and every metadata filter. The index's job is to turn tens of
    /// millions of paths into a few thousand, not to be right about all of
    /// them.
    pub(crate) fn candidate_page(
        &self,
        scope: &str,
        pattern: Option<&str>,
        match_path: bool,
        after_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileRow>> {
        let (lower, upper) = subtree_range(scope);
        // No descendant can equal `lower` (it ends in the separator), so it is
        // also the correct exclusive cursor for the first page.
        let cursor = after_path.unwrap_or(&lower);
        let Some(pattern) = pattern.filter(|pattern| !pattern.is_empty()) else {
            let mut statement = self
                .conn
                .prepare(&format!(
                    "{FILE_SELECT} WHERE path >= ?1 AND path < ?2 AND path > ?3
                     ORDER BY path LIMIT ?4"
                ))
                .map_err(query_error)?;
            let rows = statement
                .query_map(params![lower, upper, cursor, limit as i64], file_row)
                .map_err(query_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(query_error)?;
            return Ok(rows);
        };

        if pattern.chars().count() >= crate::config::MIN_TRIGRAM_CHARS {
            let column = if match_path { "path" } else { "name" };
            let match_expression = format!("{column} : \"{}\"", pattern.replace('"', "\"\""));
            let mut statement = self
                .conn
                .prepare(&format!(
                    "SELECT {FILE_COLUMNS} FROM files_fts
                     JOIN files ON files.id = files_fts.rowid
                     WHERE files_fts MATCH ?1 AND files.path >= ?2 AND files.path < ?3
                     AND files.path > ?4 ORDER BY files.path LIMIT ?5"
                ))
                .map_err(query_error)?;
            let rows = statement
                .query_map(
                    params![match_expression, lower, upper, cursor, limit as i64],
                    file_row,
                )
                .map_err(query_error)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(query_error)?;
            return Ok(rows);
        }

        // One or two characters have no trigram to look up. A bounded `LIKE`
        // over the scope's key range is still an index range scan, not a walk
        // of the disk.
        let needle = format!("%{}%", escape_like(&pattern.to_lowercase()));
        let column = if match_path { "path" } else { "name_lower" };
        let mut statement = self
            .conn
            .prepare(&format!(
                "{FILE_SELECT} WHERE path >= ?1 AND path < ?2
                 AND path > ?3 AND {column} LIKE ?4 ESCAPE '\\'
                 ORDER BY path LIMIT ?5"
            ))
            .map_err(query_error)?;
        let rows = statement
            .query_map(
                params![lower, upper, cursor, needle, limit as i64],
                file_row,
            )
            .map_err(query_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(query_error)?;
        Ok(rows)
    }
}

/// The half-open key range covering everything under `dir`.
///
/// Text ranges rather than `LIKE 'prefix%'` because only a range can use the
/// unique index on `path`. Incrementing the last character of the separator is
/// safe for both platforms' separators: UTF-8 byte order matches code point
/// order, which is what SQLite's default BINARY collation compares on.
pub(crate) fn subtree_range(dir: &str) -> (String, String) {
    let separator = std::path::MAIN_SEPARATOR;
    let mut lower = dir.to_owned();
    if !lower.ends_with(separator) {
        lower.push(separator);
    }
    let mut upper = lower.clone();
    if let Some(last) = upper.pop() {
        match char::from_u32(last as u32 + 1) {
            Some(next) => upper.push(next),
            // Unreachable for either platform separator; a lower bound with no
            // upper one is still correct, only slower.
            None => upper.push(last),
        }
    }
    (lower, upper)
}

fn escape_like(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for character in pattern.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// What [`Db::upsert_file`] leaves for the content index to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentWork {
    /// New path: add the document.
    Add,
    /// Known path whose fingerprint moved: delete the old document first.
    Replace,
    /// Known path, same fingerprint, already resolved. Do nothing.
    UpToDate,
}

pub(crate) fn state_name(state: RootState) -> &'static str {
    match state {
        RootState::Crawling => "crawling",
        RootState::Ready => "ready",
        RootState::Paused => "paused",
        RootState::Failed => "failed",
    }
}

fn state_from_name(name: &str) -> RootState {
    match name {
        "ready" => RootState::Ready,
        "paused" => RootState::Paused,
        "failed" => RootState::Failed,
        _ => RootState::Crawling,
    }
}

fn root_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RootRow> {
    Ok(RootRow {
        id: row.get(0)?,
        path: row.get(1)?,
        state: state_from_name(&row.get::<_, String>(2)?),
        last_reconciled: row.get(3)?,
        files: 0,
    })
}

fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        path: row.get(0)?,
        name: row.get(1)?,
        ext: row.get(2)?,
        kind: row.get(3)?,
        size: row.get(4)?,
        mtime: row.get(5)?,
        hidden: row.get::<_, i64>(6)? != 0,
        depth: row.get(7)?,
        fingerprint: row.get(8)?,
    })
}

const FILE_COLUMNS: &str = "files.path, files.name, files.ext, files.kind, files.size, \
                            files.mtime, files.hidden, files.depth, files.fingerprint";

const FILE_SELECT: &str = "SELECT path, name, ext, kind, size, mtime, hidden, depth, \
                           fingerprint FROM files";

const FILE_SELECT_BY_PATH: &str = "SELECT path, name, ext, kind, size, mtime, hidden, depth, \
                                   fingerprint FROM files WHERE path = ?1";

fn open_error(path: &Path, error: rusqlite::Error) -> SearchError {
    SearchError::IndexOpen {
        path: ferret_vfs::VfsPath::from(PathBuf::from(path)),
        message: error.to_string(),
    }
}

fn query_error(error: rusqlite::Error) -> SearchError {
    SearchError::IndexQuery {
        message: error.to_string(),
    }
}

fn write_error(error: rusqlite::Error) -> SearchError {
    SearchError::IndexWrite {
        message: error.to_string(),
    }
}

/// The whole schema, applied on every open.
///
/// `IF NOT EXISTS` throughout so this doubles as the migration for version 1:
/// there is nothing to migrate *from* yet, and when there is, a mismatched
/// `schema_version` quarantines the store rather than editing it in place.
///
/// The FTS table is external-content — it stores no copy of the text, only the
/// trigram postings, and the three triggers keep it in step with `files`. That
/// halves the store and makes it impossible for a writer to update one and
/// forget the other.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS roots(
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,
    state           TEXT NOT NULL,
    last_reconciled INTEGER,
    generation      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS files(
    id            INTEGER PRIMARY KEY,
    root_id       INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    path          TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL,
    name_lower    TEXT NOT NULL,
    ext           TEXT,
    kind          INTEGER NOT NULL,
    size          INTEGER NOT NULL,
    mtime         INTEGER,
    hidden        INTEGER NOT NULL,
    depth         INTEGER NOT NULL,
    fingerprint   TEXT NOT NULL,
    content_state INTEGER NOT NULL,
    seen          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS files_root ON files(root_id, seen);
CREATE INDEX IF NOT EXISTS files_content_state ON files(root_id, content_state);

CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
    name, path, content='files', content_rowid='id', tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS files_fts_insert AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;

CREATE TRIGGER IF NOT EXISTS files_fts_delete AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path)
        VALUES('delete', old.id, old.name, old.path);
END;

CREATE TRIGGER IF NOT EXISTS files_fts_update AFTER UPDATE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, name, path)
        VALUES('delete', old.id, old.name, old.path);
    INSERT INTO files_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
END;
"#;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestDb(std::path::PathBuf);

    impl TestDb {
        fn open() -> (Self, Db) {
            let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ferret-search-db-test-{}-{id}.sqlite3",
                std::process::id()
            ));
            let db = Db::open_writer(&path).unwrap();
            (Self(path), db)
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("sqlite3-shm"));
            let _ = std::fs::remove_file(self.0.with_extension("sqlite3-wal"));
        }
    }

    fn row(path: &Path, name: &str) -> FileRow {
        FileRow {
            path: path.to_string_lossy().into_owned(),
            name: name.to_owned(),
            ext: None,
            kind: kind::FILE,
            size: 0,
            mtime: None,
            hidden: false,
            depth: 1,
            fingerprint: name.to_owned(),
        }
    }

    #[test]
    fn candidate_pages_are_complete_and_path_ordered() {
        let (_temp, db) = TestDb::open();
        let scope_path = std::env::temp_dir().join(format!(
            "ferret-candidate-scope-{}",
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let scope = scope_path.to_string_lossy();
        let root = db.upsert_root(&scope).unwrap();
        let generation = db.begin_sweep(root.id).unwrap();
        for name in ["match-e", "match-c", "match-a", "match-d", "match-b"] {
            db.upsert_file(root.id, generation, &row(&scope_path.join(name), name))
                .unwrap();
        }

        let first = db
            .candidate_page(&scope, Some("match"), false, None, 2)
            .unwrap();
        let second = db
            .candidate_page(
                &scope,
                Some("match"),
                false,
                first.last().map(|row| row.path.as_str()),
                2,
            )
            .unwrap();
        let third = db
            .candidate_page(
                &scope,
                Some("match"),
                false,
                second.last().map(|row| row.path.as_str()),
                2,
            )
            .unwrap();
        let names = first
            .into_iter()
            .chain(second)
            .chain(third)
            .map(|row| row.name)
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["match-a", "match-b", "match-c", "match-d", "match-e"]
        );
    }
}
