//! Where the daemon listens, where its stores live, and how a client proves
//! it belongs to the same user.
//!
//! Everything here is per-user and inside a directory only that user can read.
//! That is the actual access control: the nonce is a freshness token that ties
//! a client to *this* launch of the daemon, and its secrecy rests entirely on
//! the permissions of the file it is written to.

use std::io::Write as _;
use std::path::PathBuf;

use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Name, NameType as _, ToFsName, ToNsName,
};

use crate::{Result, SearchError};

/// The private directory holding the socket, the nonce and the lock.
///
/// A runtime directory where the platform has one (it is cleared on logout,
/// which is exactly right for a socket), otherwise the user's cache directory.
/// Never a shared temporary directory: `/tmp` is world-readable and a socket
/// there is a socket anyone on the machine can connect to.
/// Both directories can be pointed elsewhere for development and for the
/// end-to-end test, which must not bind the socket a real daemon owns or write
/// into the index a real Ferret is using.
///
/// Overrides, not settings: a person configuring Ferret has no reason to move
/// these, and a test that could only run by touching the developer's own index
/// would not be run.
const RUNTIME_DIR_OVERRIDE: &str = "FERRET_SEARCH_RUNTIME_DIR";
const INDEX_DIR_OVERRIDE: &str = "FERRET_SEARCH_INDEX_DIR";

pub(crate) fn runtime_dir() -> Result<PathBuf> {
    if let Some(override_dir) = std::env::var_os(RUNTIME_DIR_OVERRIDE) {
        let dir = PathBuf::from(override_dir);
        create_private_dir(&dir)?;
        return Ok(dir);
    }
    let base = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .ok_or_else(|| SearchError::DaemonTransport {
            message: "this account has no cache or runtime directory".into(),
        })?;
    let dir = base.join("ferret").join("search");
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Where the SQLite path index and the Tantivy content index live.
///
/// Deliberately *not* the runtime directory: these survive a logout and are
/// expensive to rebuild, so they belong with application data rather than with
/// things the platform is entitled to delete.
pub fn index_dir() -> Result<PathBuf> {
    if let Some(override_dir) = std::env::var_os(INDEX_DIR_OVERRIDE) {
        let dir = PathBuf::from(override_dir);
        create_private_dir(&dir)?;
        return Ok(dir);
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .ok_or_else(|| SearchError::DaemonTransport {
            message: "this account has no application data directory".into(),
        })?;
    let dir = base.join("ferret").join("search-index");
    create_private_dir(&dir)?;
    Ok(dir)
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("searchd.sock"))
}

pub(crate) fn nonce_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("searchd.nonce"))
}

/// The address both sides pass to `interprocess`.
///
/// Windows named pipes live in one machine-wide namespace, so the name carries
/// the user; the pipe's default ACL, which grants only its creator and
/// administrators, is what keeps another signed-in user out. Unix gets a real
/// filesystem socket inside the 0700 runtime directory.
pub(crate) fn socket_name() -> Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        let user = username();
        // A named pipe has no directory to be moved into, so an overridden
        // runtime directory has to reach the *name* — otherwise the test and
        // the developer's real daemon would fight over one pipe on Windows.
        let scope = std::env::var_os(RUNTIME_DIR_OVERRIDE)
            .map(|dir| format!("-{:016x}", stable_hash(&dir.to_string_lossy())))
            .unwrap_or_default();
        return format!("ferret-searchd-{user}{scope}.sock")
            .to_ns_name::<GenericNamespaced>()
            .map_err(super::transport);
    }
    socket_path()?
        .to_fs_name::<GenericFilePath>()
        .map_err(super::transport)
}

/// Writes a fresh nonce for this daemon launch, replacing any stale one.
pub(crate) fn write_nonce(nonce: &str) -> Result<()> {
    let path = nonce_path()?;
    // Replaced rather than appended: a nonce from a previous launch must stop
    // working the moment this one starts, or a client left over from a crashed
    // daemon would authenticate against the new one.
    let mut file = private_file(&path)?;
    file.write_all(nonce.as_bytes()).map_err(super::transport)?;
    file.flush().map_err(super::transport)
}

pub(crate) fn read_nonce() -> Result<String> {
    let path = nonce_path()?;
    std::fs::read_to_string(&path)
        .map(|nonce| nonce.trim().to_owned())
        .map_err(|error| SearchError::DaemonTransport {
            message: format!("couldn't read {}: {error}", path.display()),
        })
}

/// A 128-bit token from the OS entropy `RandomState` is already seeded with.
///
/// Two independent `RandomState`s give two independent OS-seeded keys, which
/// is exactly the property needed here and does not cost a dependency whose
/// only job would be to call the same syscall.
pub(crate) fn fresh_nonce() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    let high = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let low = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{high:016x}{low:016x}")
}

/// A hash that is the same in two processes.
///
/// `DefaultHasher::new` is seeded identically everywhere, unlike
/// `RandomState` — which is exactly backwards from what [`fresh_nonce`] wants,
/// and exactly right here: the client and the daemon must derive the same pipe
/// name from the same override.
fn stable_hash(value: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect()
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(super::transport)?;
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(super::transport)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    // The user profile tree is already ACL'd to the user on Windows, and this
    // directory inherits that.
    std::fs::create_dir_all(dir).map_err(super::transport)
}

#[cfg(unix)]
fn private_file(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(super::transport)
}

#[cfg(not(unix))]
fn private_file(path: &std::path::Path) -> Result<std::fs::File> {
    std::fs::File::create(path).map_err(super::transport)
}
