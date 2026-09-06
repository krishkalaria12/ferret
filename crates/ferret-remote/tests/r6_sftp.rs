//! End-to-end SFTP against a real OpenSSH server: browse, mutate, transfer.
//!
//! Opt-in for the same reason as `r1_sshd`: normal CI does not need a server.
//! Set `FERRET_REMOTE_SSHD_TEST=1` to run it — see `tests/sshd/README.md`.
//!
//! This is the suite that answers the phase's "done when": a remote folder
//! lists, renames and deletes like a local one, and a file survives a round
//! trip byte for byte.

#[path = "support/sshd.rs"]
mod sshd;

use std::sync::Arc;

use ferret_remote::{
    Conflict, Connection, Credentials, Direction, HostKeyPolicy, RemoteError, SftpVfs,
    TransferDestination, TransferEvent, TransferSource, connect_with_known_hosts, shutdown_runtime,
};
use ferret_vfs::{ListingDetail, MountId, RemoveMode, Vfs, VfsPath};
use futures::StreamExt as _;

const MOUNT: MountId = MountId(1);

/// Connect to the disposable server and open an SFTP session on it.
fn mount(server: &sshd::Sshd) -> Arc<SftpVfs> {
    let connection = Connection {
        host: "127.0.0.1".into(),
        port: Some(server.port()),
        user: server.user().into(),
        key_path: Some(server.private_key()),
        ..Connection::default()
    };
    let known_hosts = server.known_hosts();

    // The first dial is refused: the harness's key is not recorded. That is
    // the behaviour `r1_sshd` covers; here it is just the way in.
    let fingerprint = match smol::block_on(connect_with_known_hosts(
        connection.clone(),
        Credentials::default(),
        HostKeyPolicy::Strict,
        Some(known_hosts.clone()),
    )) {
        Err(RemoteError::UnknownHostKey { fingerprint, .. }) => fingerprint,
        Err(error) => panic!("expected an unknown key, got {error}"),
        Ok(_) => panic!("an unrecorded host must not authenticate"),
    };
    let session = smol::block_on(connect_with_known_hosts(
        connection,
        Credentials::default(),
        HostKeyPolicy::AcceptFingerprint(fingerprint),
        Some(known_hosts),
    ))
    .expect("connect with the accepted fingerprint");

    let sftp = smol::block_on(session.open_sftp(MOUNT, "127.0.0.1")).expect("open the subsystem");
    // The session has to outlive this function: dropping it closes the
    // connection the SFTP channel is riding on.
    std::mem::forget(session);
    Arc::new(sftp)
}

fn remote(path: &str) -> VfsPath {
    VfsPath::remote(MOUNT, path)
}

/// A scratch directory on the server, under the account's own home.
fn scratch(sftp: &Arc<SftpVfs>, name: &str) -> VfsPath {
    let home = smol::block_on(sftp.home()).expect("resolve the remote home");
    let dir = home.join(name);
    // Left over from a previous run, if any.
    let _ = smol::block_on(sftp.remove(&dir, RemoveMode::Permanent));
    smol::block_on(sftp.create_dir(&dir)).expect("create the scratch directory");
    dir
}

fn list_names(sftp: &Arc<SftpVfs>, dir: &VfsPath) -> Vec<String> {
    smol::block_on(async {
        let mut stream = sftp.read_dir(dir, ListingDetail::Full);
        let mut names = Vec::new();
        while let Some(batch) = stream.next().await {
            for entry in batch.expect("a listing batch") {
                names.push(entry.name().to_string());
            }
        }
        names.sort();
        names
    })
}

