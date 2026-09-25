use crate::VfsPath;
use crate::path::lowercase_extension;
use smol_str::SmolStr;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// What a directory entry is, as far as the file list needs to care.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EntryKind {
    File,
    Dir,
    /// A symlink, with its target already resolved far enough to know whether
    /// it still points at anything. The file list draws broken links
    /// differently, so this cannot be deferred.
    Symlink {
        target_kind: Option<SymlinkTarget>,
    },
    /// Sockets, FIFOs, devices. Listed, never opened.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymlinkTarget {
    File,
    Dir,
}

impl EntryKind {
    /// Whether double-clicking navigates into this rather than opening it.
    ///
    /// A **navigation** question, and only that. It is true of a symlink
    /// pointing at a directory, which is right for opening it and wrong for
    /// anything that treats the entry as a container of other entries — see
    /// [`Self::is_directory`].
    pub fn is_dir_like(self) -> bool {
        matches!(
            self,
            EntryKind::Dir
                | EntryKind::Symlink {
                    target_kind: Some(SymlinkTarget::Dir)
                }
        )
    }

    /// Whether this entry *is* a directory, rather than merely leading to one.
    ///
    /// The distinction that [`Self::is_dir_like`] deliberately drops. A symlink
    /// to a directory navigates like a directory but is its own object on disk:
    /// it has a size, it is copied as a link, and git records it as a blob under
    /// its own path rather than as a tree — so asking a per-directory index
    /// about one finds nothing, every time.
    pub fn is_directory(self) -> bool {
        matches!(self, EntryKind::Dir)
    }

    pub fn is_broken_symlink(self) -> bool {
        matches!(self, EntryKind::Symlink { target_kind: None })
    }

    pub fn is_symlink(self) -> bool {
        matches!(self, EntryKind::Symlink { .. })
    }
}

/// The metadata tier that costs a second syscall on some platforms.
///
/// See [`crate::Capabilities::metadata_in_listing`] — where the platform hands
/// this over with the listing, there is no second call to make.
/// Carried on every entry, so its size is multiplied by the size of the
/// directory: 200,000 rows of `flat/` hold 200,000 of these. `created` and
/// `readonly` used to be here and were read by nothing — 3.2 MB of creation
/// times nobody displayed (A-34). Both come back from the same `stat` that is
/// already being made, so re-adding one when a column wants it costs a line
/// and no syscalls; carrying them until then cost 16 bytes a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Metadata {
    pub len: u64,
    pub modified: Option<SystemTime>,
    /// Hidden by name (leading dot) *or* by the platform's own flag — the
    /// `HIDDEN` / `SYSTEM` attributes on Windows, `UF_HIDDEN` on macOS.
    pub hidden: bool,
    #[serde(default)]
    pub name_hidden: bool,
    #[serde(default)]
    pub platform_hidden: bool,
}

/// One row in the file list.
///
/// Every owned buffer here is a shared handle (D-35). An `Entry` is cloned by
/// the render path once per visible cell per frame, and at 200,000 rows the
/// difference between a clone that allocates twice and one that copies
/// forty-odd bytes is the difference between a frame and several.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: VfsPath,
    /// The display name. Held separately because deriving it from `path` on
    /// every frame shows up when 200,000 rows are in play.
    ///
    /// A [`SmolStr`] stores up to 22 bytes inline, so most filenames cost no
    /// heap allocation and no atomic at all, and degrade to shared ownership
    /// only when they are long.
    name: SmolStr,
    /// The lowercased extension, resolved once here rather than re-derived.
    ///
    /// Two hot paths asked for this per call: icon dispatch, once per row per
    /// frame, and the Kind comparator, which runs ~3.4 million times for one
    /// header click on a 200,000-entry directory (A-12). Resolving it during
    /// listing costs one pass and pays for itself twice.
    ext: Option<SmolStr>,
    pub kind: EntryKind,
    /// `Some` when the backend filled it during listing, `None` when it needs a
    /// follow-up [`crate::Vfs::metadata`] call. Never block a frame on `None`;
    /// render the row and let the value arrive.
    pub metadata: Option<Metadata>,
    /// The target a symbolic link records, held as the exact text the link
    /// itself carries.
    ///
    /// `Arc<str>` rather than `Arc<Path>` for one reason: a `Path` is a fat
    /// pointer, so keeping the label alongside it would push `Entry` past the
    /// size budget (A-34). As text, one field answers both questions — the
    /// path for anything that has to resolve it, the label for anything that
    /// only draws it — and a remote server's separators survive a round trip
    /// that a `PathBuf` would have normalised on Windows.
    pub link_target: Option<Arc<str>>,
}

