//! The bundled fallback icon provider (D-18).
//!
//! The mapping table ships as `assets/icons/file-icons.json` and is parsed
//! once at startup. Lookup is synchronous and O(1): exact filename, then
//! lowercase extension, then a kind default. This is the synchronous answer
//! the file list uses so a row never blocks or flickers; the OS providers
//! layer over the same trait later and win when they resolve.

use std::collections::HashMap;
use std::sync::OnceLock;

use ferret_vfs::Entry;

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
#[derive(Debug, Default)]
pub struct FileIcons {
    by_name: HashMap<String, String>,
    by_ext: HashMap<String, String>,
    defaults: Defaults,
    /// `icons/file/<name>.svg` prefix, materialised per lookup.
    base: &'static str,
}

static ICONS: OnceLock<FileIcons> = OnceLock::new();

impl FileIcons {
    /// Parse the embedded table. A damaged table costs icons, not data:
    /// every miss falls back to the kind defaults below.
    pub fn embedded() -> &'static Self {
        ICONS.get_or_init(|| {
            let parsed: Mapping =
                serde_json::from_str(&crate::assets::file_icons_json()).unwrap_or_default();
            FileIcons {
                by_name: parsed.by_name,
                by_ext: parsed.by_ext,
                defaults: parsed.defaults,
                base: "icons/file",
            }
        })
    }

    /// The asset path for an entry's icon.
    ///
    /// The returned string is already a GPUI asset path —
    /// `Icon::default().path(result)`.
    pub fn asset_path(&self, entry: &Entry) -> String {
        let name = match entry.kind {
            ferret_vfs::EntryKind::Dir => self.defaults.directory.clone(),
            ferret_vfs::EntryKind::Symlink { .. } => self.defaults.symlink.clone(),
            ferret_vfs::EntryKind::Other => self.defaults.other.clone(),
            ferret_vfs::EntryKind::File => {
                if let Some(icon) = entry.name.strip_prefix('.').and_then(|n| {
                    self.by_name
                        .get(n)
                        .or_else(|| self.by_name.get(entry.name.as_str()))
                }) {
                    icon.clone()
                } else if let Some(icon) = self.by_name.get(entry.name.as_str()) {
                    icon.clone()
                } else if let Some(ext) = entry.path.extension() {
                    self.by_ext.get(&ext).cloned().unwrap_or_else(|| {
                        self.by_name
                            .get(entry.name.as_str())
                            .cloned()
                            .unwrap_or_else(|| self.defaults.file.clone())
                    })
                } else {
                    self.defaults.file.clone()
                }
            }
        };
        format!("{}/{}.svg", self.base, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, VfsPath};

    fn file_entry(name: &str) -> Entry {
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind: EntryKind::File,
            metadata: None,
        }
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

        let dir = Entry {
            path: VfsPath::local("/x/dir"),
            name: "dir".into(),
            kind: EntryKind::Dir,
            metadata: None,
        };
        assert!(icons.asset_path(&dir).ends_with("folder-base.svg"));

        let broken = Entry {
            path: VfsPath::local("/x/loop"),
            name: "loop".into(),
            kind: EntryKind::Symlink { target_kind: None },
            metadata: None,
        };
        assert!(icons.asset_path(&broken).ends_with("url.svg"));
    }

    #[test]
    fn every_mapped_icon_exists_as_an_asset() {
        let icons = FileIcons::embedded();
        let mut names: Vec<&String> = icons.by_name.values().collect();
        names.extend(icons.by_ext.values());
        names.extend([
            &icons.defaults.file,
            &icons.defaults.directory,
            &icons.defaults.symlink,
            &icons.defaults.other,
        ]);

        for name in names {
            let path = format!("{}/{}.svg", icons.base, name);
            assert!(
                crate::assets::FerretAssets::get(&path).is_some(),
                "mapped icon {path} is missing from assets"
            );
        }
    }
}
