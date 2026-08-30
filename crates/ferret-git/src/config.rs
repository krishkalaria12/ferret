//! Every tunable in the crate.
//!
//! Public so tests and `ferret-ui` assert against the same constants the code
//! uses rather than restating them.

use std::time::Duration;

/// How long a read-only query (`status`, `rev-parse`) may run before it is
/// killed.
///
/// Long enough for a cold index on a large repository, or one sitting on a
/// network mount; short enough that a wedged git leaves the status bar blank
/// rather than permanently spinning. Reads take no locks (`--no-optional-locks`),
/// so killing one cannot leave the repository in a worse state.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a mutating or network operation may run.
///
/// Two minutes, because a real push of a real repository over a real
/// connection takes minutes and a thirty-second cap would abort more work than
/// it ever saved. A `pre-commit` hook that runs a test suite lands here too.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(120);

/// How often a running child is checked for exit while waiting on it.
///
/// A status read usually lands inside 100 ms, so this is roughly twenty
/// wake-ups on a background thread for the common case — cheaper than a
/// watchdog thread per invocation, and it is what makes the kill-on-timeout
/// path possible at all (the child must still be owned to be killed).
pub const WAIT_POLL: Duration = Duration::from_millis(5);

/// How long refresh requests coalesce before one status process is started.
///
/// A `git add -A` in the terminal panel produces a burst of watcher events in
/// well under this; so does saving a file that a formatter then rewrites. The
/// value is above `ferret_vfs::watch::DEBOUNCE` (150 ms) on purpose — this
/// debounce sits downstream of that one and must not fire inside its window.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(220);

/// Most bytes captured from one child before it is treated as a failure.
///
/// `git status` on a repository with a million untracked files really will
/// produce output this large, and growing a `Vec` to hold it is how the window
/// process dies. See [`crate::GitError::OutputTooLarge`].
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Above this many changed entries, per-file badges are dropped (D-49).
///
/// The counts and the buttons keep working; only the map and the roll-up set
/// are skipped, so a first `git add` on a huge import degrades to "no badges"
/// instead of building a hundred thousand `PathBuf`s on every watcher event.
/// Sized so the map stays well under a megabyte for any repository a person is
/// actually reading.
pub const MAX_BADGED_FILES: usize = 25_000;

/// Most characters of git's own message shown in a notification (D-51).
///
/// git's messages are shown unedited, and "unedited" has to stop somewhere: a
/// rejected push against a repository with a thousand refs prints a thousand
/// lines, and a toast is not a log viewer.
pub const MAX_MESSAGE_CHARS: usize = 1200;