#[test]
fn a_remote_folder_lists_creates_renames_and_deletes() {
    let Some(server) = sshd::Sshd::start_if_enabled() else {
        eprintln!("skipped: set FERRET_REMOTE_SSHD_TEST=1 to run the sftp suite");
        return;
    };
    let sftp = mount(&server);
    let dir = scratch(&sftp, "ferret-r6-mutate");

    // An empty directory lists as empty rather than failing.
    assert!(list_names(&sftp, &dir).is_empty());

    // Create, and see it in the listing.
    let file = dir.join("report.txt");
    smol::block_on(sftp.create_file(&file)).expect("create a file");
    let nested = dir.join("inner");
    smol::block_on(sftp.create_dir(&nested)).expect("create a directory");
    assert_eq!(list_names(&sftp, &dir), ["inner", "report.txt"]);

    // Creating over an existing name must be refused, not silently accepted:
    // "New File" may never blank something already there.
    let again = smol::block_on(sftp.create_file(&file));
    assert!(again.is_err(), "EXCL must refuse an existing name");

    // The entry carries its metadata from the listing itself, with no second
    // round trip — the reason the backend declares `metadata_in_listing`.
    let entries = smol::block_on(async {
        let mut stream = sftp.read_dir(&dir, ListingDetail::Full);
        let mut all = Vec::new();
        while let Some(batch) = stream.next().await {
            all.extend(batch.expect("batch"));
        }
        all
    });
    let report = entries
        .iter()
        .find(|entry| entry.name() == "report.txt")
        .expect("report.txt");
    assert!(
        report.metadata.is_some(),
        "attributes must arrive with the names"
    );
    assert!(entries.iter().all(|entry| entry.name() != "."));

    // Rename.
    let renamed = dir.join("renamed.txt");
    smol::block_on(sftp.rename(&file, &renamed)).expect("rename");
    assert_eq!(list_names(&sftp, &dir), ["inner", "renamed.txt"]);

    // Trash is refused rather than quietly deleting: there is none on a
    // server, and the UI has to be told so it can say so.
    let trashed = smol::block_on(sftp.remove(&renamed, RemoveMode::Trash));
    assert!(trashed.is_err(), "a server has no trash");
    assert_eq!(list_names(&sftp, &dir), ["inner", "renamed.txt"]);

    // A tree deletes depth-first, because SFTP has no recursive remove.
    smol::block_on(sftp.create_file(&nested.join("deep.txt"))).expect("create a nested file");
    smol::block_on(sftp.remove(&dir, RemoveMode::Permanent)).expect("delete the tree");
    assert!(!smol::block_on(sftp.exists(&dir)));

    shutdown_runtime();
}

#[test]
fn a_file_survives_a_round_trip_byte_for_byte() {
    let Some(server) = sshd::Sshd::start_if_enabled() else {
        eprintln!("skipped: set FERRET_REMOTE_SSHD_TEST=1 to run the sftp suite");
        return;
    };
    let sftp = mount(&server);
    let dir = scratch(&sftp, "ferret-r6-transfer");
    let VfsPath::Remote {
        path: remote_dir, ..
    } = &dir
    else {
        panic!("the scratch directory is remote");
    };

    // Big enough to cross several chunk windows, so the pipelined write path
    // and the short-read handling are both exercised rather than a single
    // buffer round trip.
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let local = std::env::temp_dir().join(format!("ferret-r6-{}.bin", std::process::id()));
    std::fs::write(&local, &payload).expect("write the local source");

    // Upload.
    let job = sftp.clone().transfer(
        Direction::Upload,
        vec![TransferSource::Local(local.clone())],
        TransferDestination::RemoteDir(remote_dir.to_string()),
        Conflict::Replace,
    );
    let (copied, failed) = drain(job);
    assert_eq!((copied, failed), (1, 0), "the upload copied one file");

    let name = local.file_name().unwrap().to_string_lossy().into_owned();
    let uploaded = dir.join(&name);
    assert!(smol::block_on(sftp.exists(&uploaded)));
    // Nothing may be left under the part name.
    assert!(
        !list_names(&sftp, &dir)
            .iter()
            .any(|n| n.ends_with(".ferret-part"))
    );

    // The bytes on the server are the bytes that were sent.
    let read_back = smol::block_on(sftp.read(&uploaded, payload.len() + 1)).expect("read it back");
    assert_eq!(read_back.len(), payload.len());
    assert!(read_back == payload, "the uploaded bytes differ");

    // Download into a fresh directory.
    let download_dir = std::env::temp_dir().join(format!("ferret-r6-down-{}", std::process::id()));
    std::fs::create_dir_all(&download_dir).expect("create the download directory");
    let job = sftp.clone().transfer(
        Direction::Download,
        vec![TransferSource::Remote(format!(
            "{}/{name}",
            remote_dir.trim_end_matches('/')
        ))],
        TransferDestination::LocalDir(download_dir.clone()),
        Conflict::Replace,
    );
    let (copied, failed) = drain(job);
    assert_eq!((copied, failed), (1, 0), "the download copied one file");

    let round_tripped = std::fs::read(download_dir.join(&name)).expect("read the download");
    assert!(
        round_tripped == payload,
        "the file changed on the way there and back"
    );

    // Skip leaves an existing file alone rather than overwriting it.
    std::fs::write(download_dir.join(&name), b"untouched").expect("replace the download");
    let job = sftp.clone().transfer(
        Direction::Download,
        vec![TransferSource::Remote(format!(
            "{}/{name}",
            remote_dir.trim_end_matches('/')
        ))],
        TransferDestination::LocalDir(download_dir.clone()),
        Conflict::Skip,
    );
    let (copied, _) = drain(job);
    assert_eq!(copied, 0, "Skip must not copy over an existing file");
    assert_eq!(
        std::fs::read(download_dir.join(&name)).unwrap(),
        b"untouched"
    );

    let _ = smol::block_on(sftp.remove(&dir, RemoveMode::Permanent));
    std::fs::remove_file(&local).ok();
    std::fs::remove_dir_all(&download_dir).ok();
    shutdown_runtime();
}

