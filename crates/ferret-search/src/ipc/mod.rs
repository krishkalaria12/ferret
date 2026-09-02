//! The versioned local protocol between Ferret and `ferret-searchd`.
//!
//! Three properties are load-bearing and none of them is the message set:
//!
//! 1. **It is local only.** A Unix-domain socket on macOS, a named pipe on
//!    Windows. No TCP listener is ever created, so nothing here is reachable
//!    from another machine even by accident.
//! 2. **It is versioned at the first frame.** A client that speaks a different
//!    protocol than the daemon it found must discover that in the handshake
//!    and fall back to in-process live search — not halfway through decoding a
//!    result batch. Ferret and the daemon are separate binaries and *will* be
//!    mismatched during development and during an interrupted upgrade.
//! 3. **A dropped connection is a cancellation.** The daemon does not read
//!    while it is producing; it discovers a gone client when a write fails.
//!    That keeps cancellation on one mechanism instead of two that can
//!    disagree.
//!
//! Framing is a four-byte little-endian length followed by that many bytes of
//! JSON. JSON because these frames are read by a person far more often than
//! they are read by a profiler: the whole message set fits in a few hundred
//! bytes per batch, and being able to `nc` the socket and see what the daemon
//! said has repeatedly been worth more than the bytes.

pub mod client;
pub mod endpoint;
pub mod server;

use std::io::{Read, Write};

use serde::{Serialize, de::DeserializeOwned};

use crate::config::MAX_FRAME_BYTES;
use crate::{IndexStatus, Result, SearchError, SearchEvent, SearchRequest};

/// The wire contract's version.
///
/// Not a tunable: this is the number both sides compare in the handshake.
/// Bump it whenever a message changes shape in a way an older peer would
/// misread — which includes adding a variant an older peer cannot decode.
pub const PROTOCOL_VERSION: u32 = 1;

/// Everything a client may say.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ClientMessage {
    /// Always the first frame. Carries the version to compare and the
    /// per-launch nonce read from the daemon's private nonce file, which is
    /// what proves the client can already read the daemon's runtime directory.
    Hello { protocol: u32, nonce: String },
    /// Run one query. The daemon then streams [`ServerMessage::Event`] until a
    /// `Finished` event, and the connection is used for nothing else.
    Search(Box<SearchRequest>),
    /// One-shot index status, for the planner and the settings surface.
    Status,
    /// Ask the daemon to exit once it has finished in-flight work.
    Shutdown,
}

/// Everything the daemon may say.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ServerMessage {
    Welcome {
        protocol: u32,
    },
    /// The handshake failed on version. Carries the daemon's version so the
    /// client can say which side is stale rather than "something went wrong".
    Incompatible {
        protocol: u32,
    },
    /// The handshake failed on the nonce, or the request was malformed.
    Rejected {
        message: String,
    },
    Event(Box<SearchEvent>),
    Status(Box<IndexStatus>),
}

/// Writes one length-prefixed JSON frame.
pub(crate) fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value).map_err(transport)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(SearchError::DaemonTransport {
            message: format!(
                "a {} byte message exceeds the {MAX_FRAME_BYTES} byte frame limit",
                body.len()
            ),
        });
    }
    let length = u32::try_from(body.len()).expect("checked against MAX_FRAME_BYTES above");
    writer.write_all(&length.to_le_bytes()).map_err(transport)?;
    writer.write_all(&body).map_err(transport)?;
    writer.flush().map_err(transport)
}

/// The three ways a frame read can end.
pub(crate) enum FrameRead<T> {
    Frame(T),
    /// The peer hung up cleanly *between* frames.
    Eof,
    /// The caller asked to stop while waiting for a new frame to begin. Only
    /// ever returned at a frame boundary, so the stream is never left half
    /// consumed.
    Aborted,
}

/// Reads one length-prefixed JSON frame, blocking until it arrives.
pub(crate) fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>> {
    match read_frame_with(reader, || true)? {
        FrameRead::Frame(value) => Ok(Some(value)),
        FrameRead::Eof | FrameRead::Aborted => Ok(None),
    }
}

/// Reads one frame, giving the caller a say each time the socket goes quiet.
///
/// `keep_waiting` is consulted only when no byte of the next frame has arrived
/// yet — that is the single point at which abandoning the read cannot desync
/// the stream. This is how a cancelled search stops reading promptly without a
/// second channel to carry the cancellation.
pub(crate) fn read_frame_with<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    mut keep_waiting: impl FnMut() -> bool,
) -> Result<FrameRead<T>> {
    let mut header = [0u8; 4];
    match fill(reader, &mut header, &mut keep_waiting)? {
        Fill::Filled => {}
        Fill::Eof => return Ok(FrameRead::Eof),
        Fill::Aborted => return Ok(FrameRead::Aborted),
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(SearchError::DaemonTransport {
            message: format!("frame claims {length} bytes, past the {MAX_FRAME_BYTES} byte limit"),
        });
    }
    let mut body = vec![0u8; length];
    // Once the header is in, the rest of the frame is not optional: abandoning
    // it would leave the next reader parsing a body as a header.
    match fill(reader, &mut body, &mut || true)? {
        Fill::Filled => {}
        Fill::Eof | Fill::Aborted => {
            return Err(SearchError::DaemonTransport {
                message: "the connection ended in the middle of a message".into(),
            });
        }
    }
    serde_json::from_slice(&body)
        .map(FrameRead::Frame)
        .map_err(transport)
}

enum Fill {
    Filled,
    Eof,
    Aborted,
}

/// Fills `buffer`, distinguishing "the peer hung up cleanly before this frame"
/// from "the peer hung up inside it". The first is normal shutdown; the second
/// is a truncated message and has to be an error.
fn fill<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    keep_waiting: &mut impl FnMut() -> bool,
) -> Result<Fill> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Ok(Fill::Eof),
            Ok(0) => {
                return Err(SearchError::DaemonTransport {
                    message: "the connection ended in the middle of a message".into(),
                });
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if filled == 0 && !keep_waiting() {
                    return Ok(Fill::Aborted);
                }
            }
            Err(error) => return Err(transport(error)),
        }
    }
    Ok(Fill::Filled)
}

pub(crate) fn transport(error: impl std::fmt::Display) -> SearchError {
    SearchError::DaemonTransport {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests;
