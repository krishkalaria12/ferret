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
    pub icons: &'static FileIcons,
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
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, self.settings.to_json()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Persist folder view preferences (debounced by the caller).
    pub fn save_prefs(&mut self) {
        let Ok(path) = ferret_settings::folder_prefs_path() else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, self.prefs.to_json()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
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

fn default_folder_view(settings: &Settings) -> FolderView {
    FolderView {
        mode: settings.browser.default_view,
        sort_key: settings.browser.sort_key,
        sort_order: settings.browser.sort_order,
        hidden_columns: Vec::new(),
    }
}
