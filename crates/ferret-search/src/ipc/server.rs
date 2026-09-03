//! The daemon side of the local protocol.
//!
//! One thread per connection, blocking I/O, and no shared mutable state beyond
//! the hosted service. Connections are counted in the tens at the very most —
//! one per in-flight query plus the occasional status poll — so a reactor here
//! would buy nothing and cost a runtime.

use std::io::BufWriter;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use interprocess::local_socket::{ListenerNonblockingMode, ListenerOptions, prelude::*};

use super::endpoint;
use super::{ClientMessage, PROTOCOL_VERSION, ServerMessage, read_frame, transport, write_frame};
use crate::config::DAEMON_IDLE_SHUTDOWN;
use crate::{IndexStatus, Result, SearchError, SearchEvent, SearchRequest, SearchSession};

/// How long a connection may take to complete its handshake.
///
/// A real client sends `Hello` immediately. Anything that connects and then
/// says nothing is holding a thread, and on Windows an unaccepted dead pipe
/// instance also blocks later clients — so it is dropped rather than waited on.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the accept loop wakes to check for idle shutdown.
const ACCEPT_POLL: Duration = Duration::from_millis(200);

/// What the daemon exposes over the socket.
///
/// Deliberately narrower than [`crate::SearchService`]: the daemon also has to
/// answer for the index itself, and the trait is what keeps `main.rs` from
/// needing to know anything about framing.
pub trait SearchHost: Send + Sync + 'static {
    fn start(&self, request: SearchRequest) -> Result<SearchSession>;
    fn status(&self) -> IndexStatus;
}

/// Why [`serve`] returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeOutcome {
    /// Another daemon already owns the socket. This process should exit
    /// quietly and successfully — it is a duplicate launch, not a failure.
    AlreadyRunning,
    /// The idle timer expired with no connections.
    IdleShutdown,
    /// A client asked the daemon to exit.
    ShutdownRequested,
}

