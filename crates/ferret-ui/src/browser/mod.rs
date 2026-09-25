//! The browser shell: title bar, sidebar, tab strip, path bar, file pane and
//! status bar — the regions of `docs/ui-base.html` §3–§4, wired to the
//! interaction contract in §8.
//!
//! Ownership follows tabs: each [`BrowserTab`] owns its [`Folder`], its
//! [`FilePane`] and the subscriptions between them. Replacing a tab's
//! entities (navigation) drops the old subscriptions, which detaches the old
//! watcher and lets the old listing task see a dead entity and stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use ferret_platform::Place;
use ferret_settings::{FolderView, ThemeChoice, ViewMode};
use ferret_terminal::{
    CursorShape, TerminalBackend, TerminalColor, TerminalOptions, TerminalRun, TerminalSession,
    TerminalSnapshot, TerminalStyle,
};
use ferret_vfs::{Vfs as _, VfsPath};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, Selectable as _, Sizable as _,
    TitleBar, WindowExt as _,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    notification::Notification,
    status_bar::StatusBar,
    tab::{Tab, TabBar},
    v_flex,
};

use crate::actions;
use crate::folder::{Folder, FolderEvent, ListingProfile, SortRules};
use crate::format::unique_name;
use crate::pane::{FilePane, PaneEvent};
use crate::preview::{PreviewOverlay, PreviewPanel};
use crate::state::FerretState;

use commit_dialog::CommitDialog;
use connection_dialog::ConnectionDialog;
pub(crate) use git::GitHandle;
use git::GitPanel;
// Only the allocations-per-frame budget constructs one directly.
#[cfg(all(test, feature = "dhat-heap"))]
pub(crate) use git::GitSnapshot;

mod batch_rename;
mod commit_dialog;
mod connect;
mod connection_dialog;
mod git;
mod git_bar;
mod mutations;
mod path_bar;
mod power_search;
mod preview_panel;
mod render;
mod send_to;
mod sidebar;
mod tab_strip;
mod tabs;
mod terminal_panel;
mod title_bar;

/// How long the volumes list stays cached before the sidebar re-probes.
const VOLUMES_TTL: Duration = Duration::from_secs(15);

// Shell geometry from `docs/ui-base.html` §4, in logical pixels. These are
// spec values rather than tunables, so they live with the views that draw
// them rather than in `config.rs`.

