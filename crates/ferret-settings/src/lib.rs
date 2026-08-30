//! Ferret's settings and per-folder view preferences.
//!
//! The file is the source of truth and the settings UI is a view over it
//! (D-12), so anything the UI can set must be expressible by hand, and a
//! hand-edited file must never be able to brick the app.
//!
//! ```
//! use ferret_settings::Settings;
//!
//! // A partial file is valid; everything absent takes its default.
//! let (settings, warnings) = Settings::from_json(r#"{ "browser": { "show_hidden": true } }"#);
//! assert!(settings.browser.show_hidden);
//! assert_eq!(settings.appearance.ui_font_size, 13.0);
//! assert!(warnings.is_empty());
//! ```

pub mod config;

mod errors;
mod folder_prefs;
mod schema;

pub use errors::{Result, SettingsError, Warning};
pub use folder_prefs::{FolderPrefs, FolderView};
pub use schema::{
    Appearance, Browser, Keymap, RowDensity, Settings, Sidebar, SortKey, SortOrder, Terminal,
    TerminalBackend, ThemeChoice, ViewMode, WindowSettings,
};

use config::{
    APP_DIR, FOLDER_PREFS_FILE, MAX_FONT_SIZE, MAX_SIDEBAR_WIDTH, MAX_TERMINAL_HEIGHT,
    MIN_FONT_SIZE, MIN_SIDEBAR_WIDTH, MIN_TERMINAL_HEIGHT, SETTINGS_FILE,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::PathBuf;

/// Where settings and view preferences live.
pub fn config_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|d| d.join(APP_DIR))
        .ok_or(SettingsError::NoConfigDirectory)
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(SETTINGS_FILE))
}

pub fn folder_prefs_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(FOLDER_PREFS_FILE))
}

impl Settings {
    /// Parse settings, degrading rather than failing.
    ///
    /// Deliberately infallible. Each section is parsed on its own, so one bad
    /// value costs that section and nothing else, and the returned warnings say
    /// exactly what was ignored and what happened instead.
    pub fn from_json(text: &str) -> (Self, Vec<Warning>) {
        let mut warnings = Vec::new();

        let Ok(Value::Object(mut root)) = serde_json::from_str::<Value>(text) else {
            warnings.push(Warning::new(
                "settings.json",
                "isn't valid JSON.",
                "Using defaults. Your file is untouched — fix it and reopen Ferret.",
            ));
            return (Self::default(), warnings);
        };

        // Editors and humans both leave comment keys behind; they are not errors.
        root.retain(|k, _| !k.starts_with('_'));

        let mut settings = Settings {
            appearance: take_section(&mut root, "appearance", &mut warnings),
            browser: take_section(&mut root, "browser", &mut warnings),
            sidebar: take_section(&mut root, "sidebar", &mut warnings),
            terminal: take_section(&mut root, "terminal", &mut warnings),
            window: take_section(&mut root, "window", &mut warnings),
            keymap: take_section(&mut root, "keymap", &mut warnings),
            // Whatever is left is from a version that knows more than this one.
            unknown: root,
        };

        settings.clamp(&mut warnings);
        (settings, warnings)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Pull out-of-range values back into range.
    ///
    /// A hand-edited `ui_font_size` of 400 must not produce a window that cannot
    /// be used to fix `ui_font_size`.
    fn clamp(&mut self, warnings: &mut Vec<Warning>) {
        let size = self.appearance.ui_font_size;
        if !size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
            let clamped = if size.is_finite() {
                size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
            } else {
                Appearance::default().ui_font_size
            };
            warnings.push(Warning::new(
                "appearance.ui_font_size",
                format!("{size} is outside the usable range {MIN_FONT_SIZE}–{MAX_FONT_SIZE}."),
                format!("Using {clamped}."),
            ));
            self.appearance.ui_font_size = clamped;
        }

        let width = self.sidebar.width;
        if !width.is_finite() || !(MIN_SIDEBAR_WIDTH..=MAX_SIDEBAR_WIDTH).contains(&width) {
            let clamped = if width.is_finite() {
                width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH)
            } else {
                Sidebar::default().width
            };
            warnings.push(Warning::new(
                "sidebar.width",
                format!(
                    "{width} is outside the usable range \
                     {MIN_SIDEBAR_WIDTH}–{MAX_SIDEBAR_WIDTH}."
                ),
                format!("Using {clamped}."),
            ));
            self.sidebar.width = clamped;
        }

        let height = self.terminal.height;
        if !height.is_finite() || !(MIN_TERMINAL_HEIGHT..=MAX_TERMINAL_HEIGHT).contains(&height) {
            let clamped = if height.is_finite() {
                height.clamp(MIN_TERMINAL_HEIGHT, MAX_TERMINAL_HEIGHT)
            } else {
                Terminal::default().height
            };
            warnings.push(Warning::new(
                "terminal.height",
                format!(
                    "{height} is outside the usable range \
                     {MIN_TERMINAL_HEIGHT}–{MAX_TERMINAL_HEIGHT}."
                ),
                format!("Using {clamped}."),
            ));
            self.terminal.height = clamped;
        }

        let font_size = self.terminal.font_size;
        if !font_size.is_finite() || !(MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&font_size) {
            let clamped = if font_size.is_finite() {
                font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
            } else {
                Terminal::default().font_size
            };
            warnings.push(Warning::new(
                "terminal.font_size",
                format!("{font_size} is outside the usable range {MIN_FONT_SIZE}–{MAX_FONT_SIZE}."),
                format!("Using {clamped}."),
            ));
            self.terminal.font_size = clamped;
        }

