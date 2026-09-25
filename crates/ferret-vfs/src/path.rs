use smol_str::SmolStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Identifies a mounted remote backend. Unused until remote mounts land, but
/// present so [`VfsPath`] does not have to change shape when they do.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct MountId(pub u32);

/// A location the [`crate::Vfs`] can address.
///
/// Local paths keep a real [`Path`], so the standard library does the work
/// and platform semantics — drive letters, UNC, UTF-16, case-insensitivity —
/// stay correct without being reimplemented. Remote paths are always
/// `/`-separated UTF-8, which is what SFTP, S3 and WebDAV all speak.
///
/// Keeping the two apart in the type system is the point: it stops POSIX
/// assumptions leaking into remote code, and Windows assumptions leaking out.
///
/// Both variants hold a shared handle rather than an owned buffer (D-35).
/// Paths are long, they are genuinely shared — one lives in the listing, in
/// the selection set, in a drag payload and in whatever background task is
/// stat-ing it — and the render path clones them constantly. As an `Arc`,
/// every one of those clones is a refcount bump instead of a heap allocation
/// and a copy of sixty-odd bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum VfsPath {
    Local(Arc<Path>),
    Remote { mount: MountId, path: Arc<str> },
}

impl VfsPath {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        VfsPath::Local(Arc::from(path.into()))
    }

    pub fn remote(mount: MountId, path: impl AsRef<str>) -> Self {
        VfsPath::Remote {
            mount,
            path: Arc::from(path.as_ref()),
        }
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
            VfsPath::Local(p) => p.parent().map(|p| VfsPath::Local(Arc::from(p))),
            VfsPath::Remote { mount, path } => {
                let trimmed = path.trim_end_matches('/');
                let cut = trimmed.rfind('/')?;
                let parent = if cut == 0 { "/" } else { &trimmed[..cut] };
                Some(VfsPath::remote(*mount, parent))
            }
        }
    }

    /// Whether `self` is `ancestor` or lies beneath it.
    ///
    /// Component-wise, never string-wise: `/foo/barn` starts with the *text*
    /// `/foo/bar` but is not inside it, and a move that believed otherwise
    /// would refuse a legitimate drop — or worse, allow one into itself.
    pub fn starts_with(&self, ancestor: &VfsPath) -> bool {
        match (self, ancestor) {
            (VfsPath::Local(a), VfsPath::Local(b)) => {
                local_path_identity(a).starts_with(local_path_identity(b))
            }
            (
                VfsPath::Remote {
                    mount: m1,
                    path: p1,
                },
                VfsPath::Remote {
                    mount: m2,
                    path: p2,
                },
            ) => {
                if m1 != m2 {
                    return false;
                }
                let a = remote_components(p1);
                let b = remote_components(p2);
                b.is_empty() || a.starts_with(&b)
            }
            _ => false,
        }
    }

    pub fn join(&self, name: &str) -> VfsPath {
        match self {
            VfsPath::Local(p) => VfsPath::Local(Arc::from(p.join(name))),
            VfsPath::Remote { mount, path } => VfsPath::remote(*mount, join_remote(path, name)),
        }
    }

    /// The extension in lowercase, for icon and preview dispatch.
    ///
    /// Prefer [`crate::Entry::extension`] wherever an entry is in hand: this
    /// re-derives the answer, and the file list asks the same question once
    /// per row per frame and millions of times inside a Kind sort (A-12).
    pub fn extension(&self) -> Option<SmolStr> {
        self.file_name().and_then(lowercase_extension)
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
            VfsPath::Local(p) => local_path_identity(p).to_string_lossy().into_owned(),
            VfsPath::Remote { mount, path } => format!("{}:{}", mount.0, path),
        }
    }
}

fn local_path_identity(path: &Path) -> PathBuf {
    #[cfg(any(windows, target_os = "macos"))]
    {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        path.to_path_buf()
    }
}

fn remote_components(path: &str) -> Vec<&str> {
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            component => components.push(component),
        }
    }
    components
}

fn join_remote(parent: &str, name: &str) -> String {
    let parent = parent.trim_end_matches('/');
    let name = name.trim_start_matches('/');
    if parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The lowercased extension of a file name, or `None` when it has none.
///
/// A leading dot makes a hidden file, not an extension: `.gitignore` has no
/// extension, `archive.tar.gz` has `gz`. Already-lowercase names — nearly all
/// of them — come back without touching the heap, because a `SmolStr` this
/// short lives inline.
pub(crate) fn lowercase_extension(name: &str) -> Option<SmolStr> {
    let (_, ext) = name.trim_start_matches('.').rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        return Some(SmolStr::from(ext.to_ascii_lowercase()));
    }
    Some(SmolStr::new(ext))
}

pub fn validate_portable_file_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        return Err("is empty");
    }
    if name == "." || name == ".." {
        return Err("is a directory traversal name");
    }
    if name.contains('/') || name.contains('\\') || name.as_bytes().contains(&0) {
        return Err("contains a path separator");
    }
    if name.chars().any(char::is_control) {
        return Err("contains a control character");
    }
    if name
        .chars()
        .any(|character| matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err("contains a character Windows cannot store");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("ends with a dot or a space");
    }
    let device_stem = name
        .trim_start_matches('.')
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches([' ', '.']);
    if is_windows_device(device_stem) {
        return Err("is a reserved device name on Windows");
    }
    if portable_name_length(name) > 255 {
        return Err("is too long for a file name");
    }
    Ok(())
}

