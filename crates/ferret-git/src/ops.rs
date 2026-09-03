//! Reading status, and the four operations the status bar offers.
//!
//! Every function here blocks and belongs on a background thread. Each returns
//! either git's own summary of what it did, or a [`GitError::Failed`] carrying
//! git's own reason — Ferret does not paraphrase either one (D-51).

use std::ffi::OsStr;

use crate::command;
use crate::config;
use crate::discover::Repository;
use crate::errors::Result;
use crate::status::RepoStatus;

/// Read the repository's status.
///
/// The single invocation D-45 is built on: branch, upstream, ahead/behind and
/// every changed file, in one process.
///
/// `--no-optional-locks` matters more here than the flag's name suggests.
/// `git status` refreshes and rewrites the index by default, taking
/// `index.lock` to do it; this runs on a file-watcher event, so without the
/// flag it would take that lock at unpredictable moments and could lose a race
/// with the user's own git in the terminal panel below.
pub fn status(repo: &Repository) -> Result<RepoStatus> {
    let output = command::run_checked(
        repo.workdir(),
        "status",
        &[
            OsStr::new("--no-optional-locks"),
            OsStr::new("status"),
            OsStr::new("--porcelain=v2"),
            OsStr::new("--branch"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=normal"),
            OsStr::new("--ignored=no"),
        ],
        None,
        config::READ_TIMEOUT,
    )?;
    Ok(RepoStatus::parse(&output.stdout, repo.workdir()))
}

/// `git add -A` over the whole work tree.
///
/// Scoped to the root and never to the browsed folder: a "stage all changes"
/// checkbox that staged only the folder you happened to be looking at would be
/// a trap, and the commit that followed would be wrong in a way nobody notices
/// until later.
pub fn stage_all(repo: &Repository) -> Result<()> {
    command::run_checked(
        repo.workdir(),
        "add",
        &[OsStr::new("add"), OsStr::new("-A"), OsStr::new("--")],
        None,
        config::WRITE_TIMEOUT,
    )?;
    Ok(())
}

/// Commit with `message`, which arrives on stdin.
///
/// `-F -` rather than `-m <message>`: a message is arbitrary user text, and one
/// that begins with a `-`, or contains quotes or newlines, must not be able to
/// turn into arguments. It also means no shell is involved at any point.
///
/// Hooks run, because this is the real git binary — which is the point of D-04.
/// `WRITE_TIMEOUT` is what makes a `pre-commit` hook that runs a test suite
/// survive rather than being killed at some tidier-looking deadline.
pub fn commit(repo: &Repository, message: &str) -> Result<String> {
    let output = command::run_checked(
        repo.workdir(),
        "commit",
        &[OsStr::new("commit"), OsStr::new("-F"), OsStr::new("-")],
        Some(message.as_bytes().to_vec()),
        config::WRITE_TIMEOUT,
    )?;
    Ok(summary(&output.stdout, "Committed."))
}

/// `git pull --ff-only`.
///
/// Fast-forward only, deliberately. A pull that starts a merge — or that leaves
/// conflict markers in files — inside an application with no conflict
/// resolution UI is a trap. When git refuses, its own message says exactly why,
/// and the terminal panel is six pixels below the button that was pressed.
pub fn pull(repo: &Repository) -> Result<String> {
    let output = command::run_checked(
        repo.workdir(),
        "pull",
        &[OsStr::new("pull"), OsStr::new("--ff-only")],
        None,
        config::WRITE_TIMEOUT,
    )?;
    Ok(summary(&output.stdout, "Pulled."))
}

/// `git push`.
///
/// No `--force`, from any surface, ever. No `-u` either: a branch gains an
/// upstream because the user's own `push.autoSetupRemote` says so, not because
/// Ferret decided on their behalf.
pub fn push(repo: &Repository) -> Result<String> {
    let output = command::run_checked(
        repo.workdir(),
        "push",
        &[OsStr::new("push")],
        None,
        config::WRITE_TIMEOUT,
    )?;
    // git push reports progress and the result on stderr, not stdout.
    let text = String::from_utf8_lossy(&output.stderr);
    Ok(first_useful_line(&text).unwrap_or_else(|| "Pushed.".to_string()))
}

/// The first line of git's own output, for a success toast.
///
/// Success is one short sentence in git's words rather than Ferret's: "Updating
/// a1b2c3d..e4f5a6b" is more informative than "Pull complete", and it is the
/// text the user can search for when something later goes wrong.
fn summary(stdout: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stdout);
    first_useful_line(&text).unwrap_or_else(|| fallback.to_string())
}

fn first_useful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(command::cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_is_gits_first_line_and_falls_back_when_there_is_none() {
        assert_eq!(
            summary(
                b"Updating a1b2c3d..e4f5a6b\nFast-forward\n file | 2 +-\n",
                "Pulled."
            ),
            "Updating a1b2c3d..e4f5a6b"
        );
        // Leading blank lines are skipped rather than reported as an empty
        // toast — `git commit` opens with one on some configurations.
        assert_eq!(
            summary(b"\n\n[main 1a2b3c4] Updated screenshots.\n", "Committed."),
            "[main 1a2b3c4] Updated screenshots."
        );
        assert_eq!(summary(b"   \n", "Committed."), "Committed.");
        assert_eq!(summary(b"", "Pulled."), "Pulled.");
    }
}
