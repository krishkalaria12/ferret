//! The only module in the workspace that spawns `git`.
//!
//! One place, so the non-interactive contract (D-44) cannot be forgotten by a
//! future operation: Ferret has no controlling terminal, and a git that decides
//! to ask a question would otherwise block until the process is killed.
//!
//! Everything here blocks. Callers reach it from `ferret-ui` on the background
//! executor; nothing in this module may be called from a frame.

use std::ffi::OsStr;
use std::io::{Read, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config;
use crate::errors::{GitError, Result};

/// What one invocation produced. `code` is `None` only if the platform reports
/// no exit status, which on both targets means it was signalled.
pub(crate) struct Output {
    pub(crate) status_ok: bool,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl Output {
    /// git's own message, trimmed and capped for display (D-51).
    ///
    /// stderr first because that is where git puts the reason; stdout is the
    /// fallback for the handful of commands that explain themselves there.
    pub(crate) fn message(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let text = text.trim();
        let text = if text.is_empty() {
            String::from_utf8_lossy(&self.stdout).trim().to_string()
        } else {
            text.to_string()
        };
        cap(&conclusion_first(&text))
    }
}

/// Move git's `fatal:` / `error:` line to the front, keeping every other line
/// in order.
///
/// D-51 says git's message is shown unedited, and this does not edit it —
/// nothing is dropped, reworded or summarised. It reorders, because git writes
/// for a person reading a terminal top to bottom: a refused `pull --ff-only`
/// prints eight `hint:` lines of advice *before* the one line that says what
/// happened. In a toast that buries the answer, and the toast is the only place
/// this text is ever seen.
fn conclusion_first(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(at) = lines
        .iter()
        .position(|line| line.starts_with("fatal:") || line.starts_with("error:"))
    else {
        return text.to_string();
    };
    if at == 0 {
        return text.to_string();
    }
    let mut ordered = Vec::with_capacity(lines.len());
    ordered.push(lines[at]);
    ordered.extend(
        lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| (i != at).then_some(*line)),
    );
    ordered.join("\n")
}

/// Trim git's output to something a notification can hold without becoming a
/// log viewer, on a character boundary rather than a byte one.
pub(crate) fn cap(text: &str) -> String {
    if text.chars().count() <= config::MAX_MESSAGE_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(config::MAX_MESSAGE_CHARS).collect();
    out.push('…');
    out
}

/// Run `git <args>` in `dir` and wait for it, killing it past `timeout`.
///
/// `label` is what the error messages call the operation ("status", "push") —
/// a fixed string rather than the argument vector, so a message never leaks a
/// path or a commit message into a toast.
pub(crate) fn run(
    dir: &Path,
    label: &'static str,
    args: &[&OsStr],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    harden(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(GitError::NotInstalled);
        }
        Err(source) => {
            return Err(GitError::Spawn {
                command: label,
                dir: dir.to_path_buf(),
                source,
            });
        }
    };

    // Both pipes are drained on their own threads. Reading them in sequence
    // deadlocks the moment git fills the one that is not being read — which is
    // exactly what a status of a large repository does.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_capped(out_pipe.as_mut()));
    let err_reader = std::thread::spawn(move || read_capped(err_pipe.as_mut()));

    // Dropping the handle at the end of the closure is what gives git EOF;
    // without it a `commit -F -` waits forever for a message that has already
    // been written.
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        });
    }

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // The readers end as soon as the pipes close, which the
                    // kill guarantees — so this cannot leak a thread.
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(GitError::TimedOut {
                        command: label,
                        dir: dir.to_path_buf(),
                        seconds: timeout.as_secs(),
                    });
                }
                std::thread::sleep(config::WAIT_POLL);
            }
            Err(source) => {
                return Err(GitError::Spawn {
                    command: label,
                    dir: dir.to_path_buf(),
                    source,
                });
            }
        }
    };

    let stdout = out_reader.join().unwrap_or(Err(Capped));
    let stderr = err_reader.join().unwrap_or(Err(Capped));
    let (Ok(stdout), Ok(stderr)) = (stdout, stderr) else {
        return Err(GitError::OutputTooLarge {
            command: label,
            limit: config::MAX_OUTPUT_BYTES,
        });
    };

    Ok(Output {
        status_ok: status.success(),
        stdout,
        stderr,
    })
}