/// Title bar height. The library's own default is 34.
///
/// Public because the window's traffic lights are positioned at creation time
/// from a fixed offset, so `main` has to centre them against this.
pub const TITLE_BAR_HEIGHT: f32 = 52.;
/// Diameter of a macOS window control, for centring them in the title bar.
pub const TRAFFIC_LIGHT_DIAMETER: f32 = 12.;
/// The hairline between the sidebar and the pane.
///
/// One pixel, so the content column starts where the sidebar ends: a wider
/// strip separates the two regions with a band of background, which reads as
/// a gap rather than as an edge.
const SIDEBAR_DIVIDER_WIDTH: f32 = 1.;
/// How far either side of the hairline still counts as grabbing it.
///
/// The line is the whole separation, so the target has to be drawn outside
/// it — one pixel is the right width to look at and the wrong width to aim
/// at. Matches the slop a macOS split view gives its own dividers.
const SIDEBAR_GRAB_SLOP: f32 = 3.;
/// How long the sidebar takes to slide open or shut.
///
/// Short on purpose: the toggle is a keystroke people repeat, so the slide has
/// to read as one motion and be over before the next one lands. Past roughly
/// 200 ms it stops feeling like the panel moved and starts feeling like the
/// app waited.
const SIDEBAR_ANIMATION: Duration = Duration::from_millis(170);
/// How often the sidebar slide re-renders. 120 Hz: fine enough that the
/// motion has no visible steps on a 120 Hz display, and cheap enough that a
/// 170 ms slide is twenty frames.
const SIDEBAR_ANIMATION_TICK: Duration = Duration::from_millis(8);
/// What `gpui_component::TitleBar` insets its content by to clear the window
/// controls. Mirrored here because the library does not export it, and the
/// title bar's left zone has to be measured from the window edge to line the
/// nav group up with the content column below it.
#[cfg(target_os = "macos")]
const TITLE_BAR_LEFT_INSET: f32 = 80.;
#[cfg(not(target_os = "macos"))]
const TITLE_BAR_LEFT_INSET: f32 = 12.;
/// Toolbar glyph size. §4 gives no figure; at the library's default the glyphs
/// read as undersized against the reference.
const TOOLBAR_ICON_SIZE: f32 = 20.;
/// What to set a toolbar `Button`'s size to in order to get a
/// [`TOOLBAR_ICON_SIZE`] glyph out of it.
///
/// `Button::render` overwrites whatever size the icon was given with one
/// derived from the button's own — `Size::Size(v) => v * 0.75` — so sizing the
/// `Icon` directly is silently discarded and the button has to be driven
/// instead. Undo that factor here.
const TOOLBAR_BUTTON_SIZE: f32 = TOOLBAR_ICON_SIZE / 0.75;
/// Widest a tab may grow before its label truncates.
const TAB_MAX_WIDTH: f32 = 190.;
/// Status bar height.
const STATUS_BAR_HEIGHT: f32 = 24.;
/// Chrome type size: the column header, the status bar.
const CHROME_FONT_SIZE: f32 = 11.5;
/// Sidebar row icon.
const SIDEBAR_ICON_SIZE: f32 = 15.;
/// Sidebar row height. The spec's §4 says 22; the reference UI this is now
/// being matched against gives its rows noticeably more room, and at 22 the
/// list reads as a cramped stack rather than a set of targets.
const SIDEBAR_ROW_HEIGHT: f32 = 30.;
/// Sidebar group heading height.
const SIDEBAR_SECTION_HEIGHT: f32 = 18.;
/// The strip the path field sits in, and the field's own height. Taller than
/// §4's h30 because the field needs padding inside the strip to read as a
/// control rather than a band of text.
const PATH_BAR_HEIGHT: f32 = 42.;
const PATH_FIELD_HEIGHT: f32 = 30.;

/// Toolbar icon asset paths (bundled under `assets/icons/ui/`).
mod toolbar_icon {
    pub const SIDEBAR: &str = "icons/ui/sidebar.svg";
    pub const BACK: &str = "icons/ui/nav-left.svg";
    pub const FORWARD: &str = "icons/ui/nav-right.svg";
    pub const UP: &str = "icons/ui/nav-up.svg";
    pub const VIEW_LIST: &str = "icons/ui/view-list.svg";
    pub const VIEW_COLUMNS: &str = "icons/ui/view-columns.svg";
    pub const VIEW_GRID: &str = "icons/ui/view-grid.svg";
    pub const NEW_FILE: &str = "icons/ui/new-file.svg";
    pub const NEW_FOLDER: &str = "icons/ui/new-folder.svg";
    pub const HIDDEN_OFF: &str = "icons/ui/hidden-off.svg";
    pub const HIDDEN_ON: &str = "icons/ui/hidden-on.svg";
    pub const REFRESH: &str = "icons/ui/refresh.svg";
    pub const MOON: &str = "icons/ui/moon.svg";
    pub const SUN: &str = "icons/ui/sun.svg";
    pub const PENCIL: &str = "icons/ui/pencil.svg";
}

pub struct Browser {
    tabs: Vec<BrowserTab>,
    active: usize,

