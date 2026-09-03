//! `git status --porcelain=v2 --branch -z`, parsed.
//!
//! The parser is a pure function from bytes to a [`RepoStatus`]: it spawns
//! nothing, touches no disk and opens no window, which is what lets the format's
//! sharp edges be tested from byte literals (`docs/git.html` §6). Everything
//! expensive — turning repository-relative paths into absolute ones, and rolling
//! changes up onto their directories — happens here too, once per refresh and off
//! the window thread, so that the render path is left with a single hash lookup
//! per row (D-47).

use std::path::{Path, PathBuf};

use smol_str::SmolStr;

use crate::config;

/// The hasher for the two indices.
///
/// Keys are paths read from the user's own disk, never attacker-chosen input —
/// the same reasoning that put `foldhash` behind `ferret-ui`'s folder indices.
type FastMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

/// What the listing draws for one entry, collapsed from porcelain's `XY` pair.
///
/// Deliberately not a faithful model of git's state machine: the listing has one
/// dot per row, so the vocabulary is the smallest set that still distinguishes
/// the states a person acts on differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileStatus {
    /// Not in the index. New is not an alarm, so this ranks lowest.
    Untracked,
    /// Staged and unchanged since — ready to commit.
    Staged,
    /// Changed in the work tree and not staged.
    Modified,
    /// Staged, then edited again. The state people lose work in, so it outranks
    /// both halves it is made of.
    StagedAndModified,
    /// Unmerged. Outranks everything; it is the only state that blocks work.
    Conflicted,
}

impl FileStatus {
    /// Urgency, for rolling several children up onto one directory.
    ///
    /// The order is "what would you want to be told about first if you could
    /// only be told one thing", not git's own ordering — which has none.
    pub fn rank(self) -> u8 {
        match self {
            FileStatus::Untracked => 1,
            FileStatus::Staged => 2,
            FileStatus::Modified => 3,
            FileStatus::StagedAndModified => 4,
            FileStatus::Conflicted => 5,
        }
    }

    /// The more urgent of two statuses.
    pub fn merge(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    /// Tooltip text. Written to be read after the file's name, so it starts
    /// lower-case and says what git would do about it.
    pub fn label(self) -> &'static str {
        match self {
            FileStatus::Untracked => "untracked",
            FileStatus::Staged => "staged",
            FileStatus::Modified => "modified, not staged",
            FileStatus::StagedAndModified => "staged, with further changes",
            FileStatus::Conflicted => "conflicted",
        }
    }

    /// From porcelain's two-character `XY` field: `X` is the index against
    /// HEAD, `Y` is the work tree against the index, and `.` means unmodified.
    fn from_xy(xy: &[u8]) -> Self {
        let staged = xy.first().is_some_and(|c| *c != b'.');
        let unstaged = xy.get(1).is_some_and(|c| *c != b'.');
        match (staged, unstaged) {
            (true, true) => FileStatus::StagedAndModified,
            (true, false) => FileStatus::Staged,
            // `..` cannot occur — git emits a record only when something
            // differs — but a record that claims it is still a change, and
            // dropping it would under-count. Treat it as the work-tree case.
            (false, _) => FileStatus::Modified,
        }
    }
}

/// What HEAD is, in the three shapes `branch.head` can actually take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// On a branch with at least one commit.
    Branch(SmolStr),
    /// Detached. Carries the abbreviated commit, because that is the only
    /// name the state has.
    Detached(SmolStr),
    /// On a branch that has no commits yet — a fresh `git init`. `branch.oid`
    /// reads `(initial)`, and nothing may assume a commit exists.
    Unborn(SmolStr),
    /// The headers were absent or unparseable. Rendered as nothing at all
    /// rather than as a guess.
    Unknown,
}

impl Head {
    /// What the branch chip shows.
    pub fn label(&self) -> &str {
        match self {
            Head::Branch(name) | Head::Unborn(name) => name,
            Head::Detached(oid) => oid,
            Head::Unknown => "",
        }
    }

    pub fn is_detached(&self) -> bool {
        matches!(self, Head::Detached(_))
    }

    /// On a named branch with at least one commit — the only state a plain
    /// `git push` has a source and a destination to work out for itself.
    pub fn is_branch(&self) -> bool {
        matches!(self, Head::Branch(_))
    }

