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
    Appearance, Browser, Settings, Sidebar, SortKey, SortOrder, ThemeChoice, ViewMode,
};

use config::{
    APP_DIR, FOLDER_PREFS_FILE, MAX_FONT_SIZE, MAX_SIDEBAR_WIDTH, MIN_FONT_SIZE, MIN_SIDEBAR_WIDTH,
    SETTINGS_FILE,
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
            // Whatever is left is from a version that knows more than this
            // one — or a `window`/`keymap` section from a version that had
            // them declared before D-41 removed them. Either way it is kept
            // verbatim and written back, which is rule 2.
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
        // A newer Ferret wrote `terminal`, and a nested key this version has
        // never heard of. Opening in an older build must not delete either.
        let original = r#"{
            "terminal": { "font_size": 12 },
            "appearance": { "theme": "dark", "future_option": true }
        }"#;
        let (settings, _) = Settings::from_json(original);
        let round_tripped = settings.to_json();

        assert!(round_tripped.contains("terminal"), "{round_tripped}");
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
    fn a_removed_section_is_kept_rather_than_dropped() {
        // `window` and `keymap` were removed by D-41 because nothing read
        // them. A file that still has them must not lose them — that is rule
        // 2, and it is what lets the sections come back when the features
        // that read them are built.
        let (settings, warnings) = Settings::from_json(
            r#"{ "window": { "restore_session": false }, "keymap": { "cmd-t": "tab::New" } }"#,
        );
        assert!(warnings.is_empty(), "an unknown section is not an error");
        let round_tripped = settings.to_json();
        assert!(round_tripped.contains("restore_session"), "{round_tripped}");
        assert!(round_tripped.contains("cmd-t"), "{round_tripped}");
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let (reparsed, warnings) = Settings::from_json(&Settings::default().to_json());
        assert_eq!(reparsed, Settings::default());
        assert!(warnings.is_empty());
    }

    /// Every key the settings file offers must do something (D-41).
    ///
    /// Twelve did not: declared, defaulted, serialised into
    /// `default-settings.json` — which the crate ships and calls
    /// "documentation the user reads and copies from" — and read by nothing.
    /// One of them was `confirm_permanent_delete`, so the file told people
    /// they could turn off a delete confirmation that was in fact
    /// unconditional.
    ///
    /// This cannot be a compile-time check: "is read" is not a property the
    /// type system sees. So it is a grep, which is crude and would have caught
    /// all twelve on the day each was written. It runs over the workspace
    /// source outside this crate, because a key read only by its own
    /// definition is exactly the failure being tested for.
    #[test]
    fn every_setting_is_read_by_something() {
        let shipped = Settings::default().to_json();
        let root: Value = serde_json::from_str(&shipped).expect("defaults must serialise");
        let Value::Object(sections) = root else {
            panic!("the settings file is an object");
        };

        // `crates/` from `crates/ferret-settings/src/lib.rs`.
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ferret-settings has a parent")
            .to_path_buf();
        let mut sources = String::new();
        collect_rust_sources(&crates, &mut sources);
        assert!(
            sources.len() > 10_000,
            "found almost no source to search — the walk is wrong, not the settings"
        );

        let mut unread = Vec::new();
        for (section, value) in &sections {
            let Value::Object(keys) = value else {
                continue; // A scalar at the top level is not a section.
            };
            for key in keys.keys() {
                // `settings.browser.show_hidden` reaches the code as
                // `show_hidden`, and a field is always named to be read.
                if !sources.contains(key.as_str()) {
                    unread.push(format!("{section}.{key}"));
                }
            }
        }

        assert!(
            unread.is_empty(),
            "these settings are shipped in default-settings.json and read by nothing: {unread:?}\n\
             Either wire each one up, or remove it until the feature that reads it exists (D-41)."
        );
    }

    /// Every `.rs` file under `dir`, concatenated, skipping this crate.
    fn collect_rust_sources(dir: &std::path::Path, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skipped: a setting named only by its own declaration is the
                // thing this test exists to catch.
                if path.file_name().is_some_and(|n| n == "ferret-settings") {
                    continue;
                }
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push_str(&text);
            }
        }
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
