//! The application-wide state bag, registered as a GPUI [`Global`].
//!
//! One place for the things every view needs and nothing else does: the
//! parsed settings, the folder-preference store, the storage layer, the
//! platform integrations and the icon table. Views read it through
//! `cx.global::<FerretState>()`; writes go through the small mutation helpers
//! so saving stays consistent.

use std::path::PathBuf;
use std::sync::Arc;

use ferret_platform::Platform;
use ferret_settings::{FolderPrefs, FolderView, Settings};
use ferret_vfs::{LocalFs, VfsPath};

use crate::icons::FileIcons;

/// Everything global, in one struct.
pub struct FerretState {
    pub settings: Settings,
    pub prefs: FolderPrefs,
    pub fs: LocalFs,
    pub platform: Arc<dyn Platform>,
    /// The bundled fallback set: always answers, never blocks (D-18).
    pub icons: &'static FileIcons,
    /// Real Finder/Explorer icons, when the platform can supply them. Layers
    /// over `icons` and wins once resolved.
    pub os_icons: Arc<dyn ferret_platform::IconProvider>,
}

impl gpui::Global for FerretState {}

/// Load settings and preferences from disk. A missing or broken file costs
/// nothing: `Settings::from_json` degrades per section and reports why.
pub fn load_state() -> FerretState {
    let settings = match ferret_settings::settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        Some(text) => {
            let (settings, warnings) = Settings::from_json(&text);
            for warning in &warnings {
                eprintln!(
                    "settings: {} {} {}",
                    warning.key, warning.problem, warning.resolution
                );
            }
            settings
        }
        None => Settings::default(),
    };

    let prefs = ferret_settings::folder_prefs_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| FolderPrefs::from_json(&text))
        .unwrap_or_default();

    FerretState {
        settings,
        prefs,
        fs: LocalFs::new(),
        platform: ferret_platform::platform(),
        icons: FileIcons::embedded(),
        os_icons: ferret_platform::icon_provider(),
    }
}

impl FerretState {
    /// Persist settings; best-effort, failures surface on the next save.
    ///
    /// Writes go to a temp file first so a crash mid-write cannot truncate
    /// the real file.
    pub fn save_settings(&self) {
        let Ok(path) = ferret_settings::settings_path() else {
            return;
        };
        write_atomically(&path, &self.settings.to_json());
    }

    /// Persist folder view preferences (debounced by the caller).
    pub fn save_prefs(&mut self) {
        let Ok(path) = ferret_settings::folder_prefs_path() else {
            return;
        };
        if write_atomically(&path, &self.prefs.to_json()) {
            self.prefs.mark_saved();
        }
    }

    /// The remembered view for a folder, falling back to browser defaults.
    pub fn folder_view(&mut self, dir: &VfsPath) -> FolderView {
        if !self.settings.browser.remember_folder_views {
            return default_folder_view(&self.settings);
        }
        self.prefs
            .get(&dir.preference_key())
            .unwrap_or_else(|| default_folder_view(&self.settings))
    }

    pub fn remember_folder_view(&mut self, dir: &VfsPath, view: FolderView) {
        if !self.settings.browser.remember_folder_views {
            return;
        }
        self.prefs.set(dir.preference_key(), view);
    }

    pub fn home_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Write `contents` to `path` via a temp file in the same directory.
///
/// The config directory is created first: on a fresh machine nothing else
/// makes it, and without it every save fails — silently, because the write
/// goes to a path whose parent does not exist. Returns whether it landed.
fn write_atomically(path: &std::path::Path, contents: &str) -> bool {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("settings: can't create {}: {err}", parent.display());
        return false;
    }

    // Same directory as the target, so the rename stays on one filesystem
    // and is therefore atomic.
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp, contents) {
        eprintln!("settings: can't write {}: {err}", tmp.display());
        return false;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        eprintln!("settings: can't replace {}: {err}", path.display());
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

fn default_folder_view(settings: &Settings) -> FolderView {
    FolderView {
        mode: settings.browser.default_view,
        sort_key: settings.browser.sort_key,
        sort_order: settings.browser.sort_order,
        hidden_columns: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_creates_the_config_directory_it_needs() {
        let root = std::env::temp_dir().join(format!(
            "ferret-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Two levels deep and absent: the fresh-machine case, where a plain
        // write would fail and take the setting with it.
        let path = root.join("Ferret").join("settings.json");
        assert!(!path.exists());

        assert!(write_atomically(&path, "{\"a\":1}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");

        // Rewriting replaces the contents and leaves no temp file behind.
        assert!(write_atomically(&path, "{\"a\":2}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&root).ok();
    }
}