    pub fn is_unborn(&self) -> bool {
        matches!(self, Head::Unborn(_))
    }
}

/// Ahead/behind against the upstream. `None` when there is no upstream at all —
/// which is not the same as being in sync, and must not render as `↑0 ↓0`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tracking {
    pub ahead: u32,
    pub behind: u32,
}

/// How many files are in each state.
///
/// A file that is staged and then edited again increments both `staged` and
/// `unstaged`, but `changed` only once — `changed` counts files, and it is the
/// number the status bar shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub changed: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
    pub conflicted: u32,
}

impl Counts {
    pub fn is_clean(&self) -> bool {
        self.changed == 0
    }

    /// Whether the Commit button has anything to do.
    ///
    /// True with untracked files present, because the dialog's default is to
    /// stage everything first — a Commit that is disabled in a folder full of
    /// new files, with a checked "stage all" box beneath it, reads as broken.
    pub fn has_something_to_commit(&self) -> bool {
        !self.is_clean()
    }
}

/// The per-path lookup the listing uses, plus the roll-up onto directories.
///
/// Holds only what changed: a clean repository's index is empty, and nothing
/// proportional to the size of the directory being browsed is ever stored.
#[derive(Debug, Default)]
pub struct StatusIndex {
    files: FastMap<PathBuf, FileStatus>,
    /// Directories that contain a change, carrying the most urgent one found
    /// beneath them. Precomputed so a directory row costs one lookup rather
    /// than a walk (`docs/git.html` §7, rule 3).
    dirs: FastMap<PathBuf, FileStatus>,
    /// Absolute paths of directories git reported as wholly untracked. git says
    /// nothing about what is inside them, so the listing has to (rule 4).
    /// Almost always empty or tiny.
    untracked_roots: Vec<PathBuf>,
    /// Set when the repository had more changes than [`config::MAX_BADGED_FILES`]
    /// and the maps were deliberately left empty (D-49).
    truncated: bool,
}

impl StatusIndex {
    /// The status of one row, given whether it navigates like a directory.
    ///
    /// One hash of a borrowed `&Path` — no join, no format, no allocation, so
    /// this adds nothing to the per-frame budget.
    pub fn status_for(&self, path: &Path, is_dir: bool) -> Option<FileStatus> {
        if is_dir {
            self.dirs.get(path).copied()
        } else {
            self.files.get(path).copied()
        }
    }

    /// The wholly-untracked directory `dir` is, or is inside, if there is one.
    ///
    /// Answered once per refresh by the caller and cached, never per row: the
    /// walk is short but it is not free, and the answer is the same for every
    /// row in a listing.
    ///
    /// The *root* comes back rather than a yes/no, because the answer only
    /// licenses the caller to assume "untracked" for paths beneath that root —
    /// and a listing can show rows from outside it. See
    /// `GitSnapshot::status_for`.
    pub fn untracked_root_for(&self, dir: &Path) -> Option<&Path> {
        self.untracked_roots
            .iter()
            .find(|root| dir.starts_with(root))
            .map(PathBuf::as_path)
    }

    /// Whether badges were dropped for size (D-49). The status bar's tooltip
    /// says so, because a listing with no badges in a repository with changes
    /// is otherwise indistinguishable from a bug.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// How many individual files carry a status. For tests and diagnostics.
    pub fn tracked_len(&self) -> usize {
        self.files.len()
    }
}

/// Everything one `git status` call produced.
#[derive(Debug)]
pub struct RepoStatus {
    pub head: Head,
    pub upstream: Option<SmolStr>,
    pub tracking: Option<Tracking>,
    pub counts: Counts,
    pub index: StatusIndex,
}

