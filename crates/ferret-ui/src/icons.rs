//! The bundled fallback icon provider (D-18).
//!
//! The mapping table ships as `assets/icons/file-icons.json` and is parsed
//! once at startup. Lookup is synchronous and O(1): exact filename, then
//! lowercase extension, then a kind default. This is the synchronous answer
//! the file list uses so a row never blocks or flickers; the OS providers
//! layer over the same trait later and win when they resolve.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;

use ferret_vfs::Entry;

/// One unique OS-icon type and a local path that represents it.
///
/// The sample path is all the platform provider needs to resolve an icon, so
/// folder loading moves this bounded payload instead of cloning every entry.
pub(crate) type OsIconRequest = (ferret_platform::IconKey, std::path::PathBuf);

#[derive(Debug, Default, serde::Deserialize)]
struct Mapping {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    by_name: HashMap<String, String>,
    /// The shipped table spells this key `by_extension`.
    #[serde(default, rename = "by_extension")]
    by_ext: HashMap<String, String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct Defaults {
    #[serde(default = "default_file")]
    file: String,
    #[serde(default = "default_dir")]
    directory: String,
    #[serde(default = "default_symlink")]
    symlink: String,
    #[serde(default = "default_other")]
    other: String,
}

fn default_file() -> String {
    "document".into()
}
fn default_dir() -> String {
    "folder-base".into()
}
fn default_symlink() -> String {
    "url".into()
}
fn default_other() -> String {
    "binary".into()
}

/// Resolves entries to bundled SVG asset paths.
///
/// Values are complete asset paths, formatted once when the table is parsed.
/// The mapping file stores bare icon names, but the file list asks this
/// question once per row per frame, and `format!("icons/file/{name}.svg")` on
/// that path is a heap allocation per row per frame for a string that is the
/// same every time.
#[derive(Debug, Default)]
pub struct FileIcons {
    by_name: HashMap<String, gpui::SharedString>,
    by_ext: HashMap<String, gpui::SharedString>,
    defaults: ResolvedDefaults,
}

/// The kind fallbacks, as asset paths.
#[derive(Debug)]
struct ResolvedDefaults {
    file: gpui::SharedString,
    directory: gpui::SharedString,
    symlink: gpui::SharedString,
    other: gpui::SharedString,
}

impl Default for ResolvedDefaults {
    fn default() -> Self {
        Self::from(Defaults::default())
    }
}

impl From<Defaults> for ResolvedDefaults {
    fn from(d: Defaults) -> Self {
        Self {
            file: asset_path_for(&d.file),
            directory: asset_path_for(&d.directory),
            symlink: asset_path_for(&d.symlink),
            other: asset_path_for(&d.other),
        }
    }
}

/// `icons/file/<name>.svg` — the one place the layout of an icon asset path
/// is written down.
fn asset_path_for(name: &str) -> gpui::SharedString {
    gpui::SharedString::new(format!("icons/file/{name}.svg"))
}

static ICONS: OnceLock<FileIcons> = OnceLock::new();

/// Stable, decoded image handles for platform icons.
///
/// The platform cache owns raw BGRA bytes because it cannot depend on GPUI.
/// This cache is the UI-side boundary: lock-free reads return the exact same
/// `RenderImage` identity every frame, while a new type uses copy-on-write
/// insertion once.
pub struct RenderedIconCache {
    images: ArcSwap<HashMap<ferret_platform::IconKey, Arc<gpui::RenderImage>>>,
}

impl Default for RenderedIconCache {
    fn default() -> Self {
        Self {
            images: ArcSwap::from_pointee(HashMap::new()),
        }
    }
}

impl RenderedIconCache {
    fn get(&self, key: &ferret_platform::IconKey) -> Option<Arc<gpui::RenderImage>> {
        self.images.load().get(key).cloned()
    }

