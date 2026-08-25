use crate::VfsPath;
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
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: VfsPath,
    /// The display name. Held separately because deriving it from `path` on
    /// every frame shows up when 200,000 rows are in play.
    pub name: String,
    pub kind: EntryKind,
    /// `Some` when the backend filled it during listing, `None` when it needs a
    /// follow-up [`crate::Vfs::metadata`] call. Never block a frame on `None`;
    /// render the row and let the value arrive.
    pub metadata: Option<Metadata>,
}

impl Entry {
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
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind,
            metadata: Some(Metadata {
                len,
                ..Default::default()
            }),
        }
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
        let pending = Entry {
            path: VfsPath::local("/x/.ssh"),
            name: ".ssh".into(),
            kind: EntryKind::Dir,
            metadata: None,
        };
        assert!(pending.is_hidden());
    }
}
