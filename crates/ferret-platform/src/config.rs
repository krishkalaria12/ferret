//! Tunables for the platform integrations.
//!
//! Deliberately small: most of this crate is a thin wrapper over one OS call
//! each. The values here exist because they encode *policy*, not mechanics.

use std::time::Duration;

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

/// Largest file a native thumbnail service is asked about, in bytes.
///
/// Native services take a path and may read the whole file internally, so
/// this is a request limit, not a Rust byte budget — the guarantees do not
/// transfer to OS APIs. Network shares and known cloud placeholders are
/// excluded outright, not merely capped.
pub const NATIVE_THUMBNAIL_INPUT_LIMIT: u64 = 64 * 1024 * 1024;

/// Longest a native thumbnail request may take before it is abandoned.
///
/// Quick Look generation runs out of process, so a malformed file kills
/// Apple's helper and returns an error rather than hanging us — but "returns"
/// is a promise, not a proof, and a hung third-party thumbnail agent must not
/// park a warm task forever. Past this the request is a miss and the tile
/// keeps its glyph.
pub const NATIVE_THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(5);