    sidebar_visible: bool,
    /// Logical pixels, clamped to `settings::MIN/MAX_SIDEBAR_WIDTH`.
    sidebar_width: f32,
    /// How much of the sidebar is on screen: 0 shut, 1 open. Distinct from
    /// `sidebar_visible`, which is the target the slide is heading for.
    sidebar_reveal: f32,
    /// Drives `sidebar_reveal`. Held so a second toggle mid-slide cancels the
    /// first rather than racing it.
    sidebar_animation: Option<gpui::Task<()>>,
    /// Live drag state for the sidebar divider.
    sidebar_drag: Option<SidebarDrag>,
    terminal_visible: bool,
    /// Logical pixels, clamped to `settings::MIN/MAX_TERMINAL_HEIGHT`.
    terminal_height: f32,
    terminal_drag: Option<TerminalDrag>,
    /// The preview pane (F-30): one entity per window, fed by the active
    /// tab's selection. Mounted once; it swaps its `Preview`, never itself.
    preview: Entity<PreviewPanel>,
    preview_visible: bool,
    /// Logical pixels, clamped to `settings::MIN/MAX_PREVIEW_WIDTH`.
    preview_width: f32,
    /// Live drag state for the preview divider.
    preview_drag: Option<preview_panel::PreviewDrag>,
    terminal_sync: TerminalSyncState,
    terminal_focus: gpui::FocusHandle,
    terminal_session: Option<TerminalSession>,
    terminal_error: Option<String>,
    terminal_generation: u64,
    terminal_snapshot: Option<Arc<TerminalSnapshot>>,
    terminal_font_family: SharedString,
    terminal_cell_width: f32,
    terminal_columns: u16,
    terminal_rows: u16,
    /// The newest directory waiting to be injected, held until the shell can
    /// safely take it (§6). Only ever one: someone arrow-keying down a tree
    /// should produce one `cd`, not forty.
    ///
    /// A `VfsPath` rather than a `PathBuf` because an SSH session follows the
    /// mount it is logged in to, and a remote path is not a local one.
    terminal_pending_sync: Option<VfsPath>,
    /// The last directory actually written, so a queue that catches up with
    /// where the shell already is writes nothing at all.
    terminal_synced_to: Option<VfsPath>,
    /// The mount an SSH session is logged in to, if this session is one.
    ///
    /// What decides whether a `cd` may be sent at all: a session on one server
    /// must never be told to follow a path on another, or on local disk.
    terminal_mount: Option<ferret_vfs::MountId>,
    /// Whether the user has typed something the injected line would complete.
    terminal_input_dirty: bool,
    /// When the terminal last saw a keystroke. A keystroke inside the idle
    /// window restarts it — it is the one signal that a human is mid-thought.
    terminal_last_key: Option<Instant>,
    /// Runs only while a session exists; a window whose terminal was never
    /// opened schedules nothing.
    terminal_poll: Option<gpui::Task<()>>,
    /// Index of the tab currently being dragged, if any.
    tab_drag: Option<usize>,
    /// Index of the favourite currently being dragged, if any.
    favorite_drag: Option<usize>,
    /// Left edge of each tab in window space, refreshed every frame.
    ///
    /// `gpui-component`'s `Tab` stops propagation of its own pointer events,
    /// so a per-tab listener never runs; the strip therefore does its own hit
    /// testing, and this is the geometry it needs.
    tab_lefts: Arc<Mutex<Vec<gpui::Pixels>>>,
    clipboard: Option<(Vec<VfsPath>, bool)>,

    /// `Some` while the path bar renders an editable input.
    path_editing: Option<PathEdit>,
    /// Reused across opens so inputs and result state are owned by an entity,
    /// never rebuilt by `Dialog`'s per-frame builder.
    power_search: Option<Entity<power_search::PowerSearch>>,
    batch_rename: Option<Entity<batch_rename::BatchRenameDialog>>,
    /// The inline name filter, always mounted at the right of the path bar.
    filter_input: Entity<InputState>,
    /// Held so the filter input keeps notifying the folder.
    _filter_subscription: Subscription,
    filter_task: Option<gpui::Task<()>>,

    /// Mounted volumes as of the last background probe.
    ///
    /// Render only ever reads this. Probing means a `read_dir("/Volumes")`
    /// plus a `metadata` per entry, and a stale network mount can make that
    /// take seconds — which, on the render path, is seconds of frozen window.
    volumes: Vec<Place>,
    /// The well-known folders, as of the same probe. Also never touched from
    /// render: each one is an `is_dir()` (A-28).
    places: Vec<Place>,
    /// Re-probes both every [`VOLUMES_TTL`]. Held so the loop stops when the
    /// browser goes away.
    place_refresh: Option<gpui::Task<()>>,

    /// Focus for the sidebar, so Tab can reach it and it can show that it has
    /// focus (§8: "Focus must be visible").
    sidebar_focus: gpui::FocusHandle,