fn is_windows_device(name: &str) -> bool {
    const DEVICES: [&str; 28] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "com¹",
        "com²", "com³", "lpt¹", "lpt²", "lpt³",
    ];
    let name = name.to_lowercase();
    DEVICES.contains(&name.as_str())
}

#[cfg(windows)]
fn portable_name_length(name: &str) -> usize {
    use std::os::windows::ffi::OsStrExt as _;
    std::ffi::OsStr::new(name).encode_wide().count()
}

#[cfg(not(windows))]
fn portable_name_length(name: &str) -> usize {
    name.len()
}

/// How a [`VfsPath`] crosses a process boundary.
///
/// A local path is not text — it is a sequence of platform units that only
/// *usually* decodes as UTF-8. Sending `to_string_lossy` would silently
/// corrupt every path containing an unpaired surrogate or a non-UTF-8 byte,
/// and search is precisely the feature that surfaces those files. So the
/// common case travels as a string and everything else travels as its raw
/// units, widened to `u32` so one wire form works on both platforms.
#[derive(serde::Serialize, serde::Deserialize)]
enum WireVfsPath {
    LocalText(String),
    LocalRaw(Vec<u32>),
    Remote { mount: MountId, path: String },
}

impl serde::Serialize for VfsPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            VfsPath::Local(path) => match path.to_str() {
                Some(text) => WireVfsPath::LocalText(text.to_owned()),
                None => WireVfsPath::LocalRaw(native_units(path)),
            },
            VfsPath::Remote { mount, path } => WireVfsPath::Remote {
                mount: *mount,
                path: path.to_string(),
            },
        };
        wire.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for VfsPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match WireVfsPath::deserialize(deserializer)? {
            WireVfsPath::LocalText(text) => VfsPath::local(text),
            WireVfsPath::LocalRaw(units) => VfsPath::local(from_native_units(&units)),
            WireVfsPath::Remote { mount, path } => VfsPath::remote(mount, path),
        })
    }
}

#[cfg(unix)]
fn native_units(path: &Path) -> Vec<u32> {
    use std::os::unix::ffi::OsStrExt as _;
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|b| *b as u32)
        .collect()
}

#[cfg(unix)]
fn from_native_units(units: &[u32]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt as _;
    let bytes = units.iter().map(|unit| *unit as u8).collect::<Vec<u8>>();
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(windows)]
fn native_units(path: &Path) -> Vec<u32> {
    use std::os::windows::ffi::OsStrExt as _;
    path.as_os_str().encode_wide().map(u32::from).collect()
}

#[cfg(windows)]
fn from_native_units(units: &[u32]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt as _;
    let wide = units.iter().map(|unit| *unit as u16).collect::<Vec<u16>>();
    PathBuf::from(std::ffi::OsString::from_wide(&wide))
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
        VfsPath::Local(Arc::from(value))
    }
}

impl From<&Path> for VfsPath {
    fn from(value: &Path) -> Self {
        VfsPath::Local(Arc::from(value))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn containment_is_component_wise_not_textual() {
        let bar = VfsPath::local("/foo/bar");

        assert!(VfsPath::local("/foo/bar").starts_with(&bar), "itself");
        assert!(VfsPath::local("/foo/bar/baz").starts_with(&bar), "beneath");

        // The case that makes a string comparison wrong: a sibling whose name
        // merely begins with the same text is not inside it.
        assert!(!VfsPath::local("/foo/barn").starts_with(&bar));
        assert!(!VfsPath::local("/foo").starts_with(&bar), "the parent");
        assert!(!VfsPath::local("/other").starts_with(&bar));
    }

    #[test]
    fn remote_containment_needs_the_same_mount() {
        let a = remote("/data");
        let same_mount = remote("/data/inner");
        let other_mount = VfsPath::remote(MountId(2), "/data/inner");

        assert!(same_mount.starts_with(&a));
        // Two servers can both have /data; one is not inside the other.
        assert!(!other_mount.starts_with(&a));
        // And a local path is never inside a remote one.
        assert!(!VfsPath::local("/data/inner").starts_with(&a));
    }

    use super::*;

    fn remote(path: &str) -> VfsPath {
        VfsPath::remote(MountId(1), path)
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

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn preference_key_folds_case_on_case_insensitive_local_paths() {
        assert_eq!(
            VfsPath::local("/Users/K/Documents").preference_key(),
            VfsPath::local("/users/k/documents").preference_key()
        );
    }

    #[test]
    fn remote_preference_keys_keep_server_case_distinct() {
        assert_ne!(
            VfsPath::remote(MountId(1), "/srv/Data").preference_key(),
            VfsPath::remote(MountId(1), "/srv/data").preference_key()
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
        assert_eq!(remote("/").join("a").to_string(), "/a");
    }
}
