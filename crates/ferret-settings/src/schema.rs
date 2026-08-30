//! The settings file's shape.
//!
//! Three rules govern everything here, and they are the reason the file is worth
//! trusting:
//!
//! 1. **Every key is optional.** A missing key means its default, so a partial
//!    file works and deleting the file entirely is always safe.
//! 2. **Unknown keys are preserved.** A settings file written by a newer Ferret
//!    must survive being opened by an older one. Anything not recognised is kept
//!    verbatim and written back on save.
//! 3. **A bad value never stops the app.** Parsing is per section, so a mistake
//!    in `browser` cannot cost the user their `appearance`. The bad section falls
//!    back to defaults and reports a [`crate::Warning`].
//! 4. **Every key here does something.** This is the rule that had been
//!    broken: twelve keys were declared, defaulted, shipped in
//!    `default-settings.json` and read by nothing, which made the file
//!    documentation that lied. Six were wired up; the six needing features
//!    nobody has built were removed until those features exist (D-41).
//!    `every_setting_is_read_by_something` in `lib.rs` keeps this true.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Which theme to draw with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Follow the OS appearance, and change with it while running.
    #[default]
    System,
    Light,
    Dark,
}

/// How the file pane renders a directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    List,
    Columns,
    Icons,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    Name,
    Modified,
    Size,
    Kind,
}

impl SortKey {
    /// Whether ordering by this key needs each entry's [`Metadata`].
    ///
    /// Name and Kind come from the directory read itself. Modified and Size do
    /// not, and a listing that skipped the `stat` would sort every row as
    /// equal — so a consumer that wants to skip it has to check here first.
    ///
    /// [`Metadata`]: https://docs.rs/ferret-vfs
    pub fn needs_metadata(self) -> bool {
        matches!(self, SortKey::Modified | SortKey::Size)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Ascending,
    Descending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeChoice,
    /// Interface font size in logical pixels. Clamped on load; see `config`.
    pub ui_font_size: f32,
    /// Draw the window translucent over a blurred desktop.
    ///
    /// Off by default, and deliberately: blur is a macOS enhancement (D-19),
    /// unreliable on Linux compositors and version-gated on Windows, so the
    /// opaque palette stays the one the design is legible in. Turning this on
    /// swaps in the translucent palette as well — the two go together, since
    /// alpha over an opaque window composites against nothing.
    ///
    /// Known limitation: the pinned GPUI revision does not actually make the
    /// macOS window layer transparent, so today this only darkens the palette.
    /// See `vibrancy` in `crates/ferret/src/main.rs` for what was measured.
    pub vibrancy: bool,
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            ui_font_size: 13.0,
            vibrancy: false,
            unknown: Map::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Browser {
    /// Used for folders with no remembered preference of their own.
    pub default_view: ViewMode,
    pub sort_key: SortKey,
    pub sort_order: SortOrder,
    /// Directories sort above files regardless of the sort key.
    pub directories_first: bool,
    /// `file10` sorts after `file9` rather than before it.
    pub natural_sort: bool,
    pub show_hidden: bool,
    /// Confirm before a permanent delete. Trashing is never confirmed — it is
    /// reversible, and a dialog on every delete trains people to dismiss them.
    pub confirm_permanent_delete: bool,
    /// `Enter` opens the selection. macOS Finder renames instead; Ferret opens on
    /// both platforms and renames on `F2`. Set false for Finder muscle memory.
    pub enter_opens: bool,
    /// Remember view, sort and columns per folder.
    pub remember_folder_views: bool,
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            default_view: ViewMode::List,
            sort_key: SortKey::Name,
            sort_order: SortOrder::Ascending,
            directories_first: true,
            natural_sort: true,
            show_hidden: false,
            confirm_permanent_delete: true,
            enter_opens: true,
            remember_folder_views: true,
            unknown: Map::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sidebar {
    pub visible: bool,
    /// Width in logical pixels. Clamped on load; see `config`.
    pub width: f32,
    /// User-pinned folders, in display order.
    pub favorites: Vec<PathBuf>,
    pub show_places: bool,
    pub show_volumes: bool,
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            visible: true,
            width: 208.0,
            favorites: Vec::new(),
            show_places: true,
            show_volumes: true,
            unknown: Map::new(),
        }
    }
}

/// The whole settings file.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub browser: Browser,
    pub sidebar: Sidebar,
    /// Top-level sections this version does not know about, kept verbatim —
    /// which is what a `window` or `keymap` section in an existing file
    /// becomes. Those were removed rather than left declared and unread
    /// (D-41), and rule 2 is what keeps them in the file until the features
    /// that read them exist.
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub unknown: Map<String, Value>,
}
