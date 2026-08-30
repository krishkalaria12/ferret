use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::engine;
use crate::{ActiveTerminalBackend, Result, TerminalError, TerminalOptions, TerminalSnapshot};

const DEFAULT_COLUMNS: u16 = 120;
const DEFAULT_ROWS: u16 = 24;

/// One long-lived shell and its terminal state (D-28).
pub struct TerminalSession {
    snapshot: Arc<RwLock<TerminalSnapshot>>,
    engine: Sender<EngineCommand>,
    backend: ActiveTerminalBackend,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl TerminalSession {
    pub fn spawn(directory: &Path, options: &TerminalOptions) -> Result<Self> {
        let (engine, snapshot, backend) = spawn_engine(options)?;
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

        let shell = options.shell.clone().unwrap_or_else(default_shell);
        let mut command = CommandBuilder::new(&shell);
        command.cwd(directory);
        command.env("TERM", &options.term);
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
        let reader_engine = engine.clone();
        thread::Builder::new()
            .name("ferret-terminal-reader".into())
            .spawn(move || {
                let mut bytes = [0_u8; 8192];
                while let Ok(count) = reader.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    if reader_engine
                        .send(EngineCommand::Advance(bytes[..count].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = child.wait();
            })
            .map_err(TerminalError::Io)?;

        Ok(Self {
            snapshot,
            engine,
            backend,
            writer,
            master: pair.master,
            killer,
        })
    }

    pub fn backend(&self) -> ActiveTerminalBackend {
        self.backend
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
        let (reply, response) = mpsc::channel();
        self.engine
            .send(EngineCommand::Resize {
                columns,
                rows,
                cell_width: u32::from(pixel_width) / u32::from(columns.max(1)),
                cell_height: u32::from(pixel_height) / u32::from(rows.max(1)),
                reply,
            })
            .map_err(|_| {
                TerminalError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "terminal engine stopped",
                ))
            })?;
        response.recv().map_err(|_| {
            TerminalError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "terminal engine stopped",
            ))
        })?
    }
}

enum EngineCommand {
    Advance(Vec<u8>),
    Resize {
        columns: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
        reply: Sender<Result<()>>,
    },
}

fn spawn_engine(
    options: &TerminalOptions,
) -> Result<(
    Sender<EngineCommand>,
    Arc<RwLock<TerminalSnapshot>>,
    ActiveTerminalBackend,
)> {
    let (commands, receiver) = mpsc::channel();
    let (initialized, initialization) = mpsc::channel();
    let snapshot = Arc::new(RwLock::new(TerminalSnapshot::default()));
    let worker_snapshot = snapshot.clone();
    let requested = options.backend;
    let scrollback_lines = options.scrollback_lines;
    thread::Builder::new()
        .name("ferret-terminal-engine".into())
        .spawn(move || {
            let selected = engine::select_backend(requested);
            let created = selected.and_then(|backend| {
                engine::create(backend, DEFAULT_COLUMNS, DEFAULT_ROWS, scrollback_lines)
                    .map(|engine| (backend, engine))
                    .or_else(|error| {
                        if requested == crate::TerminalBackend::Auto
                            && backend == ActiveTerminalBackend::Ghostty
                        {
                            engine::create(
                                ActiveTerminalBackend::Alacritty,
                                DEFAULT_COLUMNS,
                                DEFAULT_ROWS,
                                scrollback_lines,
                            )
                            .map(|engine| (ActiveTerminalBackend::Alacritty, engine))
                        } else {
                            Err(error)
                        }
                    })
            });
            let (backend, mut engine) = match created {
                Ok(value) => value,
                Err(error) => {
                    let _ = initialized.send(Err(error));
                    return;
                }
            };
            if initialized.send(Ok(backend)).is_err() {
                return;
            }
            while let Ok(command) = receiver.recv() {
                match command {
                    EngineCommand::Advance(bytes) => {
                        engine.advance(&bytes);
                        if let Ok(next) = engine.snapshot() {
                            *worker_snapshot
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) = next;
                        }
                    }
                    EngineCommand::Resize {
                        columns,
                        rows,
                        cell_width,
                        cell_height,
                        reply,
                    } => {
                        let result = engine
                            .resize(columns, rows, cell_width, cell_height)
                            .and_then(|()| engine.snapshot());
                        let result = result.map(|next| {
                            *worker_snapshot
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) = next;
                        });
                        let _ = reply.send(result);
                    }
                }
            }
        })
        .map_err(TerminalError::Io)?;
    let backend = initialization.recv().map_err(|_| {
        TerminalError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "terminal engine failed during startup",
        ))
    })??;
    Ok((commands, snapshot, backend))
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
