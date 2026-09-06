//! GPUI-free SSH connection, SFTP mount, and transfer services for Ferret.
//!
//! This crate owns persistent connection records, credentials, host-key
//! verification, and the Tokio bridge required by the SSH stack. UI code
//! consumes the small public surface re-exported here; it does not reach into
//! these implementation modules.

pub mod config;

mod errors;
mod host_keys;
mod record;
mod runtime;
mod secrets;
mod session;
mod sftp;
mod ssh_config;
mod store;
mod transfer;

pub use errors::{PromptKind, RemoteError, Result};
pub use host_keys::{
    HostKeyVerdict, fingerprint, into_result as verify_verdict, known_hosts_path,
    learn as learn_host_key, verify as verify_host_key,
};
pub use record::{Connection, ConnectionId};
pub use runtime::{detach, is_started as runtime_is_started, shutdown as shutdown_runtime, spawn};
pub use secrets::{
    Secret, SecretKind, forget as forget_secret, forget_all as forget_all_secrets,
    load as load_secret, store as store_secret,
};
pub use session::{Credentials, HostKeyPolicy, Session, connect, connect_with_known_hosts};
pub use sftp::SftpVfs;
pub use store::{ConnectionStore, connections_path, write_store};
pub use transfer::{
    Conflict, Direction, Transfer, TransferDestination, TransferEvent, TransferSource,
};