    fn insert_if_absent(
        &self,
        key: ferret_platform::IconKey,
        image: Arc<gpui::RenderImage>,
    ) -> Arc<gpui::RenderImage> {
        loop {
            let current = self.images.load_full();
            if let Some(existing) = current.get(&key) {
                return existing.clone();
            }
            let mut next = (*current).clone();
            next.insert(key.clone(), image.clone());
            let previous = self.images.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&previous, &current) {
                return image;
            }
        }
    }
}

impl FileIcons {
    /// Parse the embedded table. A damaged table costs icons, not data:
    /// every miss falls back to the kind defaults below.
    pub fn embedded() -> &'static Self {
        ICONS.get_or_init(|| {
            let parsed: Mapping =
                serde_json::from_str(&crate::assets::file_icons_json()).unwrap_or_default();
            let resolve = |table: HashMap<String, String>| {
                table
                    .into_iter()
                    .map(|(key, icon)| (key, asset_path_for(&icon)))
                    .collect()
            };
            FileIcons {
                by_name: resolve(parsed.by_name),
                by_ext: resolve(parsed.by_ext),
                defaults: parsed.defaults.into(),
            }
        })
    }

    /// The asset path for an entry's icon.
    ///
    /// The returned string is already a GPUI asset path —
    /// `Icon::default().path(result)` — and cloning it is a refcount bump, so
    /// this is safe to call once per row per frame, which is what the file
    /// list does.
    pub fn asset_path(&self, entry: &Entry) -> gpui::SharedString {
        let icon = match entry.kind {
            ferret_vfs::EntryKind::Dir => &self.defaults.directory,
            ferret_vfs::EntryKind::Symlink { .. } => &self.defaults.symlink,
            ferret_vfs::EntryKind::Other => &self.defaults.other,
            ferret_vfs::EntryKind::File => {
                // By name first, so `Cargo.toml` beats the `toml` extension.
                // A dotfile tries its stripped name ahead of its literal one,
                // which is how `.gitignore` reaches the `gitignore` mapping.
                // Only then the extension the entry resolved at listing time,
                // rather than one re-derived from the path per row (A-12).
                let by_name = entry
                    .name()
                    .strip_prefix('.')
                    .and_then(|stem| self.by_name.get(stem))
                    .or_else(|| self.by_name.get(entry.name()));
                match by_name.or_else(|| entry.extension().and_then(|ext| self.by_ext.get(ext))) {
                    Some(icon) => icon,
                    None => &self.defaults.file,
                }
            }
        };
        icon.clone()
    }
}

/// The OS icon for an entry, if one is already cached (D-18).
///
/// Synchronous and cache-only, exactly as §6 requires: a miss returns `None`
/// and the caller draws the bundled SVG, so a row never waits on the OS.
pub fn os_icon(entry: &Entry, cx: &gpui::App) -> Option<gpui::ImageSource> {
    let state = cx.global::<crate::state::FerretState>();
    if entry.kind.is_symlink() {
        return None;
    }
    let local = entry.path.as_local()?;
    // From the entry's cached extension, not by re-parsing its path: this
    // runs once per visible row per frame (A-33).
    let key = ferret_platform::IconKey::of_entry(local, entry.is_dir_like(), entry.extension());
    if let Some(image) = state.rendered_os_icons.get(&key) {
        return Some(gpui::ImageSource::Render(image));
    }
    let bitmap = state.os_icons.lookup(&key)?;

    // Guarded rather than trusted: the bytes crossed an `unsafe` boundary,
    // and a wrong stride here would be painted as pixels.
    if !bitmap.is_well_formed() {
        return None;
    }
    let frame = ferret_preview::image::still_frame(&ferret_preview::Bitmap {
        width: bitmap.width,
        height: bitmap.height,
        has_transparency: ferret_preview::image::detect_transparency(&bitmap.bgra),
        bgra: bitmap.bgra,
    })?;
    let image = Arc::new(gpui::RenderImage::new(vec![frame]));
    Some(gpui::ImageSource::Render(
        state.rendered_os_icons.insert_if_absent(key, image),
    ))
}

