//! Real Finder and Explorer icons, behind one trait (D-18).
//!
//! A Finder replacement whose icons do not match Finder looks wrong on sight,
//! so the bundled set in `ferret-ui` is the floor rather than the answer. The
//! two rules from `docs/ui-base.html` §6 are what make this safe at 200,000
//! rows, and both are structural rather than advisory:
//!
//! 1. **The OS is never called on the render path.** [`IconProvider::lookup`]
//!    is synchronous and answers only from cache; a miss returns `None` and
//!    the caller draws the bundled icon. Resolution happens in
//!    [`IconProvider::resolve`], off the render path.
//! 2. **Keyed by extension, not by path.** One `.rs` icon serves every Rust
//!    file in a 200,000-entry directory, so the cache stays small and the
//!    number of OS calls is bounded by the number of *types* on screen.

use smol_str::SmolStr;
use std::path::Path;

/// A decoded icon, in the layout GPUI's `RenderImage` wants: BGRA8, premul-
/// ignorant, row-major, no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconBitmap {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, B, G, R, A.
    pub bgra: Vec<u8>,
}

impl IconBitmap {
    /// Whether the buffer actually matches the dimensions it claims.
    ///
    /// Checked at the boundary rather than trusted: the bytes come from an
    /// OS call through `unsafe`, and a wrong stride would be read as pixels.
    pub fn is_well_formed(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.bgra.len() == (self.width as usize) * (self.height as usize) * 4
    }
}

/// What a file type's icon is keyed by.
///
/// Extension for ordinary files, and a small set of special cases for the
/// things whose icon is not a function of their extension — a folder, an
/// application bundle, a symlink.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IconKey {
    /// Lowercased extension, without the dot.
    ///
    /// `SmolStr` rather than `String` (A-33): this is built once per visible
    /// row per frame to look something up in a cache, and an extension is
    /// nearly always short enough to live inline — so the common case costs
    /// no allocation at all.
    Extension(SmolStr),
    /// Any directory that is not an application bundle.
    Directory,
    /// A bundle or executable, whose icon is its own rather than its type's.
    ///
    /// Keyed by full path because two applications do not share an icon. This
    /// one does allocate, and has to: a path is long, and there is no cached
    /// copy of it in the shape a key wants. Bundles are rare enough on any one
    /// screen that it does not matter.
    Application(SmolStr),
    /// A file with no extension.
    Plain,
}

impl IconKey {
    /// The key a path resolves to.
    ///
    /// Prefer [`IconKey::of_entry`] wherever a listing entry is in hand: this
    /// re-derives the lowercased extension that the entry already carries.
    pub fn of(path: &Path, is_dir: bool) -> Self {
        let extension = path.extension().and_then(|e| e.to_str());
        Self::build(path, is_dir, extension, Case::Unknown)
    }

    /// The key for a listed entry, using the extension it resolved during the
    /// walk (D-35) rather than parsing its path again (A-33).
    ///
    /// `extension` is already lowercased, so the common case is a `SmolStr`
    /// clone of an inline string: no parse, no allocation, no syscall.
    pub fn of_entry(path: &Path, is_dir: bool, extension: Option<&str>) -> Self {
        Self::build(path, is_dir, extension, Case::AlreadyLower)
    }

    fn build(path: &Path, is_dir: bool, extension: Option<&str>, case: Case) -> Self {
        if is_dir {
            // A `.app` is a directory but has its own icon, so it cannot
            // share the folder key.
            let is_bundle = extension.is_some_and(|e| e.eq_ignore_ascii_case("app"));
            return if is_bundle {
                IconKey::Application(SmolStr::new(path.to_string_lossy()))
            } else {
                IconKey::Directory
            };
        }
        match extension {
            Some(ext) if !ext.is_empty() => IconKey::Extension(case.fold(ext)),
            _ => IconKey::Plain,
        }
    }
}

/// Whether an extension still needs lowercasing.
///
/// `Entry` resolves its extension lowercased during the walk (D-35), so the
/// hot path can skip the fold entirely; a bare path has made no such promise.
#[derive(Clone, Copy)]
enum Case {
    AlreadyLower,
    Unknown,
}

impl Case {
    fn fold(self, ext: &str) -> SmolStr {
        match self {
            Case::AlreadyLower => SmolStr::new(ext),
            Case::Unknown => SmolStr::from(ext.to_ascii_lowercase()),
        }
    }
}

