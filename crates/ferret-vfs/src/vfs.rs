use crate::watch::Watch;
use crate::{Entry, Metadata, Result, VfsPath};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::time::SystemTime;

/// One payload from a directory listing stream.
///
/// Batches keep a large walk from paying a channel send, task wake, and model
/// update for every individual row. Entries within a batch retain the order in
/// which the backend found them.
pub type DirEntryBatch = Vec<Entry>;

/// The asynchronous, cancel-by-drop stream returned from [`Vfs::read_dir`].
pub type ReadDirStream = BoxStream<'static, Result<DirEntryBatch>>;

/// What a backend can actually do.
///
/// The UI reads this to disable actions up front instead of letting the user
/// commit to something that fails at the last step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub symlinks: bool,
    pub permissions: bool,
    /// `rename` is atomic, so a move cannot leave both or neither.
    pub atomic_rename: bool,
    pub rename_noreplace: bool,
    pub atomic_rename_noreplace: bool,
    /// A complete file may be replaced atomically with conflict refusal (see
    /// [`Vfs::write_atomic`]), so the preview pane can offer in-place editing.
    /// A generic writable or rename capability is insufficient for that
    /// feature — only backends that check identity before replacing qualify.
    pub atomic_write: bool,
    /// Change notification is available, so listings self-refresh.
    pub watch: bool,
    /// Listings already carry [`Metadata`], so no follow-up call is needed.
    ///
    /// True on Windows, where `FindFirstFileEx` returns size, times and
    /// attributes with the name. False on macOS, where `readdir` gives only the
    /// name and kind. Consumers must honour this rather than assuming a fixed
    /// number of passes — assuming two would throw away the Windows win, and
    /// assuming one would stall macOS on a 200,000-entry directory.
    pub metadata_in_listing: bool,
    /// The platform's own hidden flag arrives with the name, rather than
    /// riding the same call as size and dates.
    ///
    /// True on Windows, where `FindFirstFileEx` returns the `HIDDEN` and
    /// `SYSTEM` attributes alongside everything else. False on Unix, where
    /// `UF_HIDDEN` lives in `st_flags` and the only way to read it is the
    /// `stat` that also yields size and dates.
    ///
    /// This is what decides whether [`ListingDetail::NamesOnly`] is free or
    /// changes what the user sees: `/` marks twelve of its entries `UF_HIDDEN`
    /// and nothing else hides them, so a listing that skipped the `stat` there
    /// would show `bin`, `etc`, `usr` and the rest while the pane beside it
    /// hid them.
    pub hidden_in_listing: bool,
}

/// How much of each entry a listing's consumer actually needs.
///
/// Names and kinds come free with the directory read. Everything else does
/// not — on Unix it is a `stat` per entry — and a consumer that draws neither
/// a size nor a date should not pay for one.
///
/// Callers must not treat this as a hint about *cost*: it changes what the
/// entries contain. [`Entry::is_hidden`] falls back to the name alone when
/// there is no [`Metadata`], and a sort by size or date over
/// [`ListingDetail::NamesOnly`] entries is a sort over zeroes. See
/// [`Capabilities::hidden_in_listing`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListingDetail {
    /// Name and kind only, including whether a symlink still resolves.
    NamesOnly,
    /// Everything: size, modification time and the platform's hidden flag.
    #[default]
    Full,
}

impl ListingDetail {
    pub fn wants_metadata(self) -> bool {
        matches!(self, ListingDetail::Full)
    }
}

/// What a file looked like when an editor last saw it intact.
///
/// Length plus high-resolution modification time plus a hash of the bytes the
/// editor holds. All three are checked before an atomic replacement, so even
/// a same-size, same-mtime external edit — which a stat alone cannot see —
/// refuses rather than silently losing someone's change. There remains a
/// check-to-replace race against writers that land between the check and the
/// rename; ordinary filesystem replacement is not compare-and-swap, and the
/// API documents that rather than promising strict concurrency safety.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub len: u64,
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub content_hash: u64,
}

impl FileIdentity {
    /// Snapshot an identity from a stat and the bytes the caller already
    /// holds. `modified` is `None` when the backend could not report a time —
    /// a file with no trustworthy clock can still be edited, but its time
    /// half of the check is the epoch rather than a lie about precision.
    pub fn new(len: u64, modified: Option<SystemTime>, contents: &[u8]) -> Self {
        let (mtime_secs, mtime_nanos) = mtime_parts(modified);
        Self {
            len,
            mtime_secs,
            mtime_nanos,
            content_hash: content_hash(contents),
        }
    }
}