/// Derive one platform-icon request for each uncached file type.
///
/// This deliberately consumes borrowed entries while the folder model is
/// available. The returned vector is bounded by distinct icon keys rather
/// than the number of directory entries.
pub(crate) fn collect_os_icon_requests<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    provider: &dyn ferret_platform::IconProvider,
) -> Vec<OsIconRequest> {
    if !provider.is_available() {
        return Vec::new();
    }

    let mut wanted = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if entry.kind.is_symlink() {
            continue;
        }
        let Some(local) = entry.path.as_local() else {
            continue;
        };
        let key = ferret_platform::IconKey::of_entry(local, entry.is_dir_like(), entry.extension());
        if provider.lookup(&key).is_some() || !seen.insert(key.clone()) {
            continue;
        }
        wanted.push((key, local.to_path_buf()));
    }
    wanted
}

/// Resolve a pre-deduplicated set of platform icon requests off the render
/// path. See [`collect_os_icon_requests`] for why this takes requests instead
/// of a whole folder listing.
pub(crate) fn resolve_os_icons(wanted: Vec<OsIconRequest>, cx: &mut gpui::App) {
    if wanted.is_empty() {
        return;
    }
    let provider = cx.global::<crate::state::FerretState>().os_icons.clone();

    cx.background_executor()
        .spawn(async move {
            for (key, sample) in wanted {
                provider.resolve(&key, &sample);
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, VfsPath};

    fn file_entry(name: &str) -> Entry {
        Entry::new(VfsPath::local(format!("/x/{name}")), name, EntryKind::File)
    }

    #[test]
    fn known_extensions_resolve() {
        let icons = FileIcons::embedded();
        assert!(
            icons
                .asset_path(&file_entry("main.rs"))
                .ends_with("rust.svg")
        );
        assert!(
            icons
                .asset_path(&file_entry("notes.md"))
                .ends_with("markdown.svg")
        );
        // Case-insensitive extension.
        assert!(
            icons
                .asset_path(&file_entry("IMG.PNG"))
                .ends_with("image.svg")
        );
    }

    #[test]
    fn known_filenames_win_over_extensions() {
        let icons = FileIcons::embedded();
        // `Cargo.toml` maps by name even though `toml` also exists by ext.
        assert!(
            icons
                .asset_path(&file_entry("Cargo.toml"))
                .ends_with("rust.svg")
        );
        assert!(
            icons
                .asset_path(&file_entry("Dockerfile"))
                .ends_with("docker.svg")
        );
    }

    #[test]
    fn dotfiles_have_no_extension_so_fall_back_to_defaults() {
        let icons = FileIcons::embedded();
        assert!(
            icons
                .asset_path(&file_entry(".gitignore"))
                .ends_with("git.svg")
        );
        assert!(
            icons
                .asset_path(&file_entry(".some-unknown-file"))
                .ends_with("document.svg")
        );
    }

    #[test]
    fn unknown_types_land_on_kind_defaults() {
        let icons = FileIcons::embedded();
        assert!(
            icons
                .asset_path(&file_entry("thing.unknownext"))
                .ends_with("document.svg")
        );

        let dir = Entry::new(VfsPath::local("/x/dir"), "dir", EntryKind::Dir);
        assert!(icons.asset_path(&dir).ends_with("folder-base.svg"));

        let broken = Entry::new(
            VfsPath::local("/x/loop"),
            "loop",
            EntryKind::Symlink { target_kind: None },
        );
        assert!(icons.asset_path(&broken).ends_with("url.svg"));
    }

    #[test]
    fn every_mapped_icon_exists_as_an_asset() {
        let icons = FileIcons::embedded();
        let mut paths: Vec<&gpui::SharedString> = icons.by_name.values().collect();
        paths.extend(icons.by_ext.values());
        paths.extend([
            &icons.defaults.file,
            &icons.defaults.directory,
            &icons.defaults.symlink,
            &icons.defaults.other,
        ]);

        for path in paths {
            assert!(
                crate::assets::FerretAssets::get(path).is_some(),
                "mapped icon {path} is missing from assets"
            );
        }
    }
}
