use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize};

use crate::engine;
use crate::shell::{ShellKind, change_directory_command};
use crate::{ActiveTerminalBackend, Result, TerminalError, TerminalOptions, TerminalSnapshot};

const DEFAULT_COLUMNS: u16 = 120;
const DEFAULT_ROWS: u16 = 24;

/// One long-lived shell and its terminal state (D-28).
pub struct TerminalSession {
    snapshot: Arc<RwLock<TerminalSnapshot>>,
    engine: Sender<EngineCommand>,
    backend: ActiveTerminalBackend,
    shell_kind: ShellKind,
    /// When the PTY last produced output. The Tier 2 safety check in §6 is an
    /// idle window, so somebody has to hold the clock; the engine thread is the
    /// only place that sees every byte.
    last_output: Arc<Mutex<Instant>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

impl TerminalSession {
    /// Start a session running an explicit program instead of a login shell.
    ///
    /// This is what F-13 uses to run the user's own `ssh` (D-07): the PTY, the
    /// engine, the reader thread and the snapshot machinery are identical, and
    /// only the command differs. `cwd` is the *local* directory the process
    /// starts in, which for an SSH session only matters for relative key paths.
    ///
    /// The shell dialect is reported as POSIX because that is what a remote
    /// login shell is overwhelmingly likely to be, and it is the only dialect
    /// whose `cd` is safe to send blind — see `shell::change_directory_command`.
    pub fn spawn_command(
        program: &Path,
        args: &[String],
        cwd: &Path,
        options: &TerminalOptions,
    ) -> Result<Self> {
        let mut command = CommandBuilder::new(program);
        for arg in args {
            command.arg(arg);
        }
        command.cwd(cwd);
        command.env("TERM", &options.term);
        Self::spawn_with(
            command,
            program.to_path_buf(),
            ShellKind::Posix,
            cwd,
            options,
        )
    }

    pub fn spawn(directory: &Path, options: &TerminalOptions) -> Result<Self> {
        let shell = options.shell.clone().unwrap_or_else(default_shell);
        let mut command = CommandBuilder::new(&shell);
        command.cwd(directory);
        command.env("TERM", &options.term);
        #[cfg(not(windows))]
        command.arg("-l");

        let kind = ShellKind::detect(&shell);
        Self::spawn_with(command, shell, kind, directory, options)
    }

    /// The half both spawns share: the PTY, the engine, the reader thread.
    ///
    /// Only the command and the reported dialect differ between a login shell
    /// and an `ssh` process, so everything else lives here rather than in two
    /// copies that could drift.
    fn spawn_with(
        command: CommandBuilder,
        program: PathBuf,
        shell_kind: ShellKind,
        directory: &Path,
        options: &TerminalOptions,
    ) -> Result<Self> {
        let Engine {
            commands: engine,
            snapshot,
            backend,
            last_output,
        } = spawn_engine(options)?;
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

        let shell = program;
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
        engine
            .send(EngineCommand::AttachWriter(writer.clone()))
            .map_err(|_| engine_stopped())?;
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
            shell_kind,
            last_output,
            writer,
            master: pair.master,
            killer,
        })
    }

    pub fn backend(&self) -> ActiveTerminalBackend {
        self.backend
    }

    /// The dialect of the shell that actually started, which is what decides
    /// how a path may be handed to it.
    pub fn shell_kind(&self) -> ShellKind {
        self.shell_kind
    }

    /// How long the PTY has been silent. Zero output is the closest thing a
    /// Tier 2 session has to "no program is running" (D-26).
    pub fn idle_for(&self) -> Duration {
        self.last_output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .elapsed()
    }

    pub fn snapshot(&self) -> TerminalSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Inject a `cd` into the running shell.
    ///
    /// Whether this is *safe* to call is the caller's decision — it depends on
    /// the alternate-screen flag and the input line, which only the panel
    /// tracks (§6). This only guarantees the line is correct for the shell.
    pub fn change_directory(&self, directory: &Path) -> Result<()> {
        let command = change_directory_command(self.shell_kind, directory)?;
        self.write(command.as_bytes())
    }

    /// Inject a `cd` to a path on the other side of an SSH session.
    ///
    /// Always the POSIX form, never the dialect this session was told it has:
    /// what is running is the *remote* shell, which Ferret has no way to
    /// classify. Sending `Set-Location` to it because the local machine runs
    /// PowerShell would be a guess with nothing behind it.
    pub fn change_remote_directory(&self, directory: &str) -> Result<()> {
        self.write(crate::shell::posix_change_directory(directory).as_bytes())
    }

    /// Write pasted text, wrapped in bracketed-paste markers when the program
    /// has asked for them (mode 2004).
    ///
    /// Without the wrapper an embedded escape would be read as a keypress, so
    /// the unbracketed form strips them: pasting into a plain `read` should
    /// never move the cursor or change a mode.
    pub fn paste(&self, text: &str) -> Result<()> {
        // A shell reads one command per carriage return, and a pasted `\r\n`
        // would submit twice.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.snapshot().bracketed_paste {
            self.write(b"\x1b[200~")?;
            self.write(text.as_bytes())?;
            return self.write(b"\x1b[201~");
        }
        let sanitized: String = text.chars().filter(|c| *c != '\x1b').collect();
        self.write(sanitized.as_bytes())
    }

    pub fn resize(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<()> {
        let (reply, response) = mpsc::channel();
        self.enqueue_resize(columns, rows, pixel_width, pixel_height, Some(reply))?;
        response.recv().map_err(|_| {
            TerminalError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "terminal engine stopped",
            ))
        })?
    }

    pub fn resize_async(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<()> {
        self.enqueue_resize(columns, rows, pixel_width, pixel_height, None)
    }

    fn enqueue_resize(
        &self,
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
        reply: Option<Sender<Result<()>>>,
    ) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width,
                pixel_height,
            })
            .map_err(|error| TerminalError::Io(std::io::Error::other(error.to_string())))?;
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
            })
    }
}

