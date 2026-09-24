//! The Ferret side of the local protocol, and the supervision around it.
//!
//! The contract this module keeps is that **search never stops working**. The
//! daemon may be missing, stale, crashed, mid-upgrade, or speaking a protocol
//! this build does not know; every one of those degrades to the in-process
//! live walk with a warning, and none of them is allowed to surface as a
//! failed query.
//!
//! Supervision is deliberately small: connect, and if nothing answers, launch
//! the helper once and wait a bounded time for it. Launches are counted, and
//! after [`DAEMON_LAUNCH_ATTEMPTS`] consecutive failures the client stops
//! trying for the rest of the session — a daemon that crashes on startup must
//! not be relaunched once per keystroke.

use std::io::BufWriter;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferret_vfs::VfsPath;
use interprocess::local_socket::prelude::*;
use parking_lot::Mutex;

use super::endpoint;
use super::{
    ClientMessage, FrameRead, PROTOCOL_VERSION, ServerMessage, read_frame, read_frame_with,
    transport, write_frame,
};
use crate::config::{
    DAEMON_CONNECT_RETRY, DAEMON_HANDSHAKE_TIMEOUT, DAEMON_LAUNCH_ATTEMPTS, DAEMON_LAUNCH_TIMEOUT,
    EVENT_BUFFER,
};
use crate::{
    IndexStatus, Result, SearchError, SearchEvent, SearchFinished, SearchRequest, SearchSession,
};

/// How long the reader waits before asking whether its session still exists.
///
/// This is the cancellation granularity of a daemon-backed query when the
/// daemon has gone quiet. In practice cancellation is noticed on the next
/// event, and this only bounds the pathological case where the daemon is
/// producing nothing at all.
const READ_POLL: Duration = Duration::from_millis(200);

/// Environment override for the helper's location, for development and for
/// packaging layouts that do not put the two binaries side by side.
const BINARY_OVERRIDE: &str = "FERRET_SEARCHD";

/// A supervised connection to `ferret-searchd`.
///
/// Holds no socket between queries on purpose. A connection is one query, so
/// there is no long-lived stream to reason about across a daemon restart, and
/// a daemon that died between two searches costs one failed connect rather
/// than a resynchronisation protocol.
#[derive(Debug, Default)]
pub struct DaemonClient {
    supervisor: Mutex<Supervisor>,
}

#[derive(Debug, Default)]
struct Supervisor {
    consecutive_failures: u32,
    /// Set once the client has given up for this process. Carries the reason
    /// so the UI can say *why* it is searching the slow way.
    disabled: Option<String>,
}

impl DaemonClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Why the daemon is not being used, if it is not.
    pub fn unavailable_reason(&self) -> Option<String> {
        self.supervisor.lock().disabled.clone()
    }

    /// Asks the daemon what it has indexed.
    ///
    /// `None` means "no usable daemon", which the planner must read as "index
    /// nothing" rather than as an error.
    pub fn status(&self) -> Option<IndexStatus> {
        let stream = self.connect().ok()?;
        let mut writer = BufWriter::new(&stream);
        write_frame(&mut writer, &ClientMessage::Status).ok()?;
        drop(writer);
        let mut reader = std::io::BufReader::new(&stream);
        match read_frame(&mut reader).ok()? {
            Some(ServerMessage::Status(status)) => Some(*status),
            _ => None,
        }
    }

    /// Asks a running daemon to exit, without launching one that is not.
    ///
    /// `attach` rather than `connect`: "stop the helper" must never be the
    /// thing that starts it.
    pub fn shutdown(&self) -> bool {
        let Ok(stream) = attach() else {
            return false;
        };
        let mut writer = BufWriter::new(&stream);
        write_frame(&mut writer, &ClientMessage::Shutdown).is_ok()
    }

    /// Starts one query on the daemon.
    pub fn start(&self, request: SearchRequest) -> Result<SearchSession> {
        let generation = request.generation;
        let stream = self.connect()?;
        {
            let mut writer = BufWriter::new(&stream);
            write_frame(&mut writer, &ClientMessage::Search(Box::new(request)))?;
        }
        stream
            .set_recv_timeout(Some(READ_POLL))
            .map_err(transport)?;

        let (events_tx, events_rx) = async_channel::bounded(EVENT_BUFFER);
        let cancelled = Arc::new(AtomicBool::new(false));
        let reader_cancelled = Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("ferret-search-ipc".into())
            .spawn(move || pump(stream, events_tx, reader_cancelled, generation))
            .map_err(transport)?;

        Ok(SearchSession::new(generation, events_rx, cancelled))
    }

    /// Connects, launching and waiting for the helper if nothing is listening.
    fn connect(&self) -> Result<LocalSocketStream> {
        {
            let supervisor = self.supervisor.lock();
            if let Some(reason) = &supervisor.disabled {
                return Err(SearchError::DaemonTransport {
                    message: reason.clone(),
                });
            }
        }
        match self.connect_once() {
            Ok(stream) => {
                self.supervisor.lock().consecutive_failures = 0;
                Ok(stream)
            }
            Err(error) => {
                let mut supervisor = self.supervisor.lock();
                supervisor.consecutive_failures += 1;
                if supervisor.consecutive_failures >= DAEMON_LAUNCH_ATTEMPTS {
                    supervisor.disabled = Some(format!(
                        "Ferret's search index helper failed to start {} times, so searches are \
                         scanning the filesystem directly. Last error: {error}",
                        supervisor.consecutive_failures
                    ));
                }
                Err(error)
            }
        }
    }

    fn connect_once(&self) -> Result<LocalSocketStream> {
        if let Ok(stream) = attach() {
            return Ok(stream);
        }
        launch()?;
        let deadline = Instant::now() + DAEMON_LAUNCH_TIMEOUT;
        let mut last = None;
        while Instant::now() < deadline {
            match attach() {
                Ok(stream) => return Ok(stream),
                Err(error) => last = Some(error),
            }
            std::thread::sleep(DAEMON_CONNECT_RETRY);
        }
        Err(last.unwrap_or_else(|| SearchError::DaemonTransport {
            message: "the search index helper did not accept connections in time".into(),
        }))
    }
}

