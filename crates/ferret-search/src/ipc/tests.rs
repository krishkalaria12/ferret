//! Protocol behaviour at the boundary two separately built binaries meet.
//!
//! Deliberately in-memory rather than over a real socket. The failures worth
//! protecting against here are a client and a daemon that disagree — a stale
//! build, an interrupted upgrade, a nonce from a daemon that has since been
//! killed — and an end-to-end test spawns two copies of the *same* build,
//! which is the one configuration that never fails.

use std::sync::atomic::AtomicBool;

use ferret_vfs::VfsPath;

use super::server::SearchHost;
use super::*;
use crate::{
    InProcessSearchService, IndexStatus, SearchGeneration, SearchRequest, SearchService as _,
    SearchSession,
};

struct StubHost;

impl SearchHost for StubHost {
    fn start(&self, request: SearchRequest) -> Result<SearchSession> {
        InProcessSearchService.start(request)
    }

    fn status(&self) -> IndexStatus {
        IndexStatus {
            available: true,
            files: 12,
            ..IndexStatus::default()
        }
    }
}

/// Runs one client-side conversation against the server logic.
fn exchange(messages: &[ClientMessage], nonce: &str) -> Vec<ServerMessage> {
    let mut request = Vec::new();
    for message in messages {
        write_frame(&mut request, message).unwrap();
    }
    let mut response = Vec::new();
    let shutdown = AtomicBool::new(false);
    super::server::converse(
        &mut request.as_slice(),
        &mut response,
        &StubHost,
        nonce,
        &shutdown,
        || Ok(()),
    )
    .unwrap();

    let mut reader = response.as_slice();
    let mut received = Vec::new();
    while let Some(message) = read_frame::<_, ServerMessage>(&mut reader).unwrap() {
        received.push(message);
    }
    received
}

fn hello(protocol: u32, nonce: &str) -> ClientMessage {
    ClientMessage::Hello {
        protocol,
        nonce: nonce.to_owned(),
    }
}

#[test]
fn a_matching_handshake_is_welcomed_and_answered() {
    let received = exchange(
        &[hello(PROTOCOL_VERSION, "token"), ClientMessage::Status],
        "token",
    );

    assert!(matches!(received[0], ServerMessage::Welcome { .. }));
    match &received[1] {
        ServerMessage::Status(status) => assert_eq!(status.files, 12),
        other => panic!("expected a status, got {other:?}"),
    }
}

#[test]
fn a_version_mismatch_is_reported_with_the_daemons_own_version() {
    // The client has to be able to say *which side* is stale. A bare failure
    // would leave "the search helper broke" as the only available message.
    let received = exchange(&[hello(PROTOCOL_VERSION + 1, "token")], "token");

    assert!(matches!(
        received[..],
        [ServerMessage::Incompatible {
            protocol: PROTOCOL_VERSION
        }]
    ));
}

#[test]
fn a_nonce_from_a_previous_launch_is_refused() {
    let received = exchange(
        &[hello(PROTOCOL_VERSION, "stale"), ClientMessage::Status],
        "fresh",
    );

    assert!(
        matches!(received[..], [ServerMessage::Rejected { .. }]),
        "and the Status that followed it is never answered: {received:?}"
    );
}

#[test]
fn a_connection_that_skips_the_handshake_gets_nothing_else() {
    let received = exchange(&[ClientMessage::Status], "token");

    assert!(matches!(received[..], [ServerMessage::Rejected { .. }]));
}

#[test]
fn shutdown_is_only_honoured_after_a_valid_handshake() {
    let shutdown = AtomicBool::new(false);
    let mut request = Vec::new();
    write_frame(&mut request, &hello(PROTOCOL_VERSION, "token")).unwrap();
    write_frame(&mut request, &ClientMessage::Shutdown).unwrap();
    super::server::converse(
        &mut request.as_slice(),
        &mut Vec::new(),
        &StubHost,
        "token",
        &shutdown,
        || Ok(()),
    )
    .unwrap();

    assert!(shutdown.load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn a_search_streams_events_and_ends_on_a_terminal_one() {
    let root = std::env::temp_dir();
    let request = SearchRequest {
        generation: SearchGeneration(3),
        scope: crate::SearchScope {
            kind: crate::SearchScopeKind::CurrentFolder,
            roots: vec![VfsPath::from(root.as_path())],
        },
        // A pattern no temporary file plausibly has: the test is about the
        // shape of the stream, not about what it found.
        name: crate::NameQuery::substring("zzz-ferret-ipc-no-such-name-zzz"),
        content: None,
        filters: crate::SearchFilters::default(),
        limits: crate::SearchLimits::default(),
    };
    let received = exchange(
        &[
            hello(PROTOCOL_VERSION, "token"),
            ClientMessage::Search(Box::new(request)),
        ],
        "token",
    );

    assert!(matches!(received[0], ServerMessage::Welcome { .. }));
    let last = received.last().expect("a search always terminates");
    assert!(
        matches!(
            last,
            ServerMessage::Event(event)
                if matches!(**event, crate::SearchEvent::Finished(_))
        ),
        "got {last:?}"
    );
}

#[test]
fn a_frame_larger_than_the_limit_is_refused_rather_than_allocated() {
    // A corrupt or hostile length prefix must not make the reader reserve a
    // gigabyte before it discovers the connection is broken.
    let mut framed = Vec::new();
    framed.extend_from_slice(&(crate::config::MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
    let error = read_frame::<_, ServerMessage>(&mut framed.as_slice()).unwrap_err();

    assert!(
        matches!(error, SearchError::DaemonTransport { .. }),
        "{error}"
    );
}

#[test]
fn a_truncated_frame_is_an_error_but_a_clean_end_of_stream_is_not() {
    let mut complete = Vec::new();
    write_frame(&mut complete, &ClientMessage::Status).unwrap();
    let truncated = &complete[..complete.len() - 1];

    assert!(
        read_frame::<_, ClientMessage>(&mut &complete[complete.len()..])
            .unwrap()
            .is_none()
    );
    assert!(read_frame::<_, ClientMessage>(&mut &truncated[..]).is_err());
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_survives_the_wire() {
    // Search is the feature that surfaces these files, so the one encoding it
    // must not lose is the one `to_string_lossy` would destroy.
    use std::os::unix::ffi::OsStrExt as _;
    let raw = std::ffi::OsStr::from_bytes(b"/tmp/inv\xffalid");
    let path = VfsPath::from(std::path::Path::new(raw));

    let encoded = serde_json::to_vec(&path).unwrap();
    let decoded: VfsPath = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded, path);
}
