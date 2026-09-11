//! CI-safe checks for the R-1 runtime and host-key boundary.
//!
//! The real-server cases live in `r1_sshd.rs` and are deliberately opt-in.

use ferret_remote::{
    HostKeyVerdict, RemoteError, runtime_is_started, shutdown_runtime, spawn, verify_verdict,
};
use std::path::PathBuf;

#[test]
fn runtime_is_lazy_and_bridges_a_non_tokio_executor() {
    assert!(
        !runtime_is_started(),
        "loading ferret-remote must not eagerly create its Tokio runtime"
    );

    let thread = std::thread::spawn(|| {
        smol::block_on(async {
            spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                42
            })
            .await
        })
    });

    assert_eq!(
        thread
            .join()
            .expect("caller thread must not panic")
            .unwrap(),
        42
    );
    assert!(runtime_is_started());
    shutdown_runtime();
    assert!(!runtime_is_started(), "shutdown releases the Tokio runtime");

    // Shutdown is not a one-way global poison: a later connection can lazily
    // create the service again.
    assert_eq!(smol::block_on(spawn(async { 7 })).unwrap(), 7);
    shutdown_runtime();
}

#[test]
fn unknown_and_changed_keys_have_distinct_non_bypassable_results() {
    let unknown = verify_verdict(
        HostKeyVerdict::Unknown {
            fingerprint: "SHA256:unknown".into(),
            algorithm: "ssh-ed25519".into(),
        },
        "server.example",
        2222,
    )
    .unwrap_err();
    assert!(matches!(
        &unknown,
        RemoteError::UnknownHostKey {
            host,
            port,
            fingerprint,
            algorithm,
        } if host == "server.example" && *port == 2222
            && fingerprint == "SHA256:unknown"
            && algorithm == "ssh-ed25519"
    ));
    assert!(
        !unknown.is_fatal_to_session(),
        "an unknown key is a question for the user"
    );

    let changed = verify_verdict(
        HostKeyVerdict::Changed {
            fingerprint: "SHA256:offered".into(),
            file: PathBuf::from("/tmp/ferret-r1-known-hosts"),
        },
        "server.example",
        2222,
    )
    .unwrap_err();
    assert!(matches!(changed, RemoteError::HostKeyMismatch { .. }));
    assert!(
        changed.is_fatal_to_session(),
        "a changed key must never expose a continue-anyway path"
    );
}

#[test]
fn production_remote_code_does_not_block_an_executor() {
    let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&source_dir).expect("read ferret-remote sources") {
        let path = entry.expect("read source entry").path();
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        // Unit tests may use a synchronous executor at the process boundary.
        // Only inspect the production portion of each module.
        let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
        assert!(
            !production.contains("block_on("),
            "{} calls block_on in production code",
            path.display()
        );
    }
}