/// Connects to an already-listening daemon and completes the handshake.
fn attach() -> Result<LocalSocketStream> {
    let name = endpoint::socket_name()?;
    let stream = LocalSocketStream::connect(name).map_err(transport)?;
    stream
        .set_recv_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))
        .map_err(transport)?;
    stream
        .set_send_timeout(Some(DAEMON_HANDSHAKE_TIMEOUT))
        .map_err(transport)?;
    let nonce = endpoint::read_nonce()?;
    {
        let mut writer = BufWriter::new(&stream);
        write_frame(
            &mut writer,
            &ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                nonce,
            },
        )?;
    }
    let mut reader = std::io::BufReader::new(&stream);
    match read_frame(&mut reader)? {
        Some(ServerMessage::Welcome { .. }) => {
            stream.set_recv_timeout(None).map_err(transport)?;
            stream.set_send_timeout(None).map_err(transport)?;
            Ok(stream)
        }
        Some(ServerMessage::Incompatible { protocol }) => Err(SearchError::ProtocolMismatch {
            expected: PROTOCOL_VERSION,
            found: protocol,
        }),
        Some(ServerMessage::Rejected { message }) => Err(SearchError::DaemonRejected { message }),
        Some(_) | None => Err(SearchError::DaemonTransport {
            message: "the search index helper did not complete the handshake".into(),
        }),
    }
}

/// Spawns the helper detached from Ferret's own streams.
///
/// Not killed when Ferret exits: the daemon owns its own idle shutdown, and a
/// second Ferret window connecting to a still-warm index is the whole point of
/// it being a separate process. Duplicate launches are harmless — the loser of
/// the bind race exits immediately.
fn launch() -> Result<()> {
    let binary = binary_path()?;
    Command::new(&binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| SearchError::DaemonLaunch {
            path: VfsPath::from(binary),
            message: error.to_string(),
        })
}

fn binary_path() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os(BINARY_OVERRIDE) {
        return Ok(PathBuf::from(override_path));
    }
    let exe = std::env::current_exe().map_err(|error| SearchError::DaemonLaunch {
        path: VfsPath::local("ferret-searchd"),
        message: format!("Ferret couldn't locate its own binary: {error}"),
    })?;
    let name = if cfg!(windows) {
        "ferret-searchd.exe"
    } else {
        "ferret-searchd"
    };
    let sibling = exe
        .parent()
        .map(|dir| dir.join(name))
        .unwrap_or_else(|| PathBuf::from(name));
    if !sibling.exists() {
        return Err(SearchError::DaemonLaunch {
            path: VfsPath::from(sibling),
            message: "the helper is not installed next to Ferret".into(),
        });
    }
    Ok(sibling)
}

/// Forwards daemon events onto the session's channel until either side stops.
fn pump(
    stream: LocalSocketStream,
    events: async_channel::Sender<SearchEvent>,
    cancelled: Arc<AtomicBool>,
    generation: crate::SearchGeneration,
) {
    let mut reader = std::io::BufReader::new(&stream);
    loop {
        let keep_waiting = || !cancelled.load(Ordering::Acquire) && !events.is_closed();
        let frame = match read_frame_with::<_, ServerMessage>(&mut reader, keep_waiting) {
            Ok(FrameRead::Frame(message)) => message,
            Ok(FrameRead::Eof) => {
                // The daemon vanished mid-query. Say so rather than letting
                // the UI sit on "Searching…" forever.
                let _ = events.send_blocking(SearchEvent::Finished(SearchFinished::Failed {
                    generation,
                    message: "Ferret's search index helper stopped responding.".into(),
                }));
                return;
            }
            Ok(FrameRead::Aborted) => return,
            Err(error) => {
                let _ = events.send_blocking(SearchEvent::Finished(SearchFinished::Failed {
                    generation,
                    message: error.to_string(),
                }));
                return;
            }
        };
        let ServerMessage::Event(event) = frame else {
            continue;
        };
        let terminal = matches!(*event, SearchEvent::Finished(_));
        if events.send_blocking(*event).is_err() || terminal {
            return;
        }
    }
    // Dropping `stream` here is what tells the daemon to stop producing.
}
