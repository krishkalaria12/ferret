//! End-to-end checks against a real repository built by the test.
//!
//! The parser's own tests (`status.rs`) run over byte literals and cover the
//! format's edges. These cover the half a literal cannot: that the flags in
//! `ops::status` are accepted by the installed git, that `--show-cdup`
//! reconstructs the work tree root, and that the paths coming back out actually
//! match the paths a listing would hold.
//!
//! Every one of them skips when git is not installed, because the crate is
//! built to work without it (D-43) and a CI image without git should not fail
//! the suite.

use std::path::{Path, PathBuf};
use std::process::Command;

use ferret_git::{FileStatus, Head};
use ferret_vfs::VfsPath;

/// A temporary repository, removed when the test ends.
struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

impl Fixture {
    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(&self.root)
            .args(args)
            .status()
            .unwrap_or_else(|err| panic!("git {args:?}: {err}"));
        assert!(status.success(), "git {args:?} failed");
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

/// A repository with one commit, or `None` when git is not installed.
fn repository(tag: &str) -> Option<Fixture> {
    if Command::new("git").arg("--version").output().is_err() {
        return None;
    }
    let root = std::env::temp_dir().join(format!(
        "ferret-git-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture { root };
    fixture.git(&["init", "-q", "-b", "main", "."]);
    // Identity and signing are set on the repository so the test cannot be
    // broken — or made to hang on a passphrase — by the developer's own config.
    fixture.git(&["config", "user.email", "test@ferret.invalid"]);
    fixture.git(&["config", "user.name", "Ferret Test"]);
    fixture.git(&["config", "commit.gpgsign", "false"]);
    Some(fixture)
}

#[test]
fn a_directory_outside_a_repository_is_not_an_error() {
    let Some(fixture) = repository("outside") else {
        return;
    };
    // The fixture's parent is a temp directory, which is not a repository.
    let outside = VfsPath::local(fixture.root.parent().unwrap().to_path_buf());
    let found = ferret_git::discover(&outside).expect("asking is not a failure");
    assert!(
        found.is_none(),
        "most directories are not repositories, and that is not an error (D-43)"
    );

    // Neither is a path that does not exist at all.
    let missing = VfsPath::local(fixture.path("no/such/dir"));
    assert!(ferret_git::discover(&missing).unwrap().is_none());
}

#[test]
fn discovery_from_a_subdirectory_finds_the_work_tree_and_the_git_dir() {
    let Some(fixture) = repository("discover") else {
        return;
    };
    fixture.write("sub/deep/a.txt", "a\n");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "one"]);

    let repo = ferret_git::discover(&VfsPath::local(fixture.path("sub/deep")))
        .expect("git ran")
        .expect("a repository is found from two levels down");

    // The root is reconstructed from `--show-cdup`, so it is spelled the way
    // the caller spelled it rather than the way git resolves symlinks.
    assert_eq!(repo.workdir(), fixture.root.as_path());
    assert_eq!(repo.git_dir().file_name().unwrap(), ".git");
    assert!(repo.contains(&fixture.path("sub/deep")));
    assert!(!repo.contains(Path::new("/")));
}

#[test]
fn status_reports_the_branch_and_every_kind_of_change() {
    let Some(fixture) = repository("status") else {
        return;
    };
    fixture.write("kept.txt", "kept\n");
    fixture.write("sub/deep/a.txt", "a\n");
    fixture.write("moved.txt", "moved\n");
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "one"]);

    // One of each: a staged rename, an unstaged edit, an untracked file whose
    // name contains a space, and a wholly untracked directory.
    fixture.git(&["mv", "moved.txt", "renamed name.txt"]);
    fixture.write("sub/deep/a.txt", "a\nchanged\n");
    fixture.write("new file.txt", "new\n");
    fixture.write("vendor/lib/x.txt", "x\n");

    let repo = ferret_git::discover(&VfsPath::local(fixture.root.clone()))
        .unwrap()
        .unwrap();
    let status = ferret_git::status(&repo).expect("status runs");

    assert_eq!(status.head, Head::Branch("main".into()));
    assert_eq!(status.upstream, None, "no remote was ever added");
    assert_eq!(
        status.tracking, None,
        "and so there is nothing to be ahead of"
    );
    assert_eq!(status.counts.changed, 4);
    assert_eq!(status.counts.staged, 1);
    assert_eq!(status.counts.unstaged, 1);
    assert_eq!(status.counts.untracked, 2);
    assert!(!status.index.is_truncated());

    let index = &status.index;
    // The paths are absolute and match what a listing of these directories
    // would hold — which is the whole point of building them here (D-47).
    assert_eq!(
        index.status_for(&fixture.path("renamed name.txt"), false),
        Some(FileStatus::Staged)
    );
    assert_eq!(
        index.status_for(&fixture.path("sub/deep/a.txt"), false),
        Some(FileStatus::Modified)
    );
    assert_eq!(
        index.status_for(&fixture.path("new file.txt"), false),
        Some(FileStatus::Untracked)
    );
    assert_eq!(
        index.status_for(&fixture.path("kept.txt"), false),
        None,
        "an unchanged file carries no badge"
    );

    // The roll-up reaches every directory above a change, the root included.
    assert_eq!(
        index.status_for(&fixture.path("sub/deep"), true),
        Some(FileStatus::Modified)
    );
    assert_eq!(
        index.status_for(&fixture.path("sub"), true),
        Some(FileStatus::Modified)
    );
    assert!(index.status_for(&fixture.root, true).is_some());

    // git collapsed `vendor/` to one entry, so nothing inside it is named and
    // the browsed-directory flag is what covers its contents.
    assert_eq!(
        index.status_for(&fixture.path("vendor"), true),
        Some(FileStatus::Untracked)
    );
    assert_eq!(
        index.status_for(&fixture.path("vendor/lib/x.txt"), false),
        None
    );
    assert!(index.is_inside_untracked_root(&fixture.path("vendor/lib")));
    assert!(!index.is_inside_untracked_root(&fixture.path("sub")));
}