    /// Repository discovery, the status snapshot and the in-flight operation
    /// (`docs/git.html`). One per window: only the active tab's pane is
    /// rendered, so one snapshot is all that can be on screen at a time.
    git: GitPanel,
    /// The commit dialog's state, kept across opens so a message survives an
    /// accidental Escape. `None` until the dialog is opened for the first time.
    commit_dialog: Option<Entity<CommitDialog>>,
    /// An edit-guard answer held while the Save it depends on is in flight.
    ///
    /// Save is async: the dialog closes at once, and this remembers where to
    /// go when the backend answers. `None` while no guarded save is pending.
    /// See `preview_panel.rs` (PV-Q02).
    guard_after_save: Option<preview_panel::GuardPending>,
    /// Whether the unsaved-changes dialog is currently open. A second
    /// selection change while it stands replaces the pending action rather
    /// than stacking a second copy of the same dialog.
    guard_open: bool,
    /// The full-window preview overlay (F-75), while open. The dialog layer
    /// owns the presentation; this remembers the entity for focus and the
    /// flag for toggle semantics, so a second `space` closes rather than
    /// stacking a second copy.
    preview_overlay: Option<Entity<PreviewOverlay>>,
    overlay_open: bool,
    /// Reused while opening and editing saved SSH endpoints. Input state must
    /// live in an entity because a dialog builder runs again every frame.
    connection_dialog: Option<Entity<ConnectionDialog>>,
    /// The running transfer, drawn in the status bar. One at a time: the
    /// interim queue exists so browsing continues during a copy, not so
    /// several copies can compete for one link.
    transfer: Option<send_to::TransferProgress>,
    /// Pumps that transfer's events. Held here so it lives as long as the
    /// window and dies with it — a transfer nobody can see should not run on.
    transfer_task: Option<gpui::Task<()>>,
    /// Re-reads git status when the window comes back to the front. Held here
    /// so it lasts as long as the window does; see
    /// [`Browser::observe_git_activation`] for why the feature needs it.
    _git_activation: Subscription,
}

struct BrowserTab {
    folder: Entity<Folder>,
    pane: Entity<FilePane>,
    view_mode: ViewMode,
    back: Vec<VfsPath>,
    forward: Vec<VfsPath>,
    /// Lives here so replacing the tab's entities detaches everything.
    _subscriptions: Vec<Subscription>,
}

struct SidebarDrag {
    start_x: gpui::Pixels,
    start_width: f32,
}

struct TerminalDrag {
    start_y: gpui::Pixels,
    start_height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalSyncState {
    On,
    Off,
}

impl TerminalSyncState {
    fn from_default(enabled: bool) -> Self {
        if enabled { Self::On } else { Self::Off }
    }

    fn is_enabled(self) -> bool {
        matches!(self, Self::On)
    }