#[test]
fn a_listing_stops_when_its_reader_goes_away() {
    let Some(server) = sshd::Sshd::start_if_enabled() else {
        eprintln!("skipped: set FERRET_REMOTE_SSHD_TEST=1 to run the sftp suite");
        return;
    };
    let sftp = mount(&server);
    let dir = scratch(&sftp, "ferret-r6-cancel");
    for i in 0..50 {
        smol::block_on(sftp.create_file(&dir.join(&format!("f{i}.txt")))).expect("create");
    }

    // Cancel-by-drop is the `Vfs` contract, and it has to hold across the
    // runtime boundary: dropping the stream must not leave a task walking a
    // directory nobody is reading.
    smol::block_on(async {
        let mut stream = sftp.read_dir(&dir, ListingDetail::Full);
        let first = stream.next().await;
        assert!(first.is_some(), "at least one batch arrives");
        drop(stream);
    });

    // The session is still usable afterwards, which is what proves the
    // cancellation closed its handle rather than wedging the channel.
    assert!(!list_names(&sftp, &dir).is_empty());

    let _ = smol::block_on(sftp.remove(&dir, RemoveMode::Permanent));
    shutdown_runtime();
}

#[test]
fn a_path_from_another_mount_is_refused() {
    let Some(server) = sshd::Sshd::start_if_enabled() else {
        eprintln!("skipped: set FERRET_REMOTE_SSHD_TEST=1 to run the sftp suite");
        return;
    };
    let sftp = mount(&server);

    // Two servers can both have /etc. A backend must never answer for a path
    // belonging to a different mount.
    let foreign = VfsPath::remote(MountId(99), "/etc");
    assert!(smol::block_on(sftp.metadata(&foreign)).is_err());
    assert!(!smol::block_on(sftp.exists(&foreign)));

    // Nor for a path that could climb out of the mount.
    assert!(smol::block_on(sftp.metadata(&remote("/srv/../../etc/passwd"))).is_err());

    shutdown_runtime();
}

/// Run a transfer to completion, returning `(copied, failed)`.
fn drain(job: ferret_remote::Transfer) -> (usize, usize) {
    smol::block_on(async {
        let mut outcome = (0, 0);
        while let Some(event) = job.next().await {
            match event {
                TransferEvent::Failed { name, reason } => {
                    eprintln!("transfer failure on {name:?}: {reason}");
                }
                TransferEvent::Finished { copied, failed, .. } => {
                    outcome = (copied, failed);
                    break;
                }
                _ => {}
            }
        }
        outcome
    })
}