impl RepoStatus {
    /// Parse porcelain v2 output. `workdir` is the work tree root **as the user
    /// spelled it** — see `discover`, which derives it from `--show-cdup`
    /// rather than from `--show-toplevel`, so that the absolute paths built
    /// here match the paths the listing holds.
    pub fn parse(output: &[u8], workdir: &Path) -> Self {
        let mut oid: Option<SmolStr> = None;
        let mut branch: Option<SmolStr> = None;
        let mut detached = false;
        let mut unborn = false;
        let mut upstream = None;
        let mut tracking = None;
        let mut counts = Counts::default();
        // Repository-relative for now; made absolute in one pass below.
        let mut entries: Vec<(&[u8], FileStatus, bool)> = Vec::new();

        let mut records = output.split(|byte| *byte == 0);
        while let Some(record) = records.next() {
            if record.is_empty() {
                continue;
            }
            match record[0] {
                b'#' => {
                    if let Some(value) = header(record, b"branch.oid") {
                        if value == b"(initial)" {
                            unborn = true;
                        } else {
                            oid = Some(SmolStr::new(short_oid(value)));
                        }
                    } else if let Some(value) = header(record, b"branch.head") {
                        if value == b"(detached)" {
                            detached = true;
                        } else {
                            branch = Some(SmolStr::new(String::from_utf8_lossy(value)));
                        }
                    } else if let Some(value) = header(record, b"branch.upstream") {
                        upstream = Some(SmolStr::new(String::from_utf8_lossy(value)));
                    } else if let Some(value) = header(record, b"branch.ab") {
                        tracking = parse_ab(value);
                    }
                }
                // Ordinary change: eight fields, then the path.
                b'1' => push_entry(record, 8, &mut entries, &mut counts, None),
                // Rename or copy: nine fields, then the path — and then a
                // *second* NUL-terminated record holding the original path,
                // which must be consumed or every record after it is read as
                // the wrong kind (`docs/git.html` §6, rule 1).
                b'2' => {
                    push_entry(record, 9, &mut entries, &mut counts, None);
                    records.next();
                }
                // Unmerged: ten fields, then the path.
                b'u' => push_entry(
                    record,
                    10,
                    &mut entries,
                    &mut counts,
                    Some(FileStatus::Conflicted),
                ),
                b'?' => push_entry(
                    record,
                    1,
                    &mut entries,
                    &mut counts,
                    Some(FileStatus::Untracked),
                ),
                // `!` never arrives: the query passes `--ignored=no`. Skipped
                // rather than trusted, so turning that flag on cannot silently
                // start badging every build artefact.
                _ => {}
            }
        }

        let head = if detached {
            Head::Detached(oid.unwrap_or_else(|| SmolStr::new_static("detached")))
        } else {
            match branch {
                Some(name) if unborn => Head::Unborn(name),
                Some(name) => Head::Branch(name),
                None => Head::Unknown,
            }
        };

        RepoStatus {
            head,
            upstream,
            // An unborn branch has no upstream state worth showing even if a
            // header slipped through: there is nothing to be ahead of.
            tracking: tracking.filter(|_| !unborn),
            counts,
            index: build_index(workdir, &entries),
        }
    }

    /// What the branch chip's tooltip says. Built here rather than in the view
    /// so the wording is testable without a window.
    pub fn tooltip(&self, workdir: &Path) -> String {
        let mut text = match &self.head {
            Head::Branch(name) => format!("On branch {name}"),
            Head::Unborn(name) => format!("On branch {name} — no commits yet"),
            Head::Detached(oid) => format!("Detached at {oid}"),
            Head::Unknown => "Git repository".to_string(),
        };
        if let Some(upstream) = &self.upstream {
            text.push_str(&format!("\nTracking {upstream}"));
        }
        text.push_str(&format!("\n{}", workdir.display()));
        if self.index.is_truncated() {
            text.push_str(
                "\n\nToo many changes to mark individual files; the counts above are exact.",
            );
        }
        text
    }
}

/// The value of `# <key> ` in a header record, if this record is that header.
fn header<'a>(record: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let rest = record.strip_prefix(b"# ")?;
    let rest = rest.strip_prefix(key)?;
    rest.strip_prefix(b" ")
}

/// `+41 -7` → ahead 41, behind 7. Absent entirely when there is no upstream,
/// which is why the caller keeps this as an `Option` rather than a zero.
fn parse_ab(value: &[u8]) -> Option<Tracking> {
    let text = std::str::from_utf8(value).ok()?;
    let mut parts = text.split_whitespace();
    let ahead = parts.next()?.strip_prefix('+')?.parse().ok()?;
    let behind = parts.next()?.strip_prefix('-')?.parse().ok()?;
    Some(Tracking { ahead, behind })
}

/// git prints full object ids in `branch.oid`; a chip has room for seven.
fn short_oid(value: &[u8]) -> String {
    String::from_utf8_lossy(&value[..value.len().min(7)]).into_owned()
}