#[test]
fn a_fresh_repository_is_unborn_and_a_commit_gives_it_a_branch() {
    let Some(fixture) = repository("unborn") else {
        return;
    };
    let repo = ferret_git::discover(&VfsPath::local(fixture.root.clone()))
        .unwrap()
        .unwrap();

    let status = ferret_git::status(&repo).unwrap();
    assert!(
        status.head.is_unborn(),
        "a repository with no commits must not claim to be on a normal branch: {:?}",
        status.head
    );
    assert_eq!(status.head.label(), "main");

    // Stage-all and commit go through Ferret's own operations, so this also
    // covers the stdin path a message with quotes and newlines depends on.
    fixture.write("a.txt", "a\n");
    ferret_git::stage_all(&repo).expect("stage all");
    let summary =
        ferret_git::commit(&repo, "Fix the \"quoted\" thing\n\nWith a body.\n").expect("commit");
    assert!(
        summary.contains("a.txt") || summary.contains("main"),
        "{summary}"
    );

    let after = ferret_git::status(&repo).unwrap();
    assert_eq!(after.head, Head::Branch("main".into()));
    assert!(after.counts.is_clean(), "{:?}", after.counts);
}

#[test]
fn a_refused_operation_carries_gits_own_message() {
    let Some(fixture) = repository("refused") else {
        return;
    };
    let repo = ferret_git::discover(&VfsPath::local(fixture.root.clone()))
        .unwrap()
        .unwrap();

    // Nothing staged, nothing to commit. D-51: the message the user sees is
    // git's, not one Ferret invented.
    let error = ferret_git::commit(&repo, "empty").expect_err("git refuses an empty commit");
    assert!(error.is_git_refusal(), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("nothing to commit") || text.contains("no changes added"),
        "git's own words must survive: {text}"
    );
}

/// A symlink to a directory is recorded by git under its **own** path.
///
/// The distinction the listing has to respect. Ferret classifies such a link as
/// directory-*like*, because double-clicking it navigates — but git gives it
/// mode `120000` and emits it as an ordinary changed-file record, so it lands in
/// the file index and never in the per-directory roll-up. A lookup keyed on
/// "navigates like a directory" therefore asks the wrong map and finds nothing,
/// and a modified link stays silently unbadged in every view.
///
/// Written against a real repository rather than a byte literal on purpose: the
/// claim being tested is about what git does, not about the parser.
#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_is_indexed_as_a_file() {
    let Some(fixture) = repository("dirlink") else {
        return;
    };
    fixture.write("realdir/a.txt", "a\n");
    std::os::unix::fs::symlink("realdir", fixture.path("dirlink")).unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "-qm", "one"]);

    // Repoint the link. The link changed; nothing inside `realdir` did.
    std::fs::remove_file(fixture.path("dirlink")).unwrap();
    std::os::unix::fs::symlink(".", fixture.path("dirlink")).unwrap();

    let repo = ferret_git::discover(&VfsPath::local(fixture.root.clone()))
        .unwrap()
        .unwrap();
    let index = ferret_git::status(&repo).unwrap().index;

    // Present as a file…
    assert_eq!(
        index.status_for(&fixture.path("dirlink"), false),
        Some(FileStatus::Modified),
        "git records the link itself, under its own path"
    );
    // …and absent from the directory map, which is what the badge lookup used
    // to consult for it.
    assert_eq!(
        index.status_for(&fixture.path("dirlink"), true),
        None,
        "a changed symlink puts nothing in the per-directory roll-up"
    );
    // The directory it points at is untouched, and must not inherit a badge.
    assert_eq!(index.status_for(&fixture.path("realdir"), true), None);
}
