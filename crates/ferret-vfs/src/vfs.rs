use crate::watch::Watch;
use crate::{Entry, Metadata, Result, VfsPath};
use async_trait::async_trait;
use futures::stream::BoxStream;

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

    /// Stream the contents of a directory.
    ///
    /// Batches arrive in whatever order the platform yields entries — sorting
    /// belongs to the view, which has to re-sort on column clicks anyway.
    /// An `Err` item is a single unreadable child and the stream continues; a
    /// failure to open the directory at all arrives as the first item and ends
    /// it. Successful items are never empty.
    fn read_dir(&self, dir: &VfsPath) -> ReadDirStream;

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

    async fn remove(&self, path: &VfsPath, mode: RemoveMode) -> Result<()>;
}
