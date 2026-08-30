//! Finding the repository a browsed directory belongs to.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use ferret_vfs::VfsPath;

use crate::command;
use crate::config;
use crate::errors::{GitError, Result};

/// One work tree, and the git directory that backs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repository {
    /// The work tree root **as the user spelled it**, not as git resolved it.
    /// See [`discover`] for why the difference matters.
    workdir: PathBuf,
    /// The real git directory, with `.git`-file indirection already followed —
    /// so this is correct inside a submodule or a linked worktree, where
    /// `<workdir>/.git` is a file rather than a directory.
    git_dir: PathBuf,
}

impl Repository {
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// The directory to watch for "somebody committed, checked out or fetched".
    ///
    /// `index`, `HEAD`, `FETCH_HEAD`, `MERGE_HEAD` and `packed-refs` are all
    /// direct children of it, so a single non-recursive watch covers every way
    /// the repository's state changes.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Whether `dir` is inside this work tree, so a navigation can keep the
    /// current status instead of clearing it and starting over.
    pub fn contains(&self, dir: &Path) -> bool {
        dir.starts_with(&self.workdir)
    }

    /// The name shown in the commit dialog's subtitle.
    pub fn name(&self) -> &str {
        self.workdir
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("repository")
    }
}

/// Find the repository containing `dir`, if there is one.
///
/// `Ok(None)` is the ordinary answer — most directories are not repositories,
/// and that is not a failure (D-43). `Err` means git could not be asked at all.
///
/// **Why `--show-cdup` and not `--show-toplevel`.** `--show-toplevel` returns
/// the *physical* path, with every symlink resolved: browsing `/tmp/repo` on
/// macOS would give `/private/tmp/repo`, and every absolute path built from it
/// would then fail to match the `/tmp/...` paths the listing holds — badges
/// would silently never appear. `--show-cdup` returns how far *up* from `dir`
/// the work tree root is (`../../`, or empty at the root), which reconstructs
/// the root in the user's own spelling.
pub fn discover(dir: &VfsPath) -> Result<Option<Repository>> {
    // Remote backends have no work tree to run git in. Not an error: the panel
    // is simply absent on an SFTP mount.
    let Some(local) = dir.as_local() else {
        return Ok(None);
    };
    if !local.is_dir() {
        return Ok(None);
    }

    let output = command::run(
        local,
        "rev-parse",
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--show-cdup"),
            OsStr::new("--absolute-git-dir"),
        ],
        None,
        config::READ_TIMEOUT,
    )?;

    if !output.status_ok {
        // Exit 128 with "not a git repository" is the common case and means
        // no. Anything else — an unreadable directory, a broken `.git` file —
        // is reported, because a repository that exists and cannot be read is
        // worth knowing about.
        let message = output.message();
        if message.contains("not a git repository") || message.contains("not a working tree") {
            return Ok(None);
        }
        return Err(GitError::Failed {
            command: "rev-parse",
            message,
        });
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let cdup = lines.next().unwrap_or("").trim();
    let git_dir = lines.next().unwrap_or("").trim();
    if git_dir.is_empty() {
        return Ok(None);
    }

    Ok(Some(Repository {
        workdir: ascend(local, cdup),
        git_dir: PathBuf::from(git_dir),
    }))
}

/// Apply `--show-cdup` to `dir`, resolving the `../` segments lexically.
///
/// Lexical is correct here and only here: `cdup` is git's own count of how many
/// components up the root is from `dir`, so popping that many components lands
/// on it — whereas canonicalising would undo the whole point by resolving the
/// symlinks the caller's paths are spelled with.
fn ascend(dir: &Path, cdup: &str) -> PathBuf {
    let ups = cdup.split('/').filter(|segment| *segment == "..").count();
    let mut root = dir.to_path_buf();
    for _ in 0..ups {
        // A `cdup` deeper than the path it came from cannot happen; stopping
        // rather than unwrapping means a surprising git could not panic Ferret.
        if !root.pop() {
            break;
        }
    }
    // `.` and `..` never survive into a key the listing is matched against.
    debug_assert!(
        !root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "the work tree root must be a plain absolute path: {root:?}"
    );
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdup_reconstructs_the_root_in_the_users_own_spelling() {
        // The macOS case the doc comment describes: `/tmp` is a symlink to
        // `/private/tmp`, and `--show-toplevel` would return the latter.
        assert_eq!(
            ascend(Path::new("/tmp/repo/sub/deep"), "../../"),
            PathBuf::from("/tmp/repo")
        );
        // At the root itself git prints an empty line.
        assert_eq!(
            ascend(Path::new("/tmp/repo"), ""),
            PathBuf::from("/tmp/repo")
        );
        // One level.
        assert_eq!(
            ascend(Path::new("/tmp/repo/sub"), "../"),
            PathBuf::from("/tmp/repo")
        );
    }

    #[test]
    fn a_cdup_deeper_than_its_path_stops_rather_than_panicking() {
        assert_eq!(ascend(Path::new("/a"), "../../../"), PathBuf::from("/"));
    }

    #[test]
    fn containment_is_component_wise() {
        let repo = Repository {
            workdir: PathBuf::from("/w/project"),
            git_dir: PathBuf::from("/w/project/.git"),
        };
        assert!(repo.contains(Path::new("/w/project")));
        assert!(repo.contains(Path::new("/w/project/src")));
        // Text-wise this starts with `/w/project`; it is a different directory.
        assert!(!repo.contains(Path::new("/w/project-old")));
        assert!(!repo.contains(Path::new("/w")));
        assert_eq!(repo.name(), "project");
    }
}
