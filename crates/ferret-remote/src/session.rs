//! SSH connection and authentication lifecycle (R-1).
//!
//! A [`Session`] is a small executor-neutral command handle. The `russh`
//! handle itself never leaves the crate's Tokio runtime, so GPUI and smol can
//! only await messages and can never accidentally drive network work on a
//! frame.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};

use crate::config::CONNECT_TIMEOUT;
use crate::{Connection, RemoteError, Result, Secret};

#[derive(Clone, Debug, Default)]
pub struct Credentials {
    pub password: Option<Secret>,
    pub key_passphrase: Option<Secret>,
}

/// Unknown keys are accepted only for the single connection attempt made
/// after a user confirms the displayed fingerprint.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HostKeyPolicy {
    #[default]
    Strict,
    AcceptFingerprint(String),
}

enum Command {
    OpenSftp(futures::channel::oneshot::Sender<Result<crate::SftpVfs>>),
    Disconnect(futures::channel::oneshot::Sender<()>),
}

/// A live authenticated SSH connection.
pub struct Session {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    connected: Arc<AtomicBool>,
}

impl Session {
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub async fn disconnect(&self) -> Result<()> {
        if !self.is_connected() {
            return Ok(());
        }
        let (tx, rx) = futures::channel::oneshot::channel();
        self.commands
            .send(Command::Disconnect(tx))
            .map_err(|_| RemoteError::Disconnected {
                host: "the remote server".into(),
            })?;
        rx.await.map_err(|_| RemoteError::Cancelled)
    }

    /// Open an SFTP channel on this authenticated SSH connection.
    ///
    /// Channel creation and protocol negotiation happen inside the remote
    /// runtime; callers only receive the executor-neutral VFS handle.
    pub async fn open_sftp(
        &self,
        mount: ferret_vfs::MountId,
        host: impl Into<String>,
    ) -> Result<crate::SftpVfs> {
        let host = host.into();
        let (tx, rx) = futures::channel::oneshot::channel();
        self.commands
            .send(Command::OpenSftp(tx))
            .map_err(|_| RemoteError::Disconnected { host: host.clone() })?;
        rx.await
            .map_err(|_| RemoteError::Cancelled)?
            .map(|opened| opened.with_mount_and_host(mount, host))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Drop is intentionally non-blocking. The actor owns the network
        // handle and observes command-channel closure as a clean shutdown.
    }
}

#[derive(Debug, thiserror::Error)]
enum ClientError {
    #[error(transparent)]
    Ssh(#[from] russh::Error),
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

struct Handler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    known_hosts: Option<PathBuf>,
}

impl client::Handler for Handler {
    type Error = ClientError;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key = offered.public_key();
        match crate::host_keys::verify_at(&self.host, self.port, &key, self.known_hosts.as_deref())?
        {
            crate::HostKeyVerdict::Known => Ok(true),
            crate::HostKeyVerdict::Unknown { fingerprint, .. } => match &self.policy {
                HostKeyPolicy::AcceptFingerprint(accepted) if accepted == &fingerprint => {
                    crate::host_keys::learn_at(
                        &self.host,
                        self.port,
                        &key,
                        self.known_hosts.as_deref(),
                    )?;
                    Ok(true)
                }
                _ => crate::verify_verdict(
                    crate::HostKeyVerdict::Unknown {
                        fingerprint,
                        algorithm: key.algorithm().to_string(),
                    },
                    &self.host,
                    self.port,
                )
                .map(|()| true)
                .map_err(Into::into),
            },
            verdict => crate::verify_verdict(verdict, &self.host, self.port)
                .map(|()| true)
                .map_err(Into::into),
        }
    }
}

/// Connect and authenticate without blocking the caller's executor.
pub async fn connect(
    connection: Connection,
    credentials: Credentials,
    host_key_policy: HostKeyPolicy,
) -> Result<Session> {
    connect_with_known_hosts(connection, credentials, host_key_policy, None).await
}

/// Connect using an explicit known-hosts file.
///
/// Normal application code uses [`connect`]. This variant keeps integration
/// tests and portable installations isolated from the user's SSH files.
pub async fn connect_with_known_hosts(
    connection: Connection,
    credentials: Credentials,
    host_key_policy: HostKeyPolicy,
    known_hosts: Option<PathBuf>,
) -> Result<Session> {
    crate::spawn(async move {
        connect_on_runtime(connection, credentials, host_key_policy, known_hosts).await
    })
    .await?
}

