use crate::VfsPath;
use crate::path::lowercase_extension;
use smol_str::SmolStr;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

/// What a directory entry is, as far as the file list needs to care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkTarget {
    File,
    Dir,
}

impl EntryKind {
    /// Whether double-clicking navigates into this rather than opening it.
    pub fn is_dir_like(self) -> bool {
        matches!(
            self,
            EntryKind::Dir
                | EntryKind::Symlink {
                    target_kind: Some(SymlinkTarget::Dir)
                }
        )
    }

    pub fn is_broken_symlink(self) -> bool {
        matches!(self, EntryKind::Symlink { target_kind: None })
    }
}

/// The metadata tier that costs a second syscall on some platforms.
///
/// See [`crate::Capabilities::metadata_in_listing`] — where the platform hands
/// this over with the listing, there is no second call to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Metadata {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub readonly: bool,
    /// Hidden by name (leading dot) *or* by the platform's own flag — the
    /// `HIDDEN` / `SYSTEM` attributes on Windows, `UF_HIDDEN` on macOS.
    pub hidden: bool,
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
    /// Where a **broken** symlink points, for the file list to show alongside
    /// the name.
    ///
    /// Only filled for links that resolve to nothing: a working link's target
    /// is not worth a `readlink` per row, and a dead one is useless without
    /// it — "broken link" that will not say to *what* is not a diagnosis.
    pub link_target: Option<Arc<Path>>,
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

    pub fn with_metadata(mut self, metadata: Option<Metadata>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_link_target(mut self, link_target: Option<Arc<Path>>) -> Self {
        self.link_target = link_target;
        self
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
    fn broken_symlink_is_detectable_and_not_dir_like() {
        let broken = EntryKind::Symlink { target_kind: None };
        assert!(broken.is_broken_symlink());
        assert!(!broken.is_dir_like());
    }

    #[test]
    fn hidden_falls_back_to_the_name_when_metadata_is_pending() {
        let pending = Entry::new(VfsPath::local("/x/.ssh"), ".ssh", EntryKind::Dir);
        assert!(pending.is_hidden());
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
