//! Tunables for the platform integrations.
//!
//! Deliberately small: most of this crate is a thin wrapper over one OS call
//! each. The values here exist because they encode *policy*, not mechanics.

/// How many entries `volumes()` will list before stopping.
///
/// A user with more than this many mounted volumes has bigger problems than a
/// truncated sidebar; the bound exists so a pathological mount table (a looped
/// network mount, say) cannot spin forever.
pub const MAX_VOLUMES: usize = 64;

/// Volume names that are not real places the user can browse.
///
/// On macOS `/Volumes` contains shadow entries for system volumes on recent
/// releases; showing them produces two entries for the same disk.
pub const VOLUME_EXCLUSIONS: &[&str] = &["Recovery", "Preboot", "VM", "Update"];

/// How many bytes of a directory listing `places()` may read while probing
/// whether a well-known folder exists.
///
/// The existence probes are one `stat` each in practice; the budget only
/// matters for the volume scan, where a stale NFS mount could otherwise hang
/// the sidebar.
pub const PROBE_TIMEOUT_MS: u64 = 250;
