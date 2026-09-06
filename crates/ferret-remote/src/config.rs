//! Every tunable in this crate.
//!
//! Each value carries the reasoning behind it: a constant with no explanation
//! is one nobody can safely change. Numbers here are stated against an 80 ms
//! round trip, because that is what a server across a country feels like and
//! it is the condition these were chosen for.

use std::time::Duration;

/// Worker threads in this crate's Tokio runtime.
///
/// Fixed at two, and deliberately not sized from the core count. This work is
/// latency-bound rather than CPU-bound — it waits on a socket — and Ferret
/// already runs `ferret_vfs::pool`, smol's blocking pool and GPUI's executor,
/// each sized from the machine. A fourth of those would oversubscribe it for
/// no throughput at all.
pub const RUNTIME_THREADS: usize = 2;

/// How long a TCP connection and SSH handshake may take before it is a
/// failure worth reporting.
///
/// Long enough for a slow link plus a hardware token tap, short enough that a
/// wrong host name does not look like a hang.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long any single SFTP request may take before the session is considered
/// dead.
///
/// A stat is one round trip; thirty seconds means something is badly wrong,
/// not merely slow.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between keepalives on an idle connection.
///
/// The case this exists for is a laptop that slept: without it, a dead link is
/// discovered on the user's next click, several seconds after they expected a
/// listing. Three missed intervals is a dead session.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How long to wait before the one automatic reconnect attempt.
///
/// One attempt, not a retry loop: if a server is down, hammering it helps
/// nobody, and the banner offering Reconnect is a better answer than a spinner
/// that never resolves.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Directory entries in one batch handed to the UI.
///
/// One `SSH_FXP_NAME` response is one batch, so this is really a cap rather
/// than a target: OpenSSH sends around 100 entries per response, which is
/// already the right granularity for a model update.
pub const LISTING_BATCH_CAP: usize = 512;

/// Concurrent in-flight requests on one SFTP channel.
///
/// The whole reason a remote listing is usable: at 80 ms RTT, sixteen
/// pipelined stats cost one round trip instead of sixteen. Beyond this the
/// server's own request queue becomes the bottleneck and the win flattens.
pub const PIPELINE_DEPTH: usize = 16;

/// Bytes per read or write request during a transfer.
///
/// 32 KB is what OpenSSH's own client uses as its default read size; larger
/// packets risk exceeding a server's maximum and being refused outright.
pub const TRANSFER_CHUNK: usize = 32 * 1024;

/// Outstanding chunks per file during a transfer.
///
/// Bandwidth-delay product: 16 × 32 KB keeps a 50 Mbit link at 80 ms RTT busy
/// without buffering half a gigabyte in memory.
pub const TRANSFER_WINDOW: usize = 16;

/// Files transferred at once per mount.
///
/// Two, not more: extra streams do not make one link faster, and they do make
/// the server's logs and the progress display worse.
pub const TRANSFER_CONCURRENCY: usize = 2;

/// Suffix for a partially written upload.
///
/// A transfer writes here and renames into place on success, so a cancelled or
/// failed upload can never leave a truncated file under the real name.
pub const PART_SUFFIX: &str = ".ferret-part";

/// How long a cached attribute stays trusted.
///
/// Short on purpose. The cache exists to stop the path bar and the column view
/// re-stat-ing what the listing just delivered — not to pretend the server is
/// local. Anything Ferret itself changes invalidates immediately regardless.
pub const ATTR_CACHE_TTL: Duration = Duration::from_secs(5);

/// Directory listings kept per mount for back/forward and column ancestors.
pub const LISTING_CACHE_SIZE: usize = 16;

/// Concurrent directory listings per mount.
///
/// Column view opens several directories at once, which over a high-latency
/// link is several concurrent `opendir` sequences. Without this cap the mode
/// is unusable at 150 ms RTT.
pub const MAX_CONCURRENT_LISTINGS: usize = 3;

/// Largest response Ferret will accept for a single read.
///
/// A server that answers a 32 KB read with 400 MB is either broken or hostile;
/// either way the channel closes rather than the allocation succeeding.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The file the connection list lives in, beside `settings.json`.
///
/// Its own file rather than a settings section: it grows with use, it is
/// written by the app rather than hand-edited, and keeping it separate means a
/// malformed record can never cost the user their settings.
pub const CONNECTIONS_FILE: &str = "connections.json";

/// Service name under which secrets are stored in the OS keychain.
///
/// Namespaced by connection id, so removing a record can remove exactly its
/// own secret and nothing else.
pub const KEYCHAIN_SERVICE: &str = "dev.ferret.connections";