/// Binds the local socket and serves until idle or asked to stop.
///
/// Single-instance enforcement is the bind itself rather than a separate lock
/// file, because a lock file records an *intent* to listen while the bound
/// socket records the fact. The one case the bind cannot decide on its own is
/// a Unix socket file left behind by a killed daemon: the file exists, so bind
/// fails, but nothing is listening. That is distinguished by trying to connect
/// before removing it.
pub fn serve(host: Arc<dyn SearchHost>) -> Result<ServeOutcome> {
    let listener = match bind() {
        Ok(listener) => listener,
        Err(BindError::AlreadyRunning) => return Ok(ServeOutcome::AlreadyRunning),
        Err(BindError::Failed(error)) => return Err(error),
    };

    let nonce = endpoint::fresh_nonce();
    endpoint::write_nonce(&nonce)?;
    let nonce = Arc::new(nonce);
    let shutdown = Arc::new(AtomicBool::new(false));
    let live_connections = Arc::new(AtomicUsize::new(0));
    let mut last_activity = Instant::now();

    loop {
        match listener.accept() {
            Ok(stream) => {
                last_activity = Instant::now();
                live_connections.fetch_add(1, Ordering::AcqRel);
                let host = Arc::clone(&host);
                let nonce = Arc::clone(&nonce);
                let shutdown = Arc::clone(&shutdown);
                let live_connections = Arc::clone(&live_connections);
                std::thread::Builder::new()
                    .name("ferret-searchd-conn".into())
                    .spawn(move || {
                        // A connection failing is a client's problem, never
                        // the daemon's: the index and every other client have
                        // to survive it.
                        let _ = handle(stream, host.as_ref(), &nonce, &shutdown);
                        live_connections.fetch_sub(1, Ordering::AcqRel);
                    })
                    .map_err(transport)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if shutdown.load(Ordering::Acquire) {
                    return Ok(ServeOutcome::ShutdownRequested);
                }
                if live_connections.load(Ordering::Acquire) > 0 {
                    last_activity = Instant::now();
                } else if last_activity.elapsed() >= DAEMON_IDLE_SHUTDOWN {
                    return Ok(ServeOutcome::IdleShutdown);
                }
                std::thread::sleep(ACCEPT_POLL);
            }
            // One refused connection is not a reason to stop serving the
            // others; the loop is the daemon's whole lifetime.
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
    }
}

enum BindError {
    AlreadyRunning,
    Failed(SearchError),
}

fn bind() -> std::result::Result<LocalSocketListener, BindError> {
    let name = endpoint::socket_name().map_err(BindError::Failed)?;
    let options = || {
        ListenerOptions::new()
            .name(endpoint::socket_name().expect("the name resolved once already"))
            .nonblocking(ListenerNonblockingMode::Accept)
    };
    match options().create_sync() {
        Ok(listener) => return Ok(listener),
        Err(error) if error.kind() != std::io::ErrorKind::AddrInUse => {
            return Err(BindError::Failed(transport(error)));
        }
        Err(_) => {}
    }

    // The address is taken. If something answers, it is a live daemon and this
    // process is a duplicate. If nothing answers, the socket file outlived its
    // daemon and is safe to clear.
    if LocalSocketStream::connect(name).is_ok() {
        return Err(BindError::AlreadyRunning);
    }
    if let Ok(path) = endpoint::socket_path() {
        let _ = std::fs::remove_file(path);
    }
    options().create_sync().map_err(|error| {
        BindError::Failed(transport(format!(
            "the search socket is in use and could not be reclaimed: {error}"
        )))
    })
}

fn handle(
    stream: LocalSocketStream,
    host: &dyn SearchHost,
    nonce: &str,
    shutdown: &AtomicBool,
) -> Result<()> {
    stream
        .set_recv_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(transport)?;
    let mut reader = std::io::BufReader::new(&stream);
    let mut writer = BufWriter::new(&stream);
    let clear_timeout = || stream.set_recv_timeout(None).map_err(transport);
    converse(
        &mut reader,
        &mut writer,
        host,
        nonce,
        shutdown,
        clear_timeout,
    )
}

/// The protocol itself, over any pair of streams.
///
/// Split from [`handle`] so the conversation can be tested against in-memory
/// buffers. Every failure mode worth a test here — a version mismatch, a stale
/// nonce, a first message that is not `Hello` — is one that only ever happens
/// between two separately built binaries, which is exactly the situation an
/// end-to-end test is worst at reproducing.
pub(super) fn converse<R: std::io::Read, W: std::io::Write>(
    reader: &mut R,
    writer: &mut W,
    host: &dyn SearchHost,
    nonce: &str,
    shutdown: &AtomicBool,
    clear_read_timeout: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let hello: Option<ClientMessage> = read_frame(reader)?;
    match hello {
        Some(ClientMessage::Hello {
            protocol,
            nonce: offered,
        }) => {
            if protocol != PROTOCOL_VERSION {
                // Answering with our own version is the point: the client can
                // then say which side is stale instead of "the daemon broke".
                return write_frame(
                    writer,
                    &ServerMessage::Incompatible {
                        protocol: PROTOCOL_VERSION,
                    },
                );
            }
            if offered != nonce {
                return write_frame(
                    writer,
                    &ServerMessage::Rejected {
                        message: "that token belongs to a previous daemon launch".into(),
                    },
                );
            }
        }
        Some(_) => {
            return write_frame(
                writer,
                &ServerMessage::Rejected {
                    message: "the first message on a connection must be Hello".into(),
                },
            );
        }
        None => return Ok(()),
    }
    write_frame(
        writer,
        &ServerMessage::Welcome {
            protocol: PROTOCOL_VERSION,
        },
    )?;

    // A query can legitimately take minutes; only the handshake is on a clock.
    clear_read_timeout()?;

    match read_frame(reader)? {
        Some(ClientMessage::Search(request)) => stream_search(host, *request, writer),
        Some(ClientMessage::Status) => {
            write_frame(writer, &ServerMessage::Status(Box::new(host.status())))
        }
        Some(ClientMessage::Shutdown) => {
            shutdown.store(true, Ordering::Release);
            Ok(())
        }
        Some(ClientMessage::Hello { .. }) => write_frame(
            writer,
            &ServerMessage::Rejected {
                message: "the connection is already established".into(),
            },
        ),
        None => Ok(()),
    }
}

fn stream_search<W: std::io::Write>(
    host: &dyn SearchHost,
    request: SearchRequest,
    writer: &mut W,
) -> Result<()> {
    let generation = request.generation;
    let session = match host.start(request) {
        Ok(session) => session,
        Err(error) => {
            // A rejected query is still an answer. Sending it as a normal
            // terminal event means the client's event loop has exactly one
            // shape whether the daemon is present or not.
            return write_frame(
                writer,
                &ServerMessage::Event(Box::new(SearchEvent::Finished(
                    crate::SearchFinished::Failed {
                        generation,
                        message: error.to_string(),
                    },
                ))),
            );
        }
    };
    while let Some(event) = session.next_blocking() {
        let terminal = matches!(event, SearchEvent::Finished(_));
        // A write failure here is the client having gone away, which is how
        // cancellation reaches the daemon. Dropping `session` on the way out
        // stops the producers.
        if write_frame(writer, &ServerMessage::Event(Box::new(event))).is_err() || terminal {
            return Ok(());
        }
    }
    Ok(())
}
