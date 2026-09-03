//! The crate's error type and `Result` alias.
//!
//! Every variant names what it failed on, and every `Display` is written to be
//! shown to a person unedited — `GitError::Failed` in particular carries git's
//! own message, which is the whole of D-51.

use std::path::PathBuf;

pub type Result<T, E = GitError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The `git` executable could not be started at all.
    ///
    /// Not an error the user is shown (D-43): most folders are not
    /// repositories and most people are not developers, so a missing git means
    /// the feature is absent rather than broken.
    #[error("Ferret can't run git — it isn't installed, or it isn't on this account's PATH.")]
    NotInstalled,

    #[error("Ferret couldn't start git {command} in {dir}: {source}")]
    Spawn {
        command: &'static str,
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The child ran past its deadline and was killed. Reported as itself
    /// rather than as a generic failure: "git push timed out" and "git push
    /// was rejected" call for different things from the user.
    #[error("git {command} in {dir} took longer than {seconds}s, so Ferret stopped it.")]
    TimedOut {
        command: &'static str,
        dir: PathBuf,
        seconds: u64,
    },

    /// git exited non-zero. `message` is git's own stderr, trimmed and capped.
    #[error("git {command} failed: {message}")]
    Failed {
        command: &'static str,
        message: String,
    },

    /// The child wrote more than [`crate::config::MAX_OUTPUT_BYTES`].
    ///
    /// Its own variant because the alternative — growing a `Vec` until the
    /// window's process is killed — is the failure this cap exists to prevent,
    /// and silently truncating would produce a status that is wrong rather
    /// than absent.
    #[error("git {command} produced more output than Ferret will hold ({limit} bytes).")]
    OutputTooLarge { command: &'static str, limit: usize },
}

impl GitError {
    /// Whether this is git saying "no", as opposed to Ferret failing to ask.
    ///
    /// The commit dialog keeps itself open on the first kind so the message is
    /// not lost with the typed text, and closes on the second.
    pub fn is_git_refusal(&self) -> bool {
        matches!(self, GitError::Failed { .. })
    }
}