    fn toggle(self) -> Self {
        if self.is_enabled() {
            Self::Off
        } else {
            Self::On
        }
    }
}

struct PathEdit {
    input: Entity<InputState>,
    _subscription: Subscription,
    /// Directories that could complete what has been typed so far.
    completions: Vec<PathBuf>,
    /// Which completion Tab would accept.
    highlighted: usize,
    /// Bumps per query, so a slow listing cannot overwrite a newer one.
    generation: u64,
}

impl Browser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter")
                .clean_on_escape()
        });
        let filter_subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().trim().to_string();
                    let filter = (!text.is_empty()).then_some(text);
                    this.filter_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(80))
                            .await;
                        let _ = this.update(cx, |this, cx| this.apply_filter(filter, cx));
                    }));
                }
                InputEvent::PressEnter { .. } => {
                    // Return in the filter field opens the first match.
                    let _ = window;
                    this.active().pane.update(cx, |pane, cx| {
                        if let Some(event) = pane.open_current(cx) {
                            pane.emit_event(event, cx);
                        }
                    });
                }
                _ => {}
            },
        );

        let sidebar_visible = cx.global::<FerretState>().settings.sidebar.visible;
        let (terminal_visible, terminal_height, terminal_sync) = {
            let terminal = &cx.global::<FerretState>().settings.terminal;
            (terminal.visible, terminal.height, terminal.sync_default)
        };
        let (preview_visible, preview_width) = {
            let preview = &cx.global::<FerretState>().settings.preview;
            (preview.visible, preview.width)
        };
        let browser_handle = cx.weak_entity();
        let preview = cx.new(|cx| PreviewPanel::new(window, cx, browser_handle.clone()));
        let (terminal_font_family, terminal_cell_width) =
            terminal_panel::terminal_metrics(cx, window);
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            sidebar_visible,
            sidebar_width: cx.global::<FerretState>().settings.sidebar.width,
            // The first frame shows the remembered state outright: a session
            // that opens by sliding its own sidebar in looks like it is still
            // loading.
            sidebar_reveal: if sidebar_visible { 1. } else { 0. },
            sidebar_animation: None,
            sidebar_drag: None,
            terminal_visible,
            terminal_height,
            terminal_drag: None,
            preview,
            preview_visible,
            preview_width,
            preview_drag: None,
            terminal_sync: TerminalSyncState::from_default(terminal_sync),
            terminal_focus: cx.focus_handle(),
            terminal_session: None,
            terminal_error: None,
            terminal_generation: 0,
            terminal_snapshot: None,
            terminal_font_family,
            terminal_cell_width,
            terminal_columns: 120,
            terminal_rows: 24,
            terminal_pending_sync: None,
            terminal_synced_to: None,
            terminal_mount: None,
            terminal_input_dirty: false,
            terminal_last_key: None,
            terminal_poll: None,
            tab_drag: None,
            favorite_drag: None,
            tab_lefts: Arc::new(Mutex::new(Vec::new())),
            clipboard: None,
            path_editing: None,
            power_search: None,
            batch_rename: None,
            filter_input,
            _filter_subscription: filter_subscription,
            filter_task: None,
            volumes: Vec::new(),
            places: Vec::new(),
            place_refresh: None,
            sidebar_focus: cx.focus_handle(),
            git: GitPanel::new(),
            commit_dialog: None,
            guard_after_save: None,
            guard_open: false,
            preview_overlay: None,
            overlay_open: false,
            connection_dialog: None,
            transfer: None,
            transfer_task: None,
            _git_activation: Self::observe_git_activation(window, cx),
        };
        this.push_tab(start_directory(), window, cx);
        if this.terminal_visible {
            // Starts the poll as a side effect; a window that opens with the
            // panel closed never arms one.
            this.restart_terminal(cx);
        }
        this.start_place_refresh(cx);
        this
    }

    // ------------------------------------------------------------------
    // Cross-region plumbing

    /// Release ends whichever drag is in flight; a pointer-up carries no clue
    /// as to which one it was, so all four are settled together.
    fn end_drags(&mut self, cx: &mut Context<Self>) {
        self.end_sidebar_drag(cx);
        self.end_terminal_drag(cx);
        self.end_preview_drag(cx);
        self.end_tab_drag(cx);
        self.end_favorite_drag(cx);
    }

    /// Tab cycles sidebar → path bar → file pane → terminal (§8 plus F-10).
    ///
    /// The path bar only holds focus while it is being edited, so Tab opens
    /// the editor when it lands there rather than skipping a region that
    /// exists but has nothing focusable in it.
    fn focus_region(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable as _;

        let pane_focus = self.active().pane.read(cx).focus_handle(cx);
        let editing_path = self.path_editing.is_some();
        let on_sidebar = self.sidebar_focus.is_focused(window);
        let on_terminal = self.terminal_focus.is_focused(window);

        // Where focus is now decides where it goes; the sidebar drops out of
        // the cycle entirely when it is hidden, and the terminal does too
        // while its panel is closed.
        let next = if forward {
            if on_sidebar {
                Region::PathBar
            } else if editing_path {
                Region::Pane
            } else if on_terminal {
                if self.sidebar_visible {
                    Region::Sidebar
                } else {
                    Region::PathBar
                }
            } else if self.terminal_visible {
                Region::Terminal
            } else if self.sidebar_visible {
                Region::Sidebar
            } else {
                Region::PathBar
            }
        } else if on_terminal {
            Region::Pane
        } else if on_sidebar {
            if self.terminal_visible {
                Region::Terminal
            } else {
                Region::Pane
            }
        } else if editing_path && self.sidebar_visible {
            Region::Sidebar
        } else if editing_path {
            if self.terminal_visible {
                Region::Terminal
            } else {
                Region::Pane
            }
        } else {
            Region::PathBar
        };

        match next {
            Region::Sidebar => {
                self.cancel_path_edit(cx);
                window.focus(&self.sidebar_focus, cx);
            }
            Region::PathBar => self.begin_path_edit(window, cx),
            Region::Pane => {
                self.cancel_path_edit(cx);
                window.focus(&pane_focus, cx);
            }
            Region::Terminal => {
                self.cancel_path_edit(cx);
                window.focus(&self.terminal_focus, cx);
            }
        }
        cx.notify();
    }
}