        if !self.terminal.line_height.is_finite()
            || !(1.0..=2.0).contains(&self.terminal.line_height)
        {
            let value = self.terminal.line_height;
            self.terminal.line_height = Terminal::default().line_height;
            warnings.push(Warning::new(
                "terminal.line_height",
                format!("{value} is outside the usable range 1–2."),
                format!("Using {}.", self.terminal.line_height),
            ));
        }

        // The Name column is not optional: a listing with no names is unusable.
        if !self.browser.columns.iter().any(|c| c == "name") {
            warnings.push(Warning::new(
                "browser.columns",
                "doesn't include \"name\".",
                "Adding it back — the file list needs it.",
            ));
            self.browser.columns.insert(0, "name".into());
        }
    }
}

/// Deserialize one section, falling back to its default with a warning.
fn take_section<T: DeserializeOwned + Default>(
    root: &mut serde_json::Map<String, Value>,
    key: &str,
    warnings: &mut Vec<Warning>,
) -> T {
    let Some(value) = root.remove(key) else {
        return T::default();
    };
    match serde_json::from_value(value) {
        Ok(parsed) => parsed,
        Err(err) => {
            warnings.push(Warning::new(
                key,
                format!("couldn't be read: {err}."),
                "Using the defaults for this section; the rest of your settings still apply.",
            ));
            T::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_all_defaults() {
        let (settings, warnings) = Settings::from_json("{}");
        assert_eq!(settings, Settings::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn a_partial_file_keeps_defaults_for_everything_else() {
        let (settings, warnings) = Settings::from_json(r#"{ "appearance": { "theme": "dark" } }"#);
        assert_eq!(settings.appearance.theme, ThemeChoice::Dark);
        assert_eq!(settings.appearance.ui_font_size, 13.0);
        assert!(settings.browser.natural_sort);
        assert!(warnings.is_empty());
    }

    #[test]
    fn unrecognised_keys_survive_a_round_trip() {
        // A newer Ferret wrote a nested key this version has never heard of.
        // Opening in this build must not delete it.
        let original = r#"{
            "terminal": { "font_size": 12 },
            "appearance": { "theme": "dark", "future_option": true }
        }"#;
        let (settings, _) = Settings::from_json(original);
        let round_tripped = settings.to_json();

        assert!(round_tripped.contains("font_size"), "{round_tripped}");
        assert!(round_tripped.contains("future_option"), "{round_tripped}");
    }

    #[test]
    fn a_bad_section_costs_only_that_section() {
        let (settings, warnings) = Settings::from_json(
            r#"{
                "appearance": { "theme": "dark" },
                "browser": { "default_view": "holographic" }
            }"#,
        );
        assert_eq!(
            settings.appearance.theme,
            ThemeChoice::Dark,
            "appearance survives"
        );
        assert_eq!(
            settings.browser.default_view,
            ViewMode::List,
            "browser resets"
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "browser");
    }

    #[test]
    fn invalid_json_yields_defaults_and_says_so() {
        let (settings, warnings) = Settings::from_json("{ not json");
        assert_eq!(settings, Settings::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].resolution.contains("untouched"));
    }

    #[test]
    fn comment_keys_are_not_treated_as_settings() {
        let (settings, warnings) =
            Settings::from_json(r#"{ "_comment": "notes to self", "sidebar": { "width": 300 } }"#);
        assert_eq!(settings.sidebar.width, 300.0);
        assert!(warnings.is_empty());
        assert!(
            settings.unknown.is_empty(),
            "comments must not be preserved as settings"
        );
    }

    #[test]
    fn an_unusable_font_size_is_clamped_with_a_warning() {
        let (settings, warnings) =
            Settings::from_json(r#"{ "appearance": { "ui_font_size": 400 } }"#);
        assert_eq!(settings.appearance.ui_font_size, MAX_FONT_SIZE);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "appearance.ui_font_size");
    }

    #[test]
    fn an_unusable_terminal_height_is_clamped_with_a_warning() {
        let (settings, warnings) = Settings::from_json(r#"{ "terminal": { "height": 9000 } }"#);
        assert_eq!(settings.terminal.height, MAX_TERMINAL_HEIGHT);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].key, "terminal.height");
    }

    #[test]
    fn the_name_column_cannot_be_removed() {
        let (settings, warnings) =
            Settings::from_json(r#"{ "browser": { "columns": ["size", "modified"] } }"#);
        assert_eq!(settings.browser.columns[0], "name");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_keymap_entry_can_unbind_rather_than_rebind() {
        let (settings, warnings) =
            Settings::from_json(r#"{ "keymap": { "cmd-t": "tab::New", "cmd-w": null } }"#);
        assert_eq!(settings.keymap["cmd-t"].as_deref(), Some("tab::New"));
        assert_eq!(settings.keymap["cmd-w"], None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let (reparsed, warnings) = Settings::from_json(&Settings::default().to_json());
        assert_eq!(reparsed, Settings::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn the_shipped_default_file_matches_the_code() {
        // `default-settings.json` is documentation the user reads and copies
        // from, so it drifting from the actual defaults would be worse than not
        // shipping it. Regenerate with:
        //   cargo run -p ferret-settings --example default_settings > \
        //     crates/ferret-settings/default-settings.json
        let shipped = include_str!("../default-settings.json");
        let (parsed, warnings) = Settings::from_json(shipped);
        assert!(
            warnings.is_empty(),
            "shipped defaults must parse cleanly: {warnings:?}"
        );
        assert_eq!(
            parsed,
            Settings::default(),
            "default-settings.json is out of date — regenerate it"
        );
    }
}