/// Pull the path out of one entry record and account for it.
///
/// `fields` is how many space-separated fields precede the path — the one
/// number that differs between the four record types, and the one a parser gets
/// wrong.
fn push_entry<'a>(
    record: &'a [u8],
    fields: usize,
    entries: &mut Vec<(&'a [u8], FileStatus, bool)>,
    counts: &mut Counts,
    forced: Option<FileStatus>,
) {
    let Some(path) = after_fields(record, fields) else {
        return;
    };
    let status = match forced {
        Some(status) => status,
        None => match record.get(2..4) {
            Some(xy) => FileStatus::from_xy(xy),
            None => return,
        },
    };

    // A trailing slash marks a wholly-untracked directory standing in for an
    // unbounded number of files (§6, rule 4).
    let is_dir = path.last() == Some(&b'/');
    let path = if is_dir {
        &path[..path.len() - 1]
    } else {
        path
    };
    if path.is_empty() {
        return;
    }

    counts.changed += 1;
    match status {
        FileStatus::Untracked => counts.untracked += 1,
        FileStatus::Conflicted => counts.conflicted += 1,
        FileStatus::Staged => counts.staged += 1,
        FileStatus::Modified => counts.unstaged += 1,
        FileStatus::StagedAndModified => {
            counts.staged += 1;
            counts.unstaged += 1;
        }
    }
    entries.push((path, status, is_dir));
}

/// The remainder of `record` after `count` space-separated fields.
///
/// Fields never contain spaces; the path can, which is exactly why it is taken
/// as "everything left" rather than as another field.
fn after_fields(record: &[u8], count: usize) -> Option<&[u8]> {
    let mut at = 0;
    for _ in 0..count {
        let space = record.get(at..)?.iter().position(|byte| *byte == b' ')?;
        at += space + 1;
    }
    record.get(at..).filter(|rest| !rest.is_empty())
}

/// Turn repository-relative entries into the two absolute-path indices.
///
/// This is the whole of the per-refresh cost, and it is deliberately all here:
/// the render path gets a map that is already keyed the way a row asks.
fn build_index(workdir: &Path, entries: &[(&[u8], FileStatus, bool)]) -> StatusIndex {
    let mut index = StatusIndex::default();
    if entries.len() > config::MAX_BADGED_FILES {
        // D-49: the counts survive, the badges do not.
        index.truncated = true;
        return index;
    }

    index.files.reserve(entries.len());
    for (relative, status, is_dir) in entries {
        let absolute = workdir.join(to_path(relative));
        if *is_dir {
            merge_dir(&mut index.dirs, &absolute, *status);
            index.untracked_roots.push(absolute.clone());
        } else {
            index
                .files
                .entry(absolute.clone())
                .and_modify(|existing| *existing = existing.merge(*status))
                .or_insert(*status);
        }

        // Roll the change up onto every directory between it and the work tree
        // root, the root included — so browsing a repository's *parent* shows a
        // dot on the repository itself.
        let mut ancestor = absolute.parent();
        while let Some(dir) = ancestor {
            if !dir.starts_with(workdir) {
                break;
            }
            merge_dir(&mut index.dirs, dir, *status);
            if dir == workdir {
                break;
            }
            ancestor = dir.parent();
        }
    }
    index
}

/// Insert or promote a directory's roll-up, allocating only on a miss.
fn merge_dir(dirs: &mut FastMap<PathBuf, FileStatus>, dir: &Path, status: FileStatus) {
    match dirs.get_mut(dir) {
        Some(existing) => *existing = existing.merge(status),
        None => {
            dirs.insert(dir.to_path_buf(), status);
        }
    }
}

