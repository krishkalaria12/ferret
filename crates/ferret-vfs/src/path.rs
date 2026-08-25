use std::fmt;
use std::path::{Path, PathBuf};

/// Identifies a mounted remote backend. Unused until remote mounts land, but
/// present so [`VfsPath`] does not have to change shape when they do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MountId(pub u32);

/// A location the [`crate::Vfs`] can address.
///
/// Local paths keep a real [`PathBuf`], so the standard library does the work
/// and platform semantics — drive letters, UNC, UTF-16, case-insensitivity —
/// stay correct without being reimplemented. Remote paths are always
/// `/`-separated UTF-8, which is what SFTP, S3 and WebDAV all speak.
///
/// Keeping the two apart in the type system is the point: it stops POSIX
/// assumptions leaking into remote code, and Windows assumptions leaking out.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VfsPath {
    Local(PathBuf),
    Remote { mount: MountId, path: String },
}

impl VfsPath {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        VfsPath::Local(path.into())
    }

    pub fn as_local(&self) -> Option<&Path> {
        match self {
            VfsPath::Local(p) => Some(p),
            VfsPath::Remote { .. } => None,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, VfsPath::Local(_))
    }

    /// The last component, for display in the file list.
    ///
    /// Returns `None` only for a filesystem root, which has no name.
    pub fn file_name(&self) -> Option<&str> {
        match self {
            VfsPath::Local(p) => p.file_name().and_then(|s| s.to_str()),
            VfsPath::Remote { path, .. } => {
                let trimmed = path.trim_end_matches('/');
                if trimmed.is_empty() {
                    None
                } else {
                    trimmed.rsplit('/').next().filter(|s| !s.is_empty())
                }
            }
        }
    }

    /// The containing directory, or `None` at a root.
    pub fn parent(&self) -> Option<VfsPath> {
        match self {
            VfsPath::Local(p) => p.parent().map(|p| VfsPath::Local(p.to_path_buf())),
            VfsPath::Remote { mount, path } => {
                let trimmed = path.trim_end_matches('/');
                let cut = trimmed.rfind('/')?;
                let parent = if cut == 0 { "/" } else { &trimmed[..cut] };
                Some(VfsPath::Remote {
                    mount: *mount,
                    path: parent.to_string(),
                })
            }
        }
    }

    pub fn join(&self, name: &str) -> VfsPath {
        match self {
            VfsPath::Local(p) => VfsPath::Local(p.join(name)),
            VfsPath::Remote { mount, path } => VfsPath::Remote {
                mount: *mount,
                path: format!("{}/{}", path.trim_end_matches('/'), name),
            },
        }
    }

    /// The extension in lowercase, for icon and preview dispatch.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name()?;
        // A leading dot makes a hidden file, not an extension: `.gitignore`
        // has no extension, `archive.tar.gz` has `gz`.
        let (_, ext) = name.trim_start_matches('.').rsplit_once('.')?;
        if ext.is_empty() {
            None
        } else {
            Some(ext.to_ascii_lowercase())
        }
    }

    /// Whether the platform considers this hidden purely from its name.
    ///
    /// This is only half the answer on Windows, where the `HIDDEN` and `SYSTEM`
    /// file attributes also count — see [`crate::Metadata::hidden`].
    pub fn is_hidden_by_name(&self) -> bool {
        self.file_name().is_some_and(|n| n.starts_with('.'))
    }

    /// A stable key for the per-folder view preference store.
    ///
    /// Case-folded, because both supported platforms are case-insensitive by
    /// default and `~/Documents` must not get a second record as `~/documents`.
    /// Callers should canonicalize first so symlinked routes to one directory
    /// collapse to one key.
    pub fn preference_key(&self) -> String {
        match self {
            VfsPath::Local(p) => p.to_string_lossy().to_lowercase(),
            VfsPath::Remote { mount, path } => format!("{}:{}", mount.0, path.to_lowercase()),
        }
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VfsPath::Local(p) => write!(f, "{}", p.display()),
            VfsPath::Remote { path, .. } => write!(f, "{path}"),
        }
    }
}

impl From<PathBuf> for VfsPath {
    fn from(value: PathBuf) -> Self {
        VfsPath::Local(value)
    }
}

impl From<&Path> for VfsPath {
    fn from(value: &Path) -> Self {
        VfsPath::Local(value.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(path: &str) -> VfsPath {
        VfsPath::Remote {
            mount: MountId(1),
            path: path.to_string(),
        }
    }

    #[test]
    fn local_names_and_parents() {
        let p = VfsPath::local("/Users/k/docs/notes.md");
        assert_eq!(p.file_name(), Some("notes.md"));
        assert_eq!(p.parent().unwrap().to_string(), "/Users/k/docs");
        assert_eq!(p.extension().as_deref(), Some("md"));
    }

    #[test]
    fn remote_names_and_parents() {
        let p = remote("/srv/data/report.csv");
        assert_eq!(p.file_name(), Some("report.csv"));
        assert_eq!(p.parent().unwrap().to_string(), "/srv/data");
        assert_eq!(p.extension().as_deref(), Some("csv"));
    }

    #[test]
    fn roots_have_no_name_and_no_parent() {
        assert_eq!(remote("/").file_name(), None);
        assert!(remote("/").parent().is_none());
        assert!(VfsPath::local("/").parent().is_none());
    }

    #[test]
    fn trailing_slashes_do_not_create_empty_names() {
        assert_eq!(remote("/srv/data/").file_name(), Some("data"));
        assert_eq!(remote("/srv/data/").parent().unwrap().to_string(), "/srv");
    }

    #[test]
    fn dotfiles_have_no_extension() {
        assert_eq!(VfsPath::local("/x/.gitignore").extension(), None);
        assert_eq!(
            VfsPath::local("/x/.env.local").extension().as_deref(),
            Some("local")
        );
        assert_eq!(
            VfsPath::local("/x/archive.tar.gz").extension().as_deref(),
            Some("gz")
        );
        assert_eq!(VfsPath::local("/x/Makefile").extension(), None);
    }

    #[test]
    fn extension_is_lowercased() {
        assert_eq!(
            VfsPath::local("/x/IMAGE.PNG").extension().as_deref(),
            Some("png")
        );
    }

    #[test]
    fn preference_key_folds_case() {
        assert_eq!(
            VfsPath::local("/Users/K/Documents").preference_key(),
            VfsPath::local("/users/k/documents").preference_key()
        );
    }

    #[test]
    fn hidden_by_name_is_leading_dot_only() {
        assert!(VfsPath::local("/x/.ssh").is_hidden_by_name());
        assert!(!VfsPath::local("/x/ssh").is_hidden_by_name());
    }

    #[test]
    fn join_does_not_double_separators() {
        assert_eq!(remote("/srv/").join("a").to_string(), "/srv/a");
        assert_eq!(remote("/srv").join("a").to_string(), "/srv/a");
    }
}