/// Split a filesystem timestamp into the `(secs, nanos)` halves [`FileIdentity`]
/// stores. `None` (or pre-epoch) becomes the epoch `(0, 0)` — the same rule
/// `LocalFs` applies when it re-derives the halves at save time, so the two
/// can never drift apart.
pub(crate) fn mtime_parts(modified: Option<SystemTime>) -> (u64, u32) {
    modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| (elapsed.as_secs(), elapsed.subsec_nanos()))
        .unwrap_or((0, 0))
}

/// The hash half of [`FileIdentity`]: FNV-1a, deterministic across processes.
///
/// `std`'s hasher is randomly seeded per process, which is exactly wrong for
/// an identity that may be compared after any restart of the surrounding
/// task. FNV-1a is collision-grade enough for an edit-conflict check backed
/// by a length and mtime comparison in front of it.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// How thoroughly to remove something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveMode {
    /// The default everywhere in the UI. Recoverable.
    Trash,
    /// Irreversible; only ever behind an explicit confirmation.
    Permanent,
}

/// The one abstraction over storage.
///
/// Deliberately thin: it wraps what the platform already provides rather than
/// reimplementing a filesystem. Its job is to keep two promises that are
/// expensive to add later —
///
/// 1. **Nothing blocks a frame.** Every method is async even for local disk,
///    so a high-latency remote mount is a slow call rather than a frozen
///    window.
/// 2. **Listings stream.** [`Vfs::read_dir`] yields entry batches as they
///    arrive, so a huge directory paints immediately instead of after the walk
///    finishes.
///
/// **Cancellation is by drop.** Dropping the stream from `read_dir` stops the
/// work; there is no separate cancel token to forget to call.
#[async_trait]
pub trait Vfs: Send + Sync + 'static {
    fn capabilities(&self) -> Capabilities;

    /// What the backend under `path` can actually do.
    ///
    /// Backend-wide [`Vfs::capabilities`] is a property of the wrong thing
    /// once mounts exist: a read-only mount shows no Edit button rather than
    /// an Edit button that fails on save. The default answers the backend
    /// capabilities for every path; a mount router overrides this per path.
    fn capabilities_for(&self, path: &VfsPath) -> Capabilities {
        let _ = path;
        self.capabilities()
    }

    /// Stream the contents of a directory.
    ///
    /// Batches arrive in whatever order the platform yields entries — sorting
    /// belongs to the view, which has to re-sort on column clicks anyway.
    /// An `Err` item is a single unreadable child and the stream continues; a
    /// failure to open the directory at all arrives as the first item and ends
    /// it. Successful items are never empty.
    ///
    /// `detail` says what the caller will draw. A backend may always return
    /// more than was asked for — [`ListingDetail::NamesOnly`] is permission to
    /// skip work, not a promise that the fields are absent — but it must never
    /// return less than [`ListingDetail::Full`] requests.
    fn read_dir(&self, dir: &VfsPath, detail: ListingDetail) -> ReadDirStream;

    /// Fill in the second metadata tier for one entry.
    ///
    /// Skip entirely when [`Capabilities::metadata_in_listing`] is set.
    async fn metadata(&self, path: &VfsPath) -> Result<Metadata>;

    /// Fill in metadata for a bounded group of entries.
    ///
    /// Backends with blocking filesystem APIs should override this to perform
    /// the whole group in one blocking-pool task. Results remain per-path so
    /// one inaccessible file does not withhold the other rows' metadata.
    async fn metadata_batch(&self, paths: Vec<VfsPath>) -> Vec<(VfsPath, Result<Metadata>)> {
        let mut resolved = Vec::with_capacity(paths.len());
        for path in paths {
            let result = self.metadata(&path).await;
            resolved.push((path, result));
        }
        resolved
    }

    /// Stat one path into a full [`Entry`]: kind resolved (including symlink
    /// targets) plus metadata in one call.
    ///
    /// This is the incremental-update primitive — the watcher hands back
    /// paths, and this turns each into a row without re-listing anything.
    async fn probe(&self, path: &VfsPath) -> Result<Entry> {
        let _ = path;
        Err(crate::VfsError::Unsupported {
            capability: "probing entries",
        })
    }

    /// Whether anything is at `path` — a file, a directory, or a broken
    /// symlink, which is still something.
    ///
    /// Its own method rather than `metadata(..).is_ok()` so a backend can
    /// answer it more cheaply than a full stat, and so callers do not have to
    /// pretend they wanted the metadata.
    async fn exists(&self, path: &VfsPath) -> bool {
        self.metadata(path).await.is_ok()
    }

    /// Resolve symlinks and relative segments to a stable identity.
    ///
    /// Used for the per-folder preference key, so two routes to one directory
    /// share one record.
    async fn canonicalize(&self, path: &VfsPath) -> Result<VfsPath>;

    /// Read at most `budget` bytes.
    ///
    /// The cap is not optional: previews and type-sniffing must never pull a
    /// 4 GB file into memory because someone arrow-keyed past it.
    async fn read(&self, path: &VfsPath, budget: usize) -> Result<Vec<u8>>;

    /// Replace a file's contents, atomically.
    ///
    /// Writes a temporary file in the *same directory* — a different
    /// filesystem makes the final step a copy, which is exactly what
    /// atomicity rules out — flushes it, then renames over the target. A
    /// reader either sees the old bytes or the new ones, never a truncated
    /// file, and a crash mid-write leaves the original intact.
    ///
    /// `expect` is the identity the caller last saw. A mismatch is refused
    /// with [`crate::VfsError::Conflict`] rather than resolved: the file
    /// changed under the editor, and only the person looking at both versions
    /// can say what should win. Length, mtime *and* content hash are all
    /// compared, so a same-size, same-mtime external edit still conflicts.
    ///
    /// Replacing installs a new inode: hard links to the old file keep the
    /// old bytes, which is standard atomic-save semantics rather than an
    /// in-place rewrite — but worth stating, because "replace" reads like
    /// "all names see new bytes".
    ///
    /// Atomic visibility and crash durability are distinct: on Unix the
    /// temporary file and the parent directory are synced where supported,
    /// and on Windows the replacement goes through `ReplaceFileW` so ACLs
    /// and attributes survive. A durability failure after a committed rename
    /// is reported, never retried blind — retrying a replacement that may
    /// have committed is how a file gets written twice.
    ///
    /// There remains a check-to-replace race against writers that land
    /// between the identity check and the rename. Ordinary filesystem
    /// replacement is not compare-and-swap against arbitrary external
    /// writers; a recoverable pre-save copy is the caller's answer to that,
    /// not a promise this method cannot keep.
    async fn write_atomic(
        &self,
        path: &VfsPath,
        contents: &[u8],
        expect: FileIdentity,
    ) -> Result<FileIdentity> {
        let _ = (path, contents, expect);
        Err(crate::VfsError::Unsupported {
            capability: "atomic replacement",
        })
    }

    async fn create_dir(&self, path: &VfsPath) -> Result<()>;

    /// Create an empty file. Refuses to overwrite — "new file" must never
    /// blank out something the user already had.
    async fn create_file(&self, path: &VfsPath) -> Result<()> {
        let _ = path;
        Err(crate::VfsError::Unsupported {
            capability: "creating files",
        })
    }

    async fn rename(&self, from: &VfsPath, to: &VfsPath) -> Result<()>;

    async fn rename_noreplace(&self, from: &VfsPath, to: &VfsPath) -> Result<()>;

    /// Copy a file or a whole directory tree to a new path.
    ///
    /// The destination must not exist — conflict policy (auto-rename,
    /// overwrite) is the caller's decision, and this signature makes
    /// "silently replaced something" impossible to write by accident.
    async fn copy(&self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let _ = (from, to);
        Err(crate::VfsError::Unsupported {
            capability: "copying",
        })
    }

    /// Watch a directory for changes. See [`crate::watch`].
    ///
    /// Dropping the returned [`Watch`] unregisters it.
    fn watch(&self, dir: &VfsPath) -> Result<Watch> {
        let _ = dir;
        Err(crate::VfsError::Unsupported {
            capability: "change notification",
        })
    }

    /// Discard anything remembered about `path`, its children, and the
    /// directory holding it.
    ///
    /// A backend that caches nothing does nothing, which is why this has a
    /// default: local disk re-reads every time and has no stale answer to
    /// drop. A backend that *does* cache must treat this as unconditional —
    /// it is what makes "Refresh" mean "ask the server" rather than "ask
    /// again and possibly get the same answer".
    ///
    /// Cheap and synchronous on purpose: callers invalidate immediately before
    /// listing, and an `async` version would make that an await on the
    /// navigation path for no benefit.
    fn invalidate(&self, path: &VfsPath) {
        let _ = path;
    }

    async fn remove(&self, path: &VfsPath, mode: RemoveMode) -> Result<()>;
}
