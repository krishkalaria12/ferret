use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::engine::Engine;
use crate::{Result, TerminalError, TerminalSnapshot};

const DEFAULT_COLUMNS: u16 = 120;
const DEFAULT_ROWS: u16 = 24;

/// One long-lived shell and its terminal state (D-28).
pub struct TerminalSession {
    snapshot: Arc<RwLock<TerminalSnapshot>>,
    engine: Arc<Mutex<Engine>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl TerminalSession {
    pub fn spawn(directory: &Path) -> Result<Self> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|source| TerminalError::OpenPty {
                directory: directory.to_path_buf(),
                source: std::io::Error::other(source.to_string()),
            })?;

        let shell = default_shell();
        let mut command = CommandBuilder::new(&shell);
        command.cwd(directory);
        command.env("TERM", "xterm-256color");
        #[cfg(not(windows))]
        command.arg("-l");

        let mut child =
            pair.slave
                .spawn_command(command)
                .map_err(|source| TerminalError::SpawnShell {
                    shell: shell.to_string_lossy().into_owned(),
                    directory: directory.to_path_buf(),
                    message: source.to_string(),
                })?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Io(std::io::Error::other(error.to_string())))?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(|error| {
            TerminalError::Io(std::io::Error::other(error.to_string()))
        })?));
        let engine = Arc::new(Mutex::new(Engine::new(
            DEFAULT_COLUMNS as usize,
            DEFAULT_ROWS as usize,
        )));
        let snapshot = Arc::new(RwLock::new(TerminalSnapshot::default()));
        let reader_engine = engine.clone();
        let reader_snapshot = snapshot.clone();
        thread::Builder::new()
            .name("ferret-terminal-reader".into())
            .spawn(move || {
                let mut bytes = [0_u8; 8192];
                while let Ok(count) = reader.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    let mut engine = reader_engine.lock().unwrap_or_else(|e| e.into_inner());
                    engine.advance(&bytes[..count]);
                    *reader_snapshot.write().unwrap_or_else(|e| e.into_inner()) = engine.snapshot();
                }
                let _ = child.wait();
            })
            .map_err(TerminalError::Io)?;

        Ok(Self {
            snapshot,
            engine,
            writer,
            master: pair.master,
            killer,
        })
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn change_directory(&self, directory: &Path) -> Result<()> {
        let command = change_directory_command(directory);
        self.write(command.as_bytes())
    }

    pub fn resize(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width,
                pixel_height,
            })
            .map_err(|error| TerminalError::Io(std::io::Error::other(error.to_string())))?;
        let mut engine = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        engine.resize(columns as usize, rows as usize);
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = engine.snapshot();
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn default_shell() -> PathBuf {
    #[cfg(windows)]
    {
        // Windows PowerShell is present on every supported Windows release;
        // using it also keeps the literal-path `cd` injection below valid.
        PathBuf::from("powershell.exe")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
    }
}

#[cfg(not(windows))]
fn change_directory_command(directory: &Path) -> String {
    let escaped = directory.to_string_lossy().replace('\'', "'\\''");
    format!("cd -- '{escaped}'\n")
}

#[cfg(windows)]
fn change_directory_command(directory: &Path) -> String {
    let escaped = directory.to_string_lossy().replace('\'', "''");
    format!("Set-Location -LiteralPath '{escaped}'\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn quotes_posix_paths_for_cd() {
        assert_eq!(
            change_directory_command(Path::new("/tmp/it's here")),
            "cd -- '/tmp/it'\\''s here'\n"
        );
    }
}
