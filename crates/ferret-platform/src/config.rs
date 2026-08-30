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

// There was a `PROBE_TIMEOUT_MS` here, written for the hazard of a stale NFS
// mount hanging the sidebar during `volumes()`. It was never wired up, and the
// hazard it guarded no longer reaches a frame: the browser probes volumes from
// a background timer and render only reads the last known list (A-25). A
// timeout would now only truncate a slow answer that costs nothing to wait for.
