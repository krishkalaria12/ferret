//! Which shell is running, and how to hand it a path (§6, "Quoting").
//!
//! The rule the operating system is *not* allowed to decide: a Windows user who
//! points `terminal.shell` at Git Bash gets POSIX quoting, and a `cmd.exe`
//! session gets `cd /d` — selecting syntax from `cfg!(windows)` alone sends
//! PowerShell verbs to shells that have never heard of them.

use std::path::Path;

use crate::{Result, TerminalError};

/// The command dialect of the shell a session actually spawned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellKind {
    /// bash, zsh, fish, sh — anything that takes `cd -- '…'`.
    #[default]
    Posix,
    PowerShell,
    Cmd,
}

impl ShellKind {
    /// Classify by executable name, which is all a spawned path tells us.
    ///
    /// Unknown names fall to POSIX rather than to the platform default: a shell
    /// nobody here recognises is far more likely to be another POSIX one than
    /// it is to be a second PowerShell.
    pub fn detect(shell: &Path) -> Self {
        // Split on both separators rather than asking `Path`: a settings file
        // is portable and may hold a Windows path while `std` is treating `\`
        // as an ordinary character.
        let full = shell.to_string_lossy().to_ascii_lowercase();
        let name = full
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&full)
            .trim_end_matches(".exe");
        match name {
            "powershell" | "pwsh" => Self::PowerShell,
            "cmd" => Self::Cmd,
            _ => Self::Posix,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Posix => "This shell",
            Self::PowerShell => "PowerShell",
            Self::Cmd => "cmd.exe",
        }
    }
}

/// One line that moves `shell` to `directory`, quoted for that shell.
///
/// The POSIX form opens with a space on purpose: with `HISTCONTROL=ignorespace`
/// or `setopt HIST_IGNORE_SPACE`, it keeps Ferret's own `cd`s out of the user's
/// history. The `--` stops a directory named `-P` from being read as a flag.
pub(crate) fn change_directory_command(kind: ShellKind, directory: &Path) -> Result<String> {
    let path = directory.to_string_lossy();
    Ok(match kind {
        ShellKind::Posix => format!(" cd -- '{}'\n", path.replace('\'', "'\\''")),
        // `-LiteralPath` keeps `[`, `]` and `*` in a real directory name from
        // being read as wildcards.
        ShellKind::PowerShell => {
            format!(
                "Set-Location -LiteralPath '{}'\r\n",
                path.replace('\'', "''")
            )
        }
        // cmd quotes with `"` and has no escape for one inside, so a path
        // containing a quote is refused rather than mangled. `/d` is what makes
        // it cross drives.
        ShellKind::Cmd => {
            if path.contains('"') {
                return Err(TerminalError::Unquotable {
                    shell: kind.name(),
                    directory: directory.to_path_buf(),
                });
            }
            format!("cd /d \"{path}\"\r\n")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shells_are_classified_by_executable_name() {
        assert_eq!(ShellKind::detect(Path::new("/bin/zsh")), ShellKind::Posix);
        assert_eq!(
            ShellKind::detect(Path::new(r"C:\Program Files\Git\bin\bash.exe")),
            ShellKind::Posix
        );
        assert_eq!(
            ShellKind::detect(Path::new(r"C:\Windows\System32\cmd.exe")),
            ShellKind::Cmd
        );
        assert_eq!(
            ShellKind::detect(Path::new("PowerShell.exe")),
            ShellKind::PowerShell
        );
        assert_eq!(ShellKind::detect(Path::new("pwsh")), ShellKind::PowerShell);
    }

    #[test]
    fn each_shell_gets_its_own_quoting() {
        let awkward = PathBuf::from("/tmp/it's here");
        assert_eq!(
            change_directory_command(ShellKind::Posix, &awkward).unwrap(),
            " cd -- '/tmp/it'\\''s here'\n"
        );
        assert_eq!(
            change_directory_command(ShellKind::PowerShell, &awkward).unwrap(),
            "Set-Location -LiteralPath '/tmp/it''s here'\r\n"
        );
        assert_eq!(
            change_directory_command(ShellKind::Cmd, &awkward).unwrap(),
            "cd /d \"/tmp/it's here\"\r\n"
        );
    }

    #[test]
    fn cmd_refuses_a_path_it_cannot_quote() {
        let error = change_directory_command(ShellKind::Cmd, Path::new(r#"C:\a "quoted" name"#))
            .unwrap_err();
        assert!(matches!(error, TerminalError::Unquotable { .. }), "{error}");
    }

    /// The Windows default is PowerShell, so its verb must not leak into the
    /// POSIX form that every other shell in the table needs.
    #[test]
    fn powershell_syntax_never_reaches_a_posix_shell() {
        let command = change_directory_command(ShellKind::Posix, Path::new("/tmp")).unwrap();
        assert!(!command.contains("Set-Location"), "{command}");
    }
}