enum EngineCommand {
    AttachWriter(Arc<Mutex<Box<dyn Write + Send>>>),
    Advance(Vec<u8>),
    Resize {
        columns: u16,
        rows: u16,
        cell_width: u32,
        cell_height: u32,
        reply: Option<Sender<Result<()>>>,
    },
}

/// What the engine thread hands back once it has a working VT.
struct Engine {
    commands: Sender<EngineCommand>,
    snapshot: Arc<RwLock<TerminalSnapshot>>,
    backend: ActiveTerminalBackend,
    last_output: Arc<Mutex<Instant>>,
}

fn spawn_engine(options: &TerminalOptions) -> Result<Engine> {
    let (commands, receiver) = mpsc::channel();
    let (initialized, initialization) = mpsc::channel();
    let snapshot = Arc::new(RwLock::new(TerminalSnapshot::default()));
    let worker_snapshot = snapshot.clone();
    let last_output = Arc::new(Mutex::new(Instant::now()));
    let worker_last_output = last_output.clone();
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
            let mut pty_writer: Option<Arc<Mutex<Box<dyn Write + Send>>>> = None;
            while let Ok(command) = receiver.recv() {
                match command {
                    EngineCommand::AttachWriter(writer) => pty_writer = Some(writer),
                    EngineCommand::Advance(bytes) => {
                        *worker_last_output
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Instant::now();
                        let replies = engine.advance(&bytes);
                        if let Some(writer) = &pty_writer {
                            write_engine_replies(writer, replies);
                        }
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
                        if let Some(reply) = reply {
                            let _ = reply.send(result);
                        }
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
    Ok(Engine {
        commands,
        snapshot,
        backend,
        last_output,
    })
}

fn write_engine_replies(writer: &Arc<Mutex<Box<dyn Write + Send>>>, replies: Vec<Vec<u8>>) {
    if replies.is_empty() {
        return;
    }
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    for reply in replies {
        if writer.write_all(&reply).is_err() {
            return;
        }
    }
    let _ = writer.flush();
}

fn engine_stopped() -> TerminalError {
    TerminalError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "terminal engine stopped",
    ))
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.killer.kill();
    }
}

fn default_shell() -> PathBuf {
    #[cfg(windows)]
    {
        // Windows PowerShell is present on every supported Windows release.
        // What it is *not* is a given: `terminal.shell` may point at cmd.exe or
        // Git Bash, which is why `ShellKind` is read back off the spawned path
        // rather than assumed from the platform.
        PathBuf::from("powershell.exe")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
    }
}