/// The platform's icon source.
///
/// Separate from [`crate::Platform`] because it has a different shape: every
/// other platform call is `async` and answers once, while this one has to
/// answer synchronously on the render path and be filled in behind.
pub trait IconProvider: Send + Sync + 'static {
    /// The icon for `key`, if it is already known.
    ///
    /// Must not touch the filesystem or the OS: this runs while a frame is
    /// being built, once per visible row.
    fn lookup(&self, key: &IconKey) -> Option<IconBitmap>;

    /// Resolve and cache `key`'s icon, using `sample` as a path of that type.
    ///
    /// Called off the render path. Returns whether anything new was cached,
    /// so the caller knows whether a repaint is worth requesting.
    fn resolve(&self, key: &IconKey, sample: &Path) -> bool;

    /// Whether this provider can produce anything at all.
    ///
    /// `false` on a platform with no implementation, which lets the UI skip
    /// the resolve machinery entirely rather than queue work that will never
    /// produce an icon.
    fn is_available(&self) -> bool {
        true
    }
}

/// A provider that never resolves anything.
///
/// The honest answer where no backend exists: the bundled icons are used and
/// nothing is queued, rather than a stub that pretends to be loading forever.
/// Constructed only on platforms without a backend, so it is dead code on
/// both supported targets — which is the point: neither falls back to it
/// silently.
#[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoIcons;

impl IconProvider for NoIcons {
    fn lookup(&self, _key: &IconKey) -> Option<IconBitmap> {
        None
    }

    fn resolve(&self, _key: &IconKey, _sample: &Path) -> bool {
        false
    }

    fn is_available(&self) -> bool {
        false
    }
}

/// The icon provider for this build.
pub fn icon_provider() -> std::sync::Arc<dyn IconProvider> {
    #[cfg(target_os = "macos")]
    {
        std::sync::Arc::new(crate::macos::MacIcons::default())
    }
    #[cfg(target_os = "windows")]
    {
        std::sync::Arc::new(crate::windows::WindowsIcons::default())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::sync::Arc::new(NoIcons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn files_are_keyed_by_lowercased_extension() {
        // One key per *type*, which is what keeps a 200,000-entry directory
        // to a handful of OS calls.
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/main.rs"), false),
            IconKey::Extension("rs".into())
        );
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/PHOTO.JPG"), false),
            IconKey::Extension("jpg".into())
        );
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/b/other.rs"), false),
            IconKey::of(&PathBuf::from("/z/main.rs"), false),
            "two files of one type share a key"
        );
    }

    #[test]
    fn extensionless_files_share_one_key() {
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/Makefile"), false),
            IconKey::Plain
        );
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/LICENSE"), false),
            IconKey::Plain
        );
    }

    #[test]
    fn app_bundles_are_keyed_individually_but_folders_are_not() {
        // Every folder shares one icon; every application has its own.
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/src"), true),
            IconKey::Directory
        );
        assert_eq!(
            IconKey::of(&PathBuf::from("/Applications/Safari.app"), true),
            IconKey::Application("/Applications/Safari.app".into())
        );
        assert_ne!(
            IconKey::of(&PathBuf::from("/Applications/Safari.app"), true),
            IconKey::of(&PathBuf::from("/Applications/Mail.app"), true)
        );
    }

    #[test]
    fn a_dotfile_has_no_extension_to_key_on() {
        // `.gitignore` is a name, not a `gitignore` type.
        assert_eq!(
            IconKey::of(&PathBuf::from("/a/.gitignore"), false),
            IconKey::Plain
        );
    }

    #[test]
    fn a_malformed_bitmap_is_rejected() {
        let good = IconBitmap {
            width: 2,
            height: 2,
            bgra: vec![0; 16],
        };
        assert!(good.is_well_formed());

        // Short buffer: reading this as pixels would run off the end.
        let short = IconBitmap {
            width: 2,
            height: 2,
            bgra: vec![0; 12],
        };
        assert!(!short.is_well_formed());

        let empty = IconBitmap {
            width: 0,
            height: 0,
            bgra: Vec::new(),
        };
        assert!(!empty.is_well_formed());
    }

    #[test]
    fn the_null_provider_reports_itself_unavailable() {
        let provider = NoIcons;
        assert!(!provider.is_available());
        assert_eq!(provider.lookup(&IconKey::Directory), None);
        assert!(!provider.resolve(&IconKey::Directory, &PathBuf::from("/a")));
    }
}