async fn connect_on_runtime(
    connection: Connection,
    credentials: Credentials,
    host_key_policy: HostKeyPolicy,
    known_hosts: Option<PathBuf>,
) -> Result<Session> {
    connection
        .validate()
        .map_err(|message| RemoteError::Connect {
            host: connection.host.clone(),
            port: connection.port(),
            message: message.into(),
        })?;
    let host = connection.host.clone();
    let port = connection.port();
    let handler = Handler {
        host: host.clone(),
        port,
        policy: host_key_policy,
        known_hosts,
    };
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(crate::config::REQUEST_TIMEOUT),
        keepalive_interval: Some(crate::config::KEEPALIVE_INTERVAL),
        keepalive_max: 3,
        ..Default::default()
    });
    let connect = client::connect(config, (host.as_str(), port), handler);
    let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(handle)) => handle,
        Ok(Err(ClientError::Remote(err))) => return Err(err),
        Ok(Err(ClientError::Ssh(err))) => return Err(connect_error(&host, port, err)),
        Err(_) => {
            return Err(RemoteError::Connect {
                host,
                port,
                message: "the connection timed out".into(),
            });
        }
    };

    let mut tried = Vec::new();
    if authenticate_agent(&mut handle, &connection.user).await {
        return Ok(start_actor(handle));
    }
    tried.push("SSH agent");

    let keys = candidate_keys(&connection);
    for path in keys {
        tried.push("private key");
        let key = match load_secret_key(
            &path,
            credentials.key_passphrase.as_ref().map(Secret::expose),
        ) {
            Ok(key) => key,
            Err(russh::keys::Error::KeyIsEncrypted) if credentials.key_passphrase.is_none() => {
                return Err(RemoteError::AuthPrompt {
                    host: host.clone(),
                    user: connection.user.clone(),
                    prompt: format!("the passphrase for {}", path.display()),
                    kind: crate::PromptKind::KeyPassphrase,
                });
            }
            Err(err) if connection.key_path.is_some() => {
                return Err(RemoteError::KeyFile {
                    path,
                    message: err.to_string(),
                });
            }
            Err(_) => continue,
        };
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        let result = handle
            .authenticate_publickey(
                connection.user.clone(),
                PrivateKeyWithHashAlg::new(Arc::new(key), hash),
            )
            .await;
        if result.is_ok_and(|r| r.success()) {
            return Ok(start_actor(handle));
        }
    }

    if let Some(password) = credentials.password {
        tried.push("password");
        if handle
            .authenticate_password(connection.user.clone(), password.expose())
            .await
            .is_ok_and(|r| r.success())
        {
            return Ok(start_actor(handle));
        }
    } else {
        return Err(RemoteError::AuthPrompt {
            host,
            user: connection.user,
            prompt: "an account password".into(),
            kind: crate::PromptKind::Password,
        });
    }

    Err(RemoteError::AuthFailed {
        host,
        user: connection.user,
        tried: tried.join(", "),
    })
}

fn candidate_keys(connection: &Connection) -> Vec<PathBuf> {
    if let Some(path) = &connection.key_path {
        return vec![expand_tilde(path)];
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| home.join(".ssh").join(name))
        .filter(|path| path.is_file())
        .collect()
}

fn expand_tilde(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

#[cfg(unix)]
async fn authenticate_agent(handle: &mut client::Handle<Handler>, user: &str) -> bool {
    let Ok(mut agent) = russh::keys::agent::client::AgentClient::connect_env().await else {
        return false;
    };
    let Ok(identities) = agent.request_identities().await else {
        return false;
    };
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .is_ok_and(|r| r.success())
        {
            return true;
        }
    }
    false
}

#[cfg(windows)]
async fn authenticate_agent(handle: &mut client::Handle<Handler>, user: &str) -> bool {
    let Ok(mut agent) =
        russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await
    else {
        return false;
    };
    let Ok(identities) = agent.request_identities().await else {
        return false;
    };
    for identity in identities {
        let key = identity.public_key().into_owned();
        // `best_supported_rsa_hash` returns `Result<Option<Option<HashAlg>>>`:
        // the outer option is "the server told us", the inner is "and it is a
        // hash rather than none". Both have to be flattened away, and this
        // path only compiles on Windows — so it is not the compiler that
        // catches getting it wrong here.
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .is_ok_and(|r| r.success())
        {
            return true;
        }
    }
    false
}

fn start_actor(handle: client::Handle<Handler>) -> Session {
    let (commands, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let connected = Arc::new(AtomicBool::new(true));
    let actor_connected = connected.clone();
    crate::detach(async move {
        let handle = handle;
        while let Some(command) = receiver.recv().await {
            match command {
                Command::OpenSftp(done) => {
                    let opened = async {
                        let channel = handle.channel_open_session().await.map_err(|error| {
                            RemoteError::Subsystem {
                                host: "the remote server".into(),
                                message: error.to_string(),
                            }
                        })?;
                        channel
                            .request_subsystem(true, "sftp")
                            .await
                            .map_err(|error| RemoteError::Subsystem {
                                host: "the remote server".into(),
                                message: error.to_string(),
                            })?;
                        crate::sftp::SftpVfs::negotiate(channel.into_stream()).await
                    }
                    .await;
                    let _ = done.send(opened);
                }
                Command::Disconnect(done) => {
                    let _ = handle
                        .disconnect(
                            russh::Disconnect::ByApplication,
                            "Ferret closed the connection",
                            "en",
                        )
                        .await;
                    actor_connected.store(false, Ordering::Release);
                    let _ = done.send(());
                    break;
                }
            }
        }
        actor_connected.store(false, Ordering::Release);
    });
    Session {
        commands,
        connected,
    }
}

fn connect_error(host: &str, port: u16, error: russh::Error) -> RemoteError {
    RemoteError::Connect {
        host: host.into(),
        port,
        message: error.to_string(),
    }
}