/// Run, and turn a non-zero exit into [`GitError::Failed`] carrying git's own
/// message. For the operations where "git said no" is the interesting outcome.
pub(crate) fn run_checked(
    dir: &Path,
    label: &'static str,
    args: &[&OsStr],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<Output> {
    let output = run(dir, label, args, stdin, timeout)?;
    if output.status_ok {
        return Ok(output);
    }
    let message = output.message();
    Err(GitError::Failed {
        command: label,
        message: if message.is_empty() {
            "git gave no reason.".to_string()
        } else {
            message
        },
    })
}

/// The child overran [`config::MAX_OUTPUT_BYTES`].
struct Capped;

fn read_capped(pipe: Option<&mut impl Read>) -> std::result::Result<Vec<u8>, Capped> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut buffer = Vec::new();
    // `take` rather than `read_to_end`: an unbounded read is how a repository
    // with a million untracked files takes the window process with it.
    let read = pipe
        .take(config::MAX_OUTPUT_BYTES as u64 + 1)
        .read_to_end(&mut buffer);
    match read {
        Ok(_) if buffer.len() > config::MAX_OUTPUT_BYTES => Err(Capped),
        // A broken pipe here means the child was killed, which the caller
        // already knows about and reports as the timeout it was.
        Ok(_) | Err(_) => Ok(buffer),
    }
}

/// D-44: every invocation is non-interactive, and says so to git.
///
/// Without this a credential prompt, an SSH passphrase request or a hook that
/// opens an editor blocks a child that has no terminal to answer on — and the
/// operation hangs with no output until its timeout, which reads to the user as
/// Ferret being broken rather than as git needing something.
fn harden(command: &mut Command) {
    command
        // Never ask on the terminal we do not have. git then fails fast with a
        // message that names the credential it wanted, which is the honest
        // outcome (D-51) and points at the terminal panel.
        .env("GIT_TERMINAL_PROMPT", "0")
        // A hook or a config that wants an editor gets one that exits happily
        // rather than one that waits.
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        // Ferret's own commit messages arrive on stdin, so nothing here should
        // ever consult a message template.
        .env_remove("GIT_INDEX_FILE");
    no_console_window(command);
}

/// Stop a console window flashing over the desktop on every status read.
///
/// `git.exe` is a console application, so Windows creates a console for it
/// unless told not to — and this runs several times a minute behind a file
/// watcher. `CREATE_NO_WINDOW` is `0x0800_0000`.
#[cfg(windows)]
fn no_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn no_console_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_capped_on_a_character_boundary() {
        // Multi-byte on purpose: a byte-wise truncation panics here.
        let long: String = std::iter::repeat_n('é', config::MAX_MESSAGE_CHARS + 50).collect();
        let capped = cap(&long);
        assert_eq!(capped.chars().count(), config::MAX_MESSAGE_CHARS + 1);
        assert!(capped.ends_with('…'));

        // Short text is returned untouched — git's message, unedited (D-51).
        let short = "fatal: unable to access 'https://example.test/': Recv failure";
        assert_eq!(cap(short), short);
    }

    #[test]
    fn stderr_wins_over_stdout_and_is_trimmed() {
        let output = Output {
            status_ok: false,
            stdout: b"Everything up-to-date\n".to_vec(),
            stderr: b"  fatal: could not read Username\n\n".to_vec(),
        };
        assert_eq!(output.message(), "fatal: could not read Username");

        // …and stdout is the fallback for the commands that explain themselves
        // there instead.
        let quiet = Output {
            status_ok: false,
            stdout: b"nothing to commit, working tree clean\n".to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(quiet.message(), "nothing to commit, working tree clean");
    }

    #[test]
    fn the_conclusion_leads_and_nothing_is_dropped() {
        // A real refused `git pull --ff-only`: eight lines of advice, and the
        // line that says what happened is last.
        let refused = "hint: Diverging branches can't be fast-forwarded, you need to either:\n\
                       hint:\n\
                       hint: git merge --no-ff\n\
                       fatal: Not possible to fast-forward, aborting.";
        let message = conclusion_first(refused);
        assert!(
            message.starts_with("fatal: Not possible to fast-forward, aborting."),
            "{message}"
        );
        // Every hint survives, in its original order, below the conclusion.
        assert!(message.contains("hint: git merge --no-ff"));
        assert_eq!(message.lines().count(), refused.lines().count());
        assert_eq!(
            message.lines().nth(1),
            Some(refused.lines().next().unwrap())
        );

        // Already leading with it, or not naming one at all, is left alone.
        let leading = "fatal: could not read Username\nhint: try again";
        assert_eq!(conclusion_first(leading), leading);
        let plain = "Everything up-to-date";
        assert_eq!(conclusion_first(plain), plain);
    }
}
