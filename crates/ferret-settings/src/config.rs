//! Every tunable in this crate.

use std::time::Duration;

/// Directory name under the OS config directory.
///
/// Resolves to `~/Library/Application Support/Ferret` on macOS and
/// `%APPDATA%\Ferret` on Windows, via the `dirs` crate. Never beside the binary:
/// installs are read-only and shared.
pub const APP_DIR: &str = "Ferret";

/// The user-editable settings file.
pub const SETTINGS_FILE: &str = "settings.json";

/// The per-folder view preference store, written by the app rather than by hand.
pub const FOLDER_PREFS_FILE: &str = "folder-views.json";

/// How many folders keep their view preferences before the oldest is evicted.
///
/// Roughly 500 KB on disk and a sub-10 ms load. Windows Explorer caps its
/// equivalent store too — the requirement is "remember the folders I use", not
/// "remember every folder forever", and an unbounded file would grow for the
/// life of the install. See D-16.
pub const FOLDER_PREFS_CAPACITY: usize = 4_000;

/// How long to wait after the last change before writing to disk.
///
/// Every sort click and column drag is a change; without this, dragging a column
/// edge would write the file on every frame.
pub const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);

pub const DEFAULT_UI_FONT_SIZE: f32 = 13.0;
pub const DEFAULT_TERMINAL_FONT_SIZE: f32 = 13.0;

/// Bounds on the interface font size, so a hand-edited file cannot make the app
/// unusable and unfixable through its own UI.
pub const MIN_FONT_SIZE: f32 = 9.0;
pub const MAX_FONT_SIZE: f32 = 24.0;

/// Bounds on the sidebar width, matching the geometry in `docs/ui-base.html` §4.
pub const MIN_SIDEBAR_WIDTH: f32 = 180.0;
pub const MAX_SIDEBAR_WIDTH: f32 = 400.0;

/// Bounds on the docked terminal panel height.
///
/// The minimum still leaves a prompt and scrollback readable; the maximum keeps
/// the file pane present at Ferret's minimum window height, so opening the
/// terminal never turns the file manager into only a terminal.
pub const MIN_TERMINAL_HEIGHT: f32 = 120.0;
pub const MAX_TERMINAL_HEIGHT: f32 = 420.0;

/// Bounds on the operations panel height.
///
/// The minimum shows a job row whole — icon, name, bar, rate, and its two
/// buttons — because a queue that truncates the control you need to stop a
/// transfer has the wrong priorities. The maximum leaves the file pane present
/// at Ferret's minimum window height, so watching a large copy never turns the
/// file manager into only a copy log.
pub const MIN_OPERATIONS_HEIGHT: f32 = 110.0;
pub const MAX_OPERATIONS_HEIGHT: f32 = 400.0;
pub const DEFAULT_OPERATIONS_HEIGHT: f32 = 180.0;

/// Bounds on the preview pane width (`docs/preview-plan.html` §3).
///
/// The default is 42% of the available browser workspace at the 1180 px
/// window; the clamps keep the pane a reading surface at the small end and
/// stop it evicting the file pane at the large one. At small window sizes the
/// preview collapses before the sidebar does, so the minimum here never
/// squeezes the file pane below its own base minimum.
pub const MIN_PREVIEW_WIDTH: f32 = 320.0;
pub const MAX_PREVIEW_WIDTH: f32 = 720.0;
pub const DEFAULT_PREVIEW_WIDTH: f32 = 400.0;