/// A repository-relative path, as bytes, becomes a `Path`.
///
/// On Unix a filename is arbitrary non-NUL bytes and must not go through
/// `str`; on Windows git emits UTF-8 and the lossy conversion is exact.
#[cfg(unix)]
fn to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/w")
    }

    /// Records are NUL-terminated; `@` reads better in a test literal.
    fn bytes(text: &str) -> Vec<u8> {
        text.replace('@', "\0").into_bytes()
    }

    fn parse(text: &str) -> RepoStatus {
        RepoStatus::parse(&bytes(text), &root())
    }

    #[test]
    fn a_clean_branch_with_an_upstream() {
        let status = parse(
            "# branch.oid abcdef1234567890@# branch.head main@\
             # branch.upstream origin/main@# branch.ab +41 -7@",
        );
        assert_eq!(status.head, Head::Branch("main".into()));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!(
            status.tracking,
            Some(Tracking {
                ahead: 41,
                behind: 7
            })
        );
        assert!(status.counts.is_clean());
        assert_eq!(status.index.tracked_len(), 0);
    }

    #[test]
    fn no_upstream_is_not_the_same_as_being_in_sync() {
        // The real output of a branch with no upstream: both `branch.upstream`
        // and `branch.ab` are simply absent. Reporting 0/0 here would tell the
        // user they are up to date with something that does not exist.
        let status = parse("# branch.oid abcdef1@# branch.head feat/x@");
        assert_eq!(status.upstream, None);
        assert_eq!(status.tracking, None);
    }

    #[test]
    fn a_detached_head_is_named_by_its_commit() {
        let status = parse(
            "# branch.oid 51be0adc951f1e88a30b5b60857c9442c58d13f0@# branch.head (detached)@",
        );
        assert_eq!(status.head, Head::Detached("51be0ad".into()));
        assert!(status.head.is_detached());
        assert_eq!(status.head.label(), "51be0ad");
    }

    #[test]
    fn an_unborn_branch_has_a_name_and_no_commit() {
        // A fresh `git init`: `branch.oid` reads `(initial)`.
        let status = parse("# branch.oid (initial)@# branch.head main@");
        assert_eq!(status.head, Head::Unborn("main".into()));
        assert!(status.head.is_unborn());
        assert_eq!(status.head.label(), "main");
    }

    #[test]
    fn a_rename_consumes_its_original_path_record() {
        // The failure this guards: without consuming the second record, `b.txt`
        // is read as an entry of unknown type and everything after it shifts.
        // The literal is real output, from a staged `git mv` plus one unstaged
        // edit and two untracked entries.
        let status = parse(
            "# branch.oid 585a06d@# branch.head main@\
             2 R. N... 100644 100644 100644 6178079 6178079 R100 renamed name.txt@b.txt@\
             1 .M N... 100644 100644 100644 7898192 7898192 sub/deep/a.txt@\
             ? space file.txt@? untracked_dir/@",
        );

        assert_eq!(status.counts.changed, 4, "four entries, not five");
        assert_eq!(status.counts.staged, 1);
        assert_eq!(status.counts.unstaged, 1);
        assert_eq!(status.counts.untracked, 2);

        let index = &status.index;
        assert_eq!(
            index.status_for(Path::new("/w/renamed name.txt"), false),
            Some(FileStatus::Staged)
        );
        // The original path is consumed, not indexed: `b.txt` no longer exists.
        assert_eq!(index.status_for(Path::new("/w/b.txt"), false), None);
        assert_eq!(
            index.status_for(Path::new("/w/sub/deep/a.txt"), false),
            Some(FileStatus::Modified)
        );
        // A space in a name survives, because `-z` means it was never quoted.
        assert_eq!(
            index.status_for(Path::new("/w/space file.txt"), false),
            Some(FileStatus::Untracked)
        );
    }

    #[test]
    fn changes_roll_up_onto_every_directory_above_them() {
        let status = parse(
            "# branch.head main@\
             1 .M N... 100644 100644 100644 a b sub/deep/a.txt@",
        );
        let index = &status.index;
        assert_eq!(
            index.status_for(Path::new("/w/sub/deep"), true),
            Some(FileStatus::Modified)
        );
        assert_eq!(
            index.status_for(Path::new("/w/sub"), true),
            Some(FileStatus::Modified)
        );
        // The work tree root itself, so browsing a repository's parent shows
        // that the repository has changes in it.
        assert_eq!(
            index.status_for(Path::new("/w"), true),
            Some(FileStatus::Modified)
        );
        // And the roll-up never escapes the work tree.
        assert_eq!(index.status_for(Path::new("/"), true), None);
    }

    #[test]
    fn a_directory_shows_the_most_urgent_change_beneath_it() {
        let status = parse(
            "# branch.head main@\
             ? sub/new.txt@\
             u UU N... 100644 100644 100644 100644 a b c sub/deep/conflict.txt@\
             1 M. N... 100644 100644 100644 a b sub/staged.txt@",
        );
        assert_eq!(status.counts.conflicted, 1);
        // Untracked, staged and conflicted all live under `sub`; conflicted is
        // the one thing you would want to be told.
        assert_eq!(
            status.index.status_for(Path::new("/w/sub"), true),
            Some(FileStatus::Conflicted)
        );
        assert_eq!(
            status.index.status_for(Path::new("/w/sub/new.txt"), false),
            Some(FileStatus::Untracked)
        );
    }

    #[test]
    fn a_file_row_and_a_directory_row_do_not_share_an_index() {
        // `sub` has changes beneath it but is not itself a changed file, and a
        // file called `sub` in another repository must not inherit that.
        let status = parse("# branch.head main@1 .M N... 100644 100644 100644 a b sub/a.txt@");
        assert_eq!(
            status.index.status_for(Path::new("/w/sub"), true),
            Some(FileStatus::Modified)
        );
        assert_eq!(status.index.status_for(Path::new("/w/sub"), false), None);
    }

    #[test]
    fn a_wholly_untracked_directory_covers_everything_inside_it() {
        let status = parse("# branch.head main@? node_modules/@");
        let index = &status.index;
        // git collapses it to one entry, so the directory itself is badged…
        assert_eq!(
            index.status_for(Path::new("/w/node_modules"), true),
            Some(FileStatus::Untracked)
        );
        // …and nothing inside it is mentioned at all, which is what the
        // browsed-directory flag exists to answer.
        assert_eq!(
            index.status_for(Path::new("/w/node_modules/react/index.js"), false),
            None
        );
        let root = Path::new("/w/node_modules");
        assert_eq!(index.untracked_root_for(root), Some(root));
        assert_eq!(
            index.untracked_root_for(Path::new("/w/node_modules/react")),
            Some(root)
        );
        assert_eq!(index.untracked_root_for(Path::new("/w/src")), None);
        // Not a prefix match on text: `/w/node_modules_old` is a different
        // directory that happens to start with the same bytes.
        assert_eq!(
            index.untracked_root_for(Path::new("/w/node_modules_old")),
            None
        );
    }

    #[test]
    fn staged_and_then_edited_again_is_its_own_state() {
        let status = parse("# branch.head main@1 MM N... 100644 100644 100644 a b x.txt@");
        assert_eq!(
            status.index.status_for(Path::new("/w/x.txt"), false),
            Some(FileStatus::StagedAndModified)
        );
        // Counted once as a change, and in both halves of the breakdown.
        assert_eq!(status.counts.changed, 1);
        assert_eq!(status.counts.staged, 1);
        assert_eq!(status.counts.unstaged, 1);
    }

    #[test]
    fn empty_and_truncated_output_produce_nothing_rather_than_a_guess() {
        let status = RepoStatus::parse(b"", &root());
        assert_eq!(status.head, Head::Unknown);
        assert_eq!(status.head.label(), "");
        assert!(status.counts.is_clean());

        // A record cut off mid-way — a killed child, a full pipe — must not
        // panic or invent an entry.
        let partial = parse("# branch.head main@1 .M N... 100644 100@");
        assert_eq!(partial.head, Head::Branch("main".into()));
        assert!(partial.counts.is_clean());
    }

    #[test]
    fn urgency_orders_the_way_the_roll_up_needs() {
        use FileStatus::*;
        assert_eq!(Untracked.merge(Conflicted), Conflicted);
        assert_eq!(Conflicted.merge(Untracked), Conflicted);
        assert_eq!(Staged.merge(Modified), Modified);
        assert_eq!(Modified.merge(StagedAndModified), StagedAndModified);
        assert_eq!(Untracked.merge(Untracked), Untracked);
    }

    #[test]
    fn past_the_cut_off_the_counts_survive_and_the_badges_do_not() {
        // D-49. Built rather than written out: the point is the size.
        let mut output = b"# branch.head main@".to_vec();
        let over = config::MAX_BADGED_FILES + 1;
        for n in 0..over {
            output.extend_from_slice(format!("? f{n}.txt\0").as_bytes());
        }
        let text = String::from_utf8(output).unwrap().replace('@', "\0");
        let status = RepoStatus::parse(text.as_bytes(), &root());

        assert_eq!(status.counts.changed as usize, over, "the count is exact");
        assert!(status.index.is_truncated());
        assert_eq!(status.index.tracked_len(), 0, "and no map was built");
        assert_eq!(status.index.status_for(Path::new("/w/f0.txt"), false), None);
    }
}
