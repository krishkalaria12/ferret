//! Opt-in R-1 tests against a disposable OpenSSH server.
//!
//! Normal CI does not need Docker. Set `FERRET_REMOTE_SSHD_TEST=1` to enable
//! this suite; see `tests/sshd/README.md` for the exact command.

#[path = "support/sshd.rs"]
mod sshd;

use ferret_remote::{
    Connection, Credentials, HostKeyPolicy, RemoteError, connect, connect_with_known_hosts,
    shutdown_runtime,
};

#[test]
fn disposable_server_is_refused_until_its_host_key_is_trusted() {
    let Some(server) = sshd::Sshd::start_if_enabled() else {
        eprintln!("skipped: set FERRET_REMOTE_SSHD_TEST=1 to run the sshd suite");
        return;
    };
    assert!(server.known_hosts().is_file());

    let connection = Connection {
        host: "127.0.0.1".into(),
        port: Some(server.port()),
        user: server.user().into(),
        key_path: Some(server.private_key()),
        ..Connection::default()
    };
    let known_hosts = server.known_hosts();
    let result = smol::block_on(connect_with_known_hosts(
        connection.clone(),
        Credentials::default(),
        HostKeyPolicy::Strict,
        Some(known_hosts.clone()),
    ));
    let fingerprint = match result {
        Err(RemoteError::UnknownHostKey { fingerprint, .. }) => fingerprint,
        Err(error) => panic!("expected an unknown key, got {error}"),
        Ok(_) => panic!("an unrecorded disposable host must not be authenticated"),
    };

    let session = smol::block_on(connect_with_known_hosts(
        connection.clone(),
        Credentials::default(),
        HostKeyPolicy::AcceptFingerprint(fingerprint),
        Some(known_hosts.clone()),
    ))
    .expect("the exact accepted fingerprint connects");
    smol::block_on(session.disconnect()).expect("disconnect accepted session");

    server.replace_with_different_host_key();
    let result = smol::block_on(connect_with_known_hosts(
        connection,
        Credentials::default(),
        HostKeyPolicy::Strict,
        Some(known_hosts),
    ));
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("a changed recorded key must not authenticate"),
    };
    assert!(
        matches!(error, RemoteError::HostKeyMismatch { .. }),
        "{error}"
    );
    shutdown_runtime();
    assert!(!ferret_remote::runtime_is_started());
}

#[test]
fn configured_trusted_server_connects_and_disconnects_cleanly() {
    let Some(connection) = sshd::trusted_connection_from_env() else {
        eprintln!(
            "skipped: set FERRET_TEST_SSH_HOST, FERRET_TEST_SSH_USER and FERRET_TEST_SSH_KEY"
        );
        return;
    };

    smol::block_on(async {
        let session = connect(connection, Credentials::default(), HostKeyPolicy::Strict)
            .await
            .expect("connect to the explicitly configured trusted server");
        assert!(session.is_connected());
        session.disconnect().await.expect("disconnect cleanly");
        assert!(!session.is_connected());
    });
}