impl Entry {
    /// Build an entry, resolving the cached extension from `name`.
    ///
    /// The constructor exists so `name` and `ext` cannot drift apart: they are
    /// the same fact stored twice, and a struct literal that set one without
    /// the other would give a row the wrong icon and the wrong sort position.
    pub fn new(path: VfsPath, name: impl AsRef<str>, kind: EntryKind) -> Self {
        let name = name.as_ref();
        Self {
            // Not gated on `kind`: the Kind comparator asks this of every
            // entry, so a directory called `my.folder` must answer the same
            // way it did when the question was put to the path.
            ext: lowercase_extension(name),
            name: SmolStr::new(name),
            path,
            kind,
            metadata: None,
            link_target: None,
        }
    }

    pub fn renamed(&self, path: VfsPath, name: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        let name_hidden = path.is_hidden_by_name();
        let metadata = self.metadata.map(|mut metadata| {
            metadata.platform_hidden |= metadata.hidden && !metadata.name_hidden;
            metadata.name_hidden = name_hidden;
            metadata.hidden = metadata.name_hidden || metadata.platform_hidden;
            metadata
        });
        Self {
            path,
            name: SmolStr::new(name),
            ext: lowercase_extension(name),
            kind: self.kind,
            metadata,
            link_target: self.link_target.clone(),
        }
    }