/// Where a fresh session starts looking; overridable for testing the
/// performance fixture: `FERRET_START_DIR=/path/to/fixture/flat just dev`.
fn start_directory() -> VfsPath {
    if let Ok(dir) = std::env::var("FERRET_START_DIR")
        && !dir.is_empty()
    {
        return VfsPath::local(dir);
    }
    dirs::home_dir()
        .map(VfsPath::local)
        .unwrap_or_else(|| VfsPath::local("/"))
}

struct BrowserTint;

impl BrowserTint {
    fn folder(cx: &App) -> gpui::Hsla {
        crate::theme::BrowserPalette::resolve(cx).folder
    }
}

enum HistoryOp {
    Push,
    Back,
    Forward,
}

/// The three regions Tab moves between (§8).
enum Region {
    Sidebar,
    PathBar,
    Pane,
    Terminal,
}

/// One toast for a batch operation. A single failure shows its own message
/// unedited — every `VfsError` names its subject — and a batch leads with the
/// count so the toast does not become a wall of near-identical lines.
fn summarize_failures(operation: &str, failures: &[String]) -> Option<String> {
    match failures {
        [] => None,
        [only] => Some(only.clone()),
        [first, ..] => Some(format!(
            "{} items could not {operation}. First: {first}",
            failures.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_failure_is_shown_unedited() {
        // Every VfsError names its subject, so a single failure is already a
        // sentence the user can act on; wrapping it would only bury the path.
        let message = summarize_failures(
            "move to the trash",
            &["Ferret can't read /private/var/root — you don't have permission.".into()],
        );
        assert_eq!(
            message.as_deref(),
            Some("Ferret can't read /private/var/root — you don't have permission.")
        );
    }

    #[test]
    fn a_batch_leads_with_the_count() {
        let message = summarize_failures("paste", &["a failed".into(), "b failed".into()])
            .expect("failures must produce a message");
        assert!(message.starts_with("2 items could not paste"), "{message}");
        assert!(message.contains("a failed"), "the first error survives");
    }

    /// The string half of the clipboard, which is what actually round-trips
    /// on the pinned GPUI revision (D-42).
    fn paths_from_text(text: &str) -> Vec<String> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(std::path::Path::new)
            .filter(|path| path.is_absolute())
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn pasted_text_is_only_taken_as_paths_when_it_looks_like_paths() {
        // What Ferret itself writes, and what a terminal would paste back.
        assert_eq!(
            paths_from_text("/a/one.txt\n/a/two.txt"),
            ["/a/one.txt", "/a/two.txt"]
        );
        // Blank lines and stray indentation survive a trip through a text
        // editor without becoming empty paths.
        assert_eq!(paths_from_text("\n  /a/one.txt  \n\n"), ["/a/one.txt"]);

        // Ordinary prose is not a file list. Relative fragments are dropped
        // here; anything that survives and is not real is dropped by the
        // paste task's existence check, off the window thread.
        assert!(paths_from_text("Dear Sir or Madam,\n\nI write to you").is_empty());
        assert!(paths_from_text("notes.txt").is_empty(), "not absolute");
        assert!(paths_from_text("").is_empty());
    }

    #[test]
    fn success_produces_no_toast() {
        assert_eq!(summarize_failures("paste", &[]), None);
    }
}