    pub fn with_metadata(mut self, metadata: Option<Metadata>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Attaches the target a symbolic link records.
    ///
    /// Takes the target as the backend read it, so nothing about it is
    /// re-derived: an SFTP server's own text and a local `read_link`'s bytes
    /// are both what the row shows.
    pub fn with_link_target(mut self, link_target: Option<Arc<str>>) -> Self {
        self.link_target = link_target;
        self
    }

    /// [`Entry::with_link_target`] for a backend holding the target as a
    /// `String`, which is what a lossy conversion of a local path produces.
    pub fn with_link_target_text(self, target: String) -> Self {
        self.with_link_target(Some(Arc::from(target)))
    }

    /// The target as a path, for anything that has to resolve against it.
    pub fn link_target_path(&self) -> Option<&Path> {
        self.link_target.as_deref().map(Path::new)
    }

    /// The target as written, for anything that only draws it.
    pub fn link_target_label(&self) -> Option<&str> {
        self.link_target.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The name as a cheaply cloneable handle, for the render path.
    ///
    /// `gpui::SharedString` is a newtype over this same `SmolStr`, so a cell
    /// that needs an owned label converts without touching the heap for any
    /// name short enough to live inline.
    pub fn name_handle(&self) -> &SmolStr {
        &self.name
    }

    /// The lowercased extension, or `None` for anything that has none —
    /// including every directory, symlink and device node.
    pub fn extension(&self) -> Option<&str> {
        self.ext.as_deref()
    }

    pub fn is_dir_like(&self) -> bool {
        self.kind.is_dir_like()
    }

    /// See [`EntryKind::is_directory`] — an actual directory, not a link to one.
    pub fn is_directory(&self) -> bool {
        self.kind.is_directory()
    }

    /// Size for the Size column. Directories deliberately have none rather than
    /// a number that would require walking them.
    pub fn display_len(&self) -> Option<u64> {
        if self.is_dir_like() {
            None
        } else {
            self.metadata.map(|m| m.len)
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.metadata
            .map(|m| m.hidden)
            .unwrap_or_else(|| self.path.is_hidden_by_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind, len: u64) -> Entry {
        Entry::new(VfsPath::local(format!("/x/{name}")), name, kind).with_metadata(Some(Metadata {
            len,
            ..Default::default()
        }))
    }

    #[test]
    fn directories_report_no_size() {
        assert_eq!(entry("d", EntryKind::Dir, 4096).display_len(), None);
        assert_eq!(entry("f", EntryKind::File, 512).display_len(), Some(512));
    }

    #[test]
    fn symlink_to_dir_navigates_like_a_dir() {
        let link = EntryKind::Symlink {
            target_kind: Some(SymlinkTarget::Dir),
        };
        assert!(link.is_dir_like());
        assert_eq!(entry("l", link, 0).display_len(), None);
    }

    #[test]
    fn a_symlink_to_a_dir_navigates_like_one_without_being_one() {
        // The two questions the callers must not confuse. Anything asking
        // "where does a double-click go" wants the first; anything treating the
        // entry as a container — git's per-directory roll-up above all, which
        // records a symlink under its own path whatever it points at — wants
        // the second.
        let link = EntryKind::Symlink {
            target_kind: Some(SymlinkTarget::Dir),
        };
        assert!(link.is_dir_like());
        assert!(!link.is_directory());
        assert!(!entry("l", link, 0).is_directory());

        // A real directory answers both, and a file neither.
        assert!(EntryKind::Dir.is_dir_like() && EntryKind::Dir.is_directory());
        assert!(!EntryKind::File.is_dir_like() && !EntryKind::File.is_directory());
        // Including a symlink to a *file*, which was never dir-like anyway.
        let to_file = EntryKind::Symlink {
            target_kind: Some(SymlinkTarget::File),
        };
        assert!(!to_file.is_dir_like() && !to_file.is_directory());
    }

    #[test]
    fn broken_symlink_is_detectable_and_not_dir_like() {
        let broken = EntryKind::Symlink { target_kind: None };
        assert!(broken.is_broken_symlink());
        assert!(!broken.is_dir_like());
    }

    #[test]
    fn a_link_target_answers_both_a_path_and_a_label_from_one_field() {
        // The label is not derived per row, so nothing allocates while
        // drawing: one field holds the target exactly as the backend read it
        // and both accessors are views over it. The local backend reaches it
        // through a lossy conversion of a `Path`, SFTP through the server's
        // own text, so both constructors have to answer the same.
        let from_path = entry("l", EntryKind::Symlink { target_kind: None }, 0)
            .with_link_target(Some(std::sync::Arc::from("/targets/real.txt")));
        assert_eq!(
            from_path.link_target_path(),
            Some(Path::new("/targets/real.txt"))
        );
        assert_eq!(from_path.link_target_label(), Some("/targets/real.txt"));

        let from_text = entry("l", EntryKind::Symlink { target_kind: None }, 0)
            .with_link_target_text("real.txt".to_string());
        assert_eq!(from_text.link_target_path(), Some(Path::new("real.txt")));
        assert_eq!(from_text.link_target_label(), Some("real.txt"));
    }

    #[test]
    fn hidden_falls_back_to_the_name_when_metadata_is_pending() {
        let pending = Entry::new(VfsPath::local("/x/.ssh"), ".ssh", EntryKind::Dir);
        assert!(pending.is_hidden());
    }

    #[test]
    fn renaming_recomputes_name_hidden_without_losing_platform_hidden() {
        let hidden = Entry::new(VfsPath::local("/x/.old"), ".old", EntryKind::File).with_metadata(
            Some(Metadata {
                hidden: true,
                name_hidden: true,
                ..Default::default()
            }),
        );
        assert!(!hidden.renamed(VfsPath::local("/x/new"), "new").is_hidden());

        let visible = Entry::new(VfsPath::local("/x/old"), "old", EntryKind::File)
            .with_metadata(Some(Metadata::default()));
        assert!(
            visible
                .renamed(VfsPath::local("/x/.new"), ".new")
                .is_hidden()
        );

        let platform_hidden = Entry::new(VfsPath::local("/x/old"), "old", EntryKind::File)
            .with_metadata(Some(Metadata {
                hidden: true,
                ..Default::default()
            }));
        assert!(
            platform_hidden
                .renamed(VfsPath::local("/x/new"), "new")
                .is_hidden()
        );

        let both_hidden = Entry::new(VfsPath::local("/x/.old"), ".old", EntryKind::File)
            .with_metadata(Some(Metadata {
                hidden: true,
                name_hidden: true,
                platform_hidden: true,
                ..Default::default()
            }));
        assert!(
            both_hidden
                .renamed(VfsPath::local("/x/new"), "new")
                .is_hidden()
        );
    }

    #[test]
    fn the_cached_extension_matches_what_the_path_would_have_said() {
        // The constructor is the only writer, so this is the invariant that
        // keeps icon dispatch and the Kind sort agreeing with each other.
        for name in [
            "main.rs",
            "IMAGE.PNG",
            "archive.tar.gz",
            ".gitignore",
            ".env.local",
            "Makefile",
            "trailing.",
        ] {
            let entry = entry(name, EntryKind::File, 0);
            assert_eq!(
                entry.extension(),
                entry.path.extension().as_deref(),
                "{name}"
            );
        }
    }

    #[test]
    fn a_dotted_directory_answers_like_its_path_did() {
        // The Kind comparator asks every entry, not just files, and used to
        // ask the path — so `my.folder` has to keep reporting `folder`.
        assert_eq!(
            entry("my.folder", EntryKind::Dir, 0).extension(),
            Some("folder")
        );
    }
}

#[cfg(test)]
mod size_tests {
    /// `Entry` is carried once per row, so its size is multiplied by the size
    /// of the directory — 200,000 of them for `flat/`. This is a tripwire, not
    /// a target: growing it is sometimes right, and doing so accidentally is
    /// not (A-34).
    #[test]
    fn entry_stays_small() {
        assert!(
            size_of::<super::Entry>() <= 128,
            "Entry is {} bytes; at 200,000 rows every extra 8 is 1.6 MB",
            size_of::<super::Entry>()
        );
        // The niche in `VfsPath`'s `Arc` means an empty slab slot costs
        // nothing over a full one, which is what makes the slab affordable.
        assert_eq!(
            size_of::<Option<super::Entry>>(),
            size_of::<super::Entry>(),
            "Option<Entry> must pack into Entry's niche"
        );
    }
}
