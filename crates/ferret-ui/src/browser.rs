//! The browser shell: title bar, sidebar, tab strip, path bar, file pane and
//! status bar — the regions of `docs/ui-base.html` §3–§4, wired to the
//! interaction contract in §8.
//!
//! Ownership follows tabs: each [`BrowserTab`] owns its [`Folder`], its
//! [`FilePane`] and the subscriptions between them. Replacing a tab's
//! entities (navigation) drops the old subscriptions, which detaches the old
//! watcher and lets the old listing task see a dead entity and stop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_platform::Place;
use ferret_settings::{FolderView, SortKey, SortOrder, ThemeChoice, ViewMode};
use ferret_vfs::{Vfs as _, VfsPath};
use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
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
use crate::folder::{Folder, FolderEvent};
use crate::format::unique_name;
use crate::pane::{FilePane, PaneEvent};
use crate::state::FerretState;

/// How long the volumes list stays cached before the sidebar re-probes.
const VOLUMES_TTL: Duration = Duration::from_secs(15);

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
    /// Live drag state for the sidebar divider.
    sidebar_drag: Option<SidebarDrag>,
    /// Index of the tab currently being dragged, if any.
    tab_drag: Option<usize>,
    /// Index of the favourite currently being dragged, if any.
    favorite_drag: Option<usize>,
    /// Left edge of each tab in window space, refreshed every frame.
    ///
    /// `gpui-component`'s `Tab` stops propagation of its own pointer events,
    /// so a per-tab listener never runs; the strip therefore does its own hit
    /// testing, and this is the geometry it needs.
    tab_lefts: std::sync::Arc<std::sync::Mutex<Vec<gpui::Pixels>>>,
    clipboard: Option<(Vec<VfsPath>, bool)>,

    /// `Some` while the path bar renders an editable input.
    path_editing: Option<PathEdit>,

    /// The inline name filter, always mounted at the right of the path bar.
    filter_input: Entity<InputState>,
    /// Held so the filter input keeps notifying the folder.
    _filter_subscription: Subscription,

    volumes_cache: Option<(Instant, Vec<Place>)>,

    /// Focus for the sidebar, so Tab can reach it and it can show that it has
    /// focus (§8: "Focus must be visible").
    sidebar_focus: gpui::FocusHandle,
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
                    this.apply_filter((!text.is_empty()).then_some(text), cx);
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

        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            sidebar_visible: cx.global::<FerretState>().settings.sidebar.visible,
            sidebar_width: cx.global::<FerretState>().settings.sidebar.width,
            sidebar_drag: None,
            tab_drag: None,
            favorite_drag: None,
            tab_lefts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            clipboard: None,
            path_editing: None,
            filter_input,
            _filter_subscription: filter_subscription,
            volumes_cache: None,
            sidebar_focus: cx.focus_handle(),
        };
        this.push_tab(start_directory(), window, cx);
        this
    }

    // ------------------------------------------------------------------
    // Tab plumbing

    fn push_tab(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let (folder, pane, mode, subscriptions) = self.build_tab_entities(dir, window, cx);
        self.tabs.push(BrowserTab {
            folder,
            pane,
            view_mode: mode,
            back: Vec::new(),
            forward: Vec::new(),
            _subscriptions: subscriptions,
        });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// Build a fresh Folder + FilePane pair already loading `dir`.
    ///
    /// Per-folder preferences load here so navigating restores what the
    /// folder remembers before its first frame paints.
    fn build_tab_entities(
        &self,
        dir: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<Folder>,
        Entity<FilePane>,
        ViewMode,
        Vec<Subscription>,
    ) {
        let ((sort_key, sort_order), mode, hidden_columns, show_hidden) = {
            let state = cx.global_mut::<FerretState>();
            let view = state.folder_view(&dir);
            (
                view.sort_key_and_order(),
                view.mode,
                view.hidden_columns.clone(),
                state.settings.browser.show_hidden,
            )
        };

        let folder = cx.new(|_| Folder::new(dir.clone(), sort_key, sort_order, show_hidden));
        folder.update(cx, |f, cx| f.start_loading(cx));

        let pane = cx.new(|cx| FilePane::new(folder.clone(), mode, &hidden_columns, window, cx));

        let mut subscriptions = vec![cx.subscribe_in(
            &pane,
            window,
            |this, _pane, event: &PaneEvent, window, cx| match event {
                PaneEvent::EnterDirectory(path) => this.navigate(path.clone(), window, cx),
                PaneEvent::OpenFile(path) => this.open_with_default(path.clone(), window, cx),
                PaneEvent::DropPaths {
                    sources,
                    dest,
                    force_copy,
                } => this.transfer(sources.clone(), dest.clone(), *force_copy, window, cx),
                // Sort/view preferences changed: persist them for this folder.
                PaneEvent::ViewChanged => this.record_current_view(cx),
            },
        )];
        // Listing progress repaints the status bar and title.
        subscriptions.push(cx.subscribe_in(&folder, window, |_, _, _, _, cx| {
            cx.notify();
        }));

        (folder, pane, mode, subscriptions)
    }

    fn active(&self) -> &BrowserTab {
        &self.tabs[self.active]
    }

    fn active_mut(&mut self) -> &mut BrowserTab {
        &mut self.tabs[self.active]
    }

    /// Swap in fresh entities for the active tab, pushing history.
    fn swap_active_tab(
        &mut self,
        dir: VfsPath,
        history: HistoryOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.record_current_view(cx);
        let current = self.active().folder.read(cx).dir.clone();
        let (folder, pane, mode, subscriptions) = self.build_tab_entities(dir, window, cx);

        let tab = self.active_mut();
        match history {
            HistoryOp::Push => {
                tab.back.push(current);
                tab.forward.clear();
            }
            HistoryOp::Back => {
                tab.back.pop();
                tab.forward.push(current);
            }
            HistoryOp::Forward => {
                tab.forward.pop();
                tab.back.push(current);
            }
        }
        tab.folder = folder;
        tab.pane = pane;
        tab.view_mode = mode;
        tab._subscriptions = subscriptions;

        self.path_editing = None;
        // A filter belongs to the folder you typed it in, not to the tab.
        self.filter_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        cx.notify();
    }

    fn navigate(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.active().folder.read(cx).dir == dir {
            return;
        }
        self.swap_active_tab(dir, HistoryOp::Push, window, cx);
    }

    fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.active().back.last().cloned() else {
            return;
        };
        self.swap_active_tab(previous, HistoryOp::Back, window, cx);
    }

    fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.active().forward.last().cloned() else {
            return;
        };
        self.swap_active_tab(next, HistoryOp::Forward, window, cx);
    }

    fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.active().folder.read(cx).dir.parent() else {
            return;
        };
        self.navigate(parent, window, cx);
    }

    /// The open folder vanished: walk up to the nearest parent that still
    /// exists, so the user lands somewhere real rather than on another
    /// missing page.
    fn go_to_existing_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dir = self.active().folder.read(cx).dir.clone();
        while let Some(parent) = dir.parent() {
            if parent.as_local().is_some_and(Path::is_dir) {
                self.navigate(parent, window, cx);
                return;
            }
            dir = parent;
        }
        let home = cx.global::<FerretState>().home_dir();
        self.navigate(VfsPath::local(home), window, cx);
    }

    /// Whether the active folder is gone (deleted or unreachable), which
    /// swaps the pane for a banner.
    fn active_folder_is_gone(&self, cx: &Context<Self>) -> bool {
        self.active()
            .folder
            .read(cx)
            .error()
            .is_some_and(ferret_vfs::VfsError::is_gone)
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // One tab always stays so `+` has something to clone.
        }
        self.tabs.remove(ix);
        self.active = self.active.min(self.tabs.len() - 1);
        cx.notify();
    }

    /// Persist the current folder's view preferences: on every sort or
    /// view-mode change, and again before navigating away.
    fn record_current_view(&mut self, cx: &mut Context<Self>) {
        let tab = self.active();
        let dir = tab.folder.read(cx).dir.clone();
        let (sort_key, sort_order) = {
            let f = tab.folder.read(cx);
            (f.sort_key(), f.sort_order())
        };
        // The mode comes from the pane, not this tab's cached copy: a mode
        // set from the pane's own menu must not be reverted by a later sort.
        let view = FolderView {
            mode: tab.pane.read(cx).view_mode(),
            sort_key,
            sort_order,
            hidden_columns: tab.pane.read(cx).hidden_column_keys(),
        };
        let state = cx.global_mut::<FerretState>();
        state.remember_folder_view(&dir, view);
        state.save_prefs();
    }

    // ------------------------------------------------------------------
    // Mutations

    fn open_with_default(&mut self, path: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(local) = path.as_local().map(Path::to_path_buf) else {
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.open_with_default(&local).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
    }

    fn entry_names(&self, cx: &Context<Self>) -> std::collections::HashSet<String> {
        self.active().folder.read(cx).entry_names()
    }

    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = unique_name(&self.entry_names(cx), "untitled folder");
        let path = dir.join(&name);

        let fs = cx.global::<FerretState>().fs;
        // A single mkdir is sub-millisecond on local disks; blocking buys us
        // a live window to start the inline rename in. Remote backends will
        // route through an operation queue instead.
        if let Err(err) = smol_block_on(fs.create_dir(&path)) {
            window.push_notification(Notification::error(err.to_string()), cx);
            return;
        }
        self.active()
            .pane
            .update(cx, |pane, cx| pane.begin_rename(path, window, cx));
    }

    fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = unique_name(&self.entry_names(cx), "untitled.txt");
        let path = dir.join(&name);

        let fs = cx.global::<FerretState>().fs;
        if let Err(err) = smol_block_on(fs.create_file(&path)) {
            window.push_notification(Notification::error(err.to_string()), cx);
            return;
        }
        self.active()
            .pane
            .update(cx, |pane, cx| pane.begin_rename(path, window, cx));
    }

    fn trash_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();
            for path in paths {
                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                    continue;
                };
                if let Err(err) = platform.move_to_trash(&local).await {
                    failures.push(err.to_string());
                }
            }
            if let Some(message) = summarize_failures("move to the trash", &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
        // Watchers refresh the listing as entries vanish.
    }

    fn delete_selection_permanently(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let this = cx.weak_entity();
        let window_handle = window.window_handle();
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .title("Delete Permanently?")
                .description(format!(
                    "{count} item(s) will be deleted permanently. This can't be undone."
                ))
                .confirm()
                .on_ok({
                    let paths = paths.clone();
                    let this = this.clone();
                    move |_, _, cx| {
                        if this.upgrade().is_none() {
                            return true;
                        }
                        let platform = cx.global::<FerretState>().platform.clone();
                        let paths = paths.clone();
                        cx.spawn(async move |cx| {
                            let mut failures = 0usize;
                            let mut first_error = None;
                            for path in paths {
                                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                                    continue;
                                };
                                if let Err(err) = platform.remove_permanently(&local).await {
                                    failures += 1;
                                    first_error.get_or_insert(err.to_string());
                                }
                            }
                            if failures > 0 {
                                let message = match first_error {
                                    Some(err) if failures == 1 => err,
                                    Some(err) => {
                                        format!("{failures} items could not be deleted: {err}")
                                    }
                                    None => String::new(),
                                };
                                let _ = window_handle.update(cx, |_, window, cx| {
                                    window.push_notification(Notification::error(message), cx);
                                });
                            }
                        })
                        .detach();
                        true
                    }
                })
        });
    }

    /// Drop a set of paths into `dest` (§8).
    ///
    /// Move within a volume, copy across one, and copy either way when Alt
    /// was held. The cross-volume case is not guessed at from the paths —
    /// `rename` reports `CrossVolume` and the copy path takes over, which is
    /// the only answer that stays right for mount points and network shares.
    fn transfer(
        &mut self,
        sources: Vec<VfsPath>,
        dest: VfsPath,
        force_copy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();
        // Names already in the destination, so a copy beside its original
        // gets "file 2" rather than failing or overwriting.
        let taken = if dest == self.active().folder.read(cx).dir {
            self.entry_names(cx)
        } else {
            std::collections::HashSet::new()
        };

        cx.spawn(async move |_, cx| {
            let mut taken = taken;
            let mut failures = Vec::new();

            for src in sources {
                let Some(name) = src.file_name().map(str::to_owned) else {
                    continue;
                };
                let target = unique_name(&taken, &name);
                taken.insert(target.clone());
                let into = dest.join(&target);

                let result = if force_copy {
                    fs.copy(&src, &into).await
                } else {
                    match fs.rename(&src, &into).await {
                        // Across volumes there is no rename to do: copy, then
                        // remove the original, which is what a move means.
                        Err(ferret_vfs::VfsError::CrossVolume { .. }) => {
                            match fs.copy(&src, &into).await {
                                Ok(()) => fs.remove(&src, ferret_vfs::RemoveMode::Permanent).await,
                                Err(err) => Err(err),
                            }
                        }
                        other => other,
                    }
                };
                if let Err(err) = result {
                    failures.push(err.to_string());
                }
            }

            let verb = if force_copy { "copy" } else { "move" };
            if let Some(message) = summarize_failures(verb, &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
        // Both directories are watched, so the rows follow on their own.
    }

    fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if !paths.is_empty() {
            self.clipboard = Some((paths, cut));
        }
    }

    fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((sources, cut)) = self.clipboard.take() else {
            return;
        };
        let restore_on_copy = (!cut).then(|| sources.clone());
        let dest_dir = self.active().folder.read(cx).dir.clone();
        let taken = self.entry_names(cx);
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            let mut taken = taken;
            let mut failures = Vec::new();
            for src in sources {
                let Some(name) = src.file_name() else {
                    continue;
                };
                let dest_parent = src.parent();
                if cut && dest_parent.as_ref() == Some(&dest_dir) {
                    continue; // Pasting a cut item into its own folder.
                }
                let target = unique_name(&taken, name);
                taken.insert(target.clone());
                let dest = dest_dir.join(&target);

                let result = match fs.copy(&src, &dest).await {
                    Ok(()) => {
                        if cut {
                            fs.remove(&src, ferret_vfs::RemoveMode::Permanent).await
                        } else {
                            Ok(())
                        }
                    }
                    Err(err) => Err(err),
                };
                if let Err(err) = result {
                    failures.push(err.to_string());
                }
            }
            if let Some(message) = summarize_failures("paste", &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();

        // Copies stay on the clipboard: paste twice, paste elsewhere.
        self.clipboard = restore_on_copy.map(|sources| (sources, false));
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        let show = {
            let state = cx.global_mut::<FerretState>();
            state.settings.browser.show_hidden = !state.settings.browser.show_hidden;
            state.save_settings();
            state.settings.browser.show_hidden
        };
        self.active().folder.update(cx, |f, cx| {
            f.set_show_hidden(show);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.active()
            .folder
            .update(cx, |f, cx| f.reload_preserving_state(cx));
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        let state = cx.global_mut::<FerretState>();
        state.settings.sidebar.visible = self.sidebar_visible;
        state.save_settings();
        cx.notify();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::{Theme, ThemeMode};
        let next = if Theme::global(cx).mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.appearance.theme = if next.is_dark() {
                ThemeChoice::Dark
            } else {
                ThemeChoice::Light
            };
            state.save_settings();
        }
        Theme::change(next, Some(window), cx);
        cx.notify();
    }

    fn set_view_mode(&mut self, mode: ViewMode, window: &mut Window, cx: &mut Context<Self>) {
        let _ = window;
        self.active_mut().view_mode = mode;
        self.active()
            .pane
            .update(cx, |pane, cx| pane.set_view_mode(mode, cx));

        // Remember per folder (F-06): the mode rides the same FolderView
        // record as sort.
        self.record_current_view(cx);
        cx.notify();
    }

    fn add_current_to_favorites(&mut self, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let Some(local) = dir.as_local() else {
            return;
        };
        let state = cx.global_mut::<FerretState>();
        if !state.settings.sidebar.favorites.iter().any(|p| p == local) {
            state.settings.sidebar.favorites.push(local.to_path_buf());
            state.save_settings();
        }
        cx.notify();
    }

    fn favorites(&self, cx: &Context<Self>) -> Vec<PathBuf> {
        cx.global::<FerretState>()
            .settings
            .sidebar
            .favorites
            .clone()
    }

    fn places(&self, cx: &Context<Self>) -> Vec<Place> {
        cx.global::<FerretState>().platform.places()
    }

    fn volumes(&mut self, cx: &Context<Self>) -> Vec<Place> {
        if let Some((at, volumes)) = &self.volumes_cache
            && at.elapsed() < VOLUMES_TTL
        {
            return volumes.clone();
        }
        let volumes = cx.global::<FerretState>().platform.volumes();
        self.volumes_cache = Some((Instant::now(), volumes.clone()));
        volumes
    }

    // ------------------------------------------------------------------
    // Path bar

    fn begin_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_editing.is_some() {
            return;
        }
        let current = self.active().folder.read(cx).dir.to_string();

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value(&current);
            // The whole path starts selected, so typing replaces it the way
            // every other path field does. Without this the caret sits at
            // offset 0 and the first keystroke prepends to the current path.
            state.set_selected_range(0..current.chars().count(), cx);
            state
        });

        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.path_editing = None;
                    if let Some(target) = resolve_path_input(&text) {
                        this.navigate(target, window, cx);
                    }
                    cx.notify();
                }
                // Enter navigates to what is typed; the completions only
                // narrow, so they are refreshed on every keystroke.
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.refresh_completions(text, cx);
                }
                InputEvent::Blur => {
                    this.cancel_path_edit(cx);
                }
                _ => {}
            },
        );

        // Focus the editor directly. Preselection of the whole path arrives
        // with inline-editing polish; the fresh value is committed whole on
        // Enter either way.
        use gpui::Focusable as _;
        let handle = input.read(cx).focus_handle(cx).clone();
        window.focus(&handle, cx);

        self.path_editing = Some(PathEdit {
            input,
            _subscription: subscription,
            completions: Vec::new(),
            highlighted: 0,
            generation: 0,
        });
        cx.notify();
    }

    /// The single path into the folder's filter.
    ///
    /// `InputState::set_value` deliberately suppresses its change event, so a
    /// programmatic clear cannot rely on the subscription to notice — both
    /// routes call this instead.
    fn apply_filter(&mut self, filter: Option<String>, cx: &mut Context<Self>) {
        self.active().folder.update(cx, |f, cx| {
            f.set_filter(filter);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        cx.notify();
    }

    fn cancel_path_edit(&mut self, cx: &mut Context<Self>) {
        if self.path_editing.take().is_some() {
            cx.notify();
        }
    }

    /// Re-list the directory being typed into and keep the matches.
    ///
    /// The listing runs on the executor like every other one — a path bar
    /// that stutters on a slow network mount is worse than no completion at
    /// all — and a stale result is dropped by generation.
    fn refresh_completions(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_mut() else {
            return;
        };
        edit.generation += 1;
        let generation = edit.generation;

        let Some((dir, prefix)) = completion_target(&text) else {
            edit.completions.clear();
            edit.highlighted = 0;
            cx.notify();
            return;
        };

        let fs = cx.global::<FerretState>().fs;
        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&VfsPath::local(dir));
            let mut entries = Vec::new();
            while let Some(Ok(entry)) = futures::StreamExt::next(&mut stream).await {
                entries.push(entry);
            }
            let matches = matching_directories(&entries, &prefix);

            let _ = this.update(cx, |this, cx| {
                let Some(edit) = this.path_editing.as_mut() else {
                    return;
                };
                if edit.generation != generation {
                    return; // A newer keystroke already owns the popover.
                }
                edit.completions = matches;
                edit.highlighted = 0;
                cx.notify();
            });
        })
        .detach();
    }

    /// Move the highlight; the list wraps so holding the key cycles.
    fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_mut() else {
            return;
        };
        let n = edit.completions.len();
        if n == 0 {
            return;
        }
        let next = (edit.highlighted as isize + delta).rem_euclid(n as isize) as usize;
        edit.highlighted = next;
        cx.notify();
    }

    /// Put the highlighted completion in the field, with a trailing separator
    /// so the next Tab completes the level below it.
    ///
    /// With nothing to accept, Tab means what it means everywhere else and
    /// moves to the next region — otherwise focus would be stuck in the path
    /// bar whenever the typed path has no completions.
    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_ref() else {
            return;
        };
        let Some(path) = edit.completions.get(edit.highlighted).cloned() else {
            self.focus_region(true, window, cx);
            return;
        };
        let input = edit.input.clone();
        let mut text = path.to_string_lossy().into_owned();
        text.push(std::path::MAIN_SEPARATOR);
        input.update(cx, |state, cx| state.set_value(&text, window, cx));
        // Same reason as the filter: no change event follows `set_value`, so
        // the next level's completions are requested here rather than waiting
        // for a keystroke that may never come.
        self.refresh_completions(text, cx);
        cx.notify();
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
}

trait SortKeyAndOrder {
    fn sort_key_and_order(&self) -> (SortKey, SortOrder);
}

impl SortKeyAndOrder for FolderView {
    fn sort_key_and_order(&self) -> (SortKey, SortOrder) {
        (self.sort_key, self.sort_order)
    }
}

fn smol_block_on<F: std::future::Future>(fut: F) -> F::Output {
    smol::block_on(fut)
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

/// How many completions the path bar offers at once.
///
/// A popover taller than this stops being a hint and becomes a second file
/// list; typing one more character is the faster way to narrow it.
const MAX_COMPLETIONS: usize = 8;

/// Split typed path text into "the directory to list" and "the prefix to
/// match inside it".
///
/// A trailing separator means the directory itself is complete and everything
/// in it matches; anything else completes the last segment.
fn completion_target(text: &str) -> Option<(PathBuf, String)> {
    let expanded = expand_home(text.trim())?;
    if text.trim().is_empty() {
        return None;
    }
    if text.ends_with('/') || text.ends_with('\\') {
        return Some((expanded, String::new()));
    }
    let parent = expanded.parent()?.to_path_buf();
    let prefix = expanded.file_name()?.to_string_lossy().into_owned();
    Some((parent, prefix))
}

/// Directories in `dir` whose name starts with `prefix`, case-insensitively.
///
/// Directories only: the path bar navigates, and offering files would only
/// produce completions that cannot be accepted. Hidden entries stay out
/// unless the prefix reaches for them explicitly.
fn matching_directories(entries: &[ferret_vfs::Entry], prefix: &str) -> Vec<PathBuf> {
    let wanted = prefix.to_lowercase();
    let mut hits: Vec<&ferret_vfs::Entry> = entries
        .iter()
        .filter(|e| e.is_dir_like())
        .filter(|e| !e.is_hidden() || wanted.starts_with('.'))
        .filter(|e| e.name.to_lowercase().starts_with(&wanted))
        .collect();
    hits.sort_by(|a, b| crate::format::natural_compare(&a.name, &b.name));
    hits.into_iter()
        .take(MAX_COMPLETIONS)
        .filter_map(|e| e.path.as_local().map(Path::to_path_buf))
        .collect()
}

/// `~` and `~/…` expand against the home directory; everything else is taken
/// literally.
fn expand_home(text: &str) -> Option<PathBuf> {
    if text == "~" || text.starts_with("~/") {
        let home = dirs::home_dir()?;
        return Some(home.join(text.trim_start_matches('~').trim_start_matches('/')));
    }
    Some(PathBuf::from(text))
}

/// Expand `~`, trim slashes. Relative paths are accepted as-is from the
/// home-relative expansion only; typing a bare name navigates nowhere rather
/// than guessing a directory.
fn resolve_path_input(text: &str) -> Option<VfsPath> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let expanded = if text == "~" || text.starts_with("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(text.trim_start_matches('~').trim_start_matches('/')),
            None => return None,
        }
    } else {
        PathBuf::from(text)
    };
    Some(VfsPath::local(expanded))
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

// ---------------------------------------------------------------------------
// Rendering

/// Sidebar width bounds; mirrors `ferret-settings`' clamps so a drag can't
/// produce a width the settings file would refuse.
const MIN_SIDEBAR_WIDTH: f32 = ferret_settings::config::MIN_SIDEBAR_WIDTH;
const MAX_SIDEBAR_WIDTH: f32 = ferret_settings::config::MAX_SIDEBAR_WIDTH;

fn tool_button(id: &'static str, icon_path: &str, tooltip: &str) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::default().path(icon_path))
        .tooltip(tooltip)
}

/// Whether a sidebar place is the directory being browsed (or an ancestor of
/// it for volumes and drives, which stay lit while you descend into them).
fn place_is_current(current: &VfsPath, place: &Path) -> bool {
    let Some(cur) = current.as_local() else {
        return false;
    };
    if cur == place {
        return true;
    }
    cur.starts_with(place) && place.parent().is_some()
}

impl Browser {
    /// The drag handle on the sidebar's right edge.
    fn render_sidebar_divider(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-divider")
            .w(px(5.))
            .h_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .hover(|el| el.bg(cx.theme().primary.opacity(0.35)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.sidebar_drag = Some(SidebarDrag {
                        start_x: event.position.x,
                        start_width: this.sidebar_width,
                    });
                    cx.notify();
                }),
            )
    }

    /// Follow the pointer while the sidebar divider is held. Width is clamped
    /// here rather than on release so the sidebar never paints outside the
    /// bounds `ferret-settings` would accept.
    fn update_sidebar_drag(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.sidebar_drag else {
            return;
        };
        let delta = f32::from(x - drag.start_x);
        let width = (drag.start_width + delta).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// Persist on release, not on every move: a drag would otherwise rewrite
    /// the settings file once per frame.
    fn end_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_drag.take().is_none() {
            return;
        }
        let width = self.sidebar_width;
        let state = cx.global_mut::<FerretState>();
        state.settings.sidebar.width = width;
        state.save_settings();
        cx.notify();
    }

    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tab = self.active();
        let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
        let back_enabled = !tab.back.is_empty();
        let forward_enabled = !tab.forward.is_empty();
        let show_hidden = cx.global::<FerretState>().settings.browser.show_hidden;
        let _mode = tab.pane.read(cx).view_mode();
        let dark = gpui_component::Theme::global(cx).mode.is_dark();

        let view_group = h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(theme.radius)
            .bg(theme.secondary)
            .child(
                tool_button("view-list", toolbar_icon::VIEW_LIST, "List").on_click(cx.listener(
                    |_, _, _, cx| {
                        cx.dispatch_action(&actions::ViewAsList);
                    },
                )),
            )
            .child(
                tool_button("view-columns", toolbar_icon::VIEW_COLUMNS, "Columns").on_click(
                    cx.listener(|_, _, _, cx| {
                        cx.dispatch_action(&actions::ViewAsColumns);
                    }),
                ),
            )
            .child(
                tool_button("view-icons", toolbar_icon::VIEW_GRID, "Icons").on_click(cx.listener(
                    |_, _, _, cx| {
                        cx.dispatch_action(&actions::ViewAsIcons);
                    },
                )),
            );

        TitleBar::new().child(
            h_flex()
                .w_full()
                .items_center()
                .gap_1()
                .px_1()
                .child(
                    tool_button("sidebar-toggle", toolbar_icon::SIDEBAR, "Toggle Sidebar")
                        .selected(self.sidebar_visible)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                )
                .child(
                    tool_button("nav-back", toolbar_icon::BACK, "Back")
                        .disabled(!back_enabled)
                        .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                )
                .child(
                    tool_button("nav-forward", toolbar_icon::FORWARD, "Forward")
                        .disabled(!forward_enabled)
                        .on_click(cx.listener(|this, _, window, cx| this.go_forward(window, cx))),
                )
                .child(
                    tool_button("nav-up", toolbar_icon::UP, "Enclosing Folder")
                        .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
                )
                .child(
                    div()
                        .ml_2()
                        .max_w(px(300.))
                        .truncate()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(SharedString::from(format!("Ferret — {title}"))),
                )
                .child(div().flex_1())
                .child(view_group)
                .child(div().flex_1())
                .child(
                    tool_button("new-file", toolbar_icon::NEW_FILE, "New File")
                        .on_click(cx.listener(|this, _, window, cx| this.new_file(window, cx))),
                )
                .child(
                    tool_button("new-folder", toolbar_icon::NEW_FOLDER, "New Folder")
                        .on_click(cx.listener(|this, _, window, cx| this.new_folder(window, cx))),
                )
                .child(
                    tool_button(
                        "toggle-hidden",
                        if show_hidden {
                            toolbar_icon::HIDDEN_ON
                        } else {
                            toolbar_icon::HIDDEN_OFF
                        },
                        if show_hidden {
                            "Hide Hidden Files"
                        } else {
                            "Show Hidden Files"
                        },
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
                )
                .child(
                    tool_button("refresh", toolbar_icon::REFRESH, "Refresh")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                )
                .child(
                    tool_button(
                        "theme",
                        if dark {
                            toolbar_icon::SUN
                        } else {
                            toolbar_icon::MOON
                        },
                        "Toggle Light/Dark",
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_theme(window, cx))),
                )
                .child(
                    Button::new("more-menu")
                        .ghost()
                        .xsmall()
                        .icon(gpui_component::IconName::EllipsisVertical)
                        .dropdown_menu(|menu, _, _| {
                            menu.menu("Select All", Box::new(actions::SelectAll))
                                .menu("Paste", Box::new(actions::Paste))
                                .separator()
                                .menu("Show Hidden Files", Box::new(actions::ToggleHidden))
                                .separator()
                                .menu("Add Folder to Favorites", Box::new(actions::AddToFavorites))
                        }),
                ),
        )
    }

    // -- sidebar -----------------------------------------------------------

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dir = self.active().folder.read(cx).dir.clone();

        let focused = self.sidebar_focus.is_focused(window);
        let mut side = v_flex()
            .id("sidebar")
            .key_context(actions::context::SIDEBAR)
            .track_focus(&self.sidebar_focus)
            .overflow_y_scroll()
            .w_full()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            // Which region has focus has to be visible, or Tab moves
            // something the user cannot see.
            .when(focused, |el| el.border_r_2().border_color(theme.primary))
            .pt(px(6.))
            .pb(px(8.));

        side = side.child(section_label("Places", cx));
        for (i, place) in self.places(cx).into_iter().enumerate() {
            let active = place_is_current(&dir, &place.path);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("place-{i}")),
                &place.name,
                place.kind.icon_name(),
                place.path.clone(),
                active,
                None,
                cx,
            ));
        }

        side = side.child(section_label("Favorites", cx));
        for (i, fav) in self.favorites(cx).into_iter().enumerate() {
            let name = fav
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| fav.to_string_lossy().into_owned());
            let active = place_is_current(&dir, &fav);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("favorite-{i}")),
                &name,
                "icons/file/folder-base.svg",
                fav.clone(),
                active,
                Some((i, fav)),
                cx,
            ));
        }

        side = side.child(section_label("Volumes", cx));
        for (i, volume) in self.volumes(cx).into_iter().enumerate() {
            let active = place_is_current(&dir, &volume.path);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("volume-{i}")),
                &volume.name,
                volume.kind.icon_name(),
                volume.path.clone(),
                active,
                None,
                cx,
            ));
        }

        side
    }

    /// One h22 pill row. `favorite` carries the row's place in the favourites
    /// list, which is what makes it removable and re-orderable.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_place_row(
        &self,
        id: SharedString,
        label: &str,
        icon: &'static str,
        target: PathBuf,
        active: bool,
        favorite: Option<(usize, PathBuf)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = if active {
            crate::theme::text_on(theme.selection, theme.sidebar_foreground)
        } else {
            theme.sidebar_foreground
        };

        let drop_tint = theme.selection.opacity(0.5);
        let this = cx.weak_entity();
        let dragging = favorite
            .as_ref()
            .is_some_and(|(ix, _)| self.favorite_drag == Some(*ix));
        let row = h_flex()
            .id(id)
            .w_full()
            .h(px(22.))
            .mx(px(6.))
            .rounded(px(5.))
            .px(px(8.))
            .gap(px(8.))
            .items_center()
            .flex_shrink_0()
            .when(active, |el| el.bg(theme.selection))
            .when(!active, |el| el.hover(|el| el.bg(theme.secondary)))
            .when(dragging, |el| el.opacity(0.6))
            // Every sidebar row is a folder, so every one is a drop target
            // (§8). Dropping here moves into that place without navigating.
            .drag_over::<crate::pane::DraggedPaths>(move |style, _, _, _| style.bg(drop_tint))
            .on_drop({
                let this = this.clone();
                let dest = VfsPath::local(target.clone());
                move |dragged: &crate::pane::DraggedPaths, window, cx| {
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let (sources, dest) = (dragged.paths.clone(), dest.clone());
                    let force_copy = window.modifiers().alt;
                    this.update(cx, |this, cx| {
                        // Same guard the pane applies.
                        let sources = crate::pane::droppable(&sources, &dest);
                        if !sources.is_empty() {
                            this.transfer(sources, dest, force_copy, window, cx);
                        }
                    });
                }
            })
            // Favourites reorder by drag (§8). Driven by mouse-move with the
            // button held, so the row's own click handler still navigates.
            .when_some(favorite.as_ref().map(|(ix, _)| *ix), |el, ix| {
                el.on_mouse_move(
                    cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(gpui::MouseButton::Left) {
                            return;
                        }
                        match this.favorite_drag {
                            None => {
                                this.favorite_drag = Some(ix);
                                cx.notify();
                            }
                            Some(_) => this.move_dragged_favorite_to(ix, cx),
                        }
                    }),
                )
            })
            .child(
                Icon::default()
                    .path(icon)
                    .size_4()
                    .text_color(fg.opacity(0.9)),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(label.to_string()),
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let target = VfsPath::local(target.clone());
                this.update(cx, |this, cx| {
                    this.navigate(target, window, cx);
                });
            });

        match favorite.map(|(_, path)| path) {
            Some(path) => row
                .context_menu(move |menu, _, _| {
                    menu.item(PopupMenuItem::new("Remove from Favorites").on_click({
                        let path = path.clone();
                        move |_, _, cx| {
                            use gpui::BorrowAppContext as _;
                            cx.update_global(|state: &mut FerretState, _| {
                                state
                                    .settings
                                    .sidebar
                                    .favorites
                                    .retain(|p| p.as_path() != path);
                                state.save_settings();
                            });
                        }
                    }))
                })
                .into_any_element(),
            None => row.into_any_element(),
        }
    }
}

/// A sidebar group heading (§4): h20, 10.5 px, bold, uppercase, in
/// `--app-fg-faint` — muted foreground at 70% (§7).
fn section_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .h(px(20.))
        .mt(px(8.))
        .px(px(14.))
        .text_size(px(10.5))
        .line_height(px(20.))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(cx.theme().muted_foreground.opacity(0.7))
        .child(label.to_uppercase())
}

// -- tab strip / path bar / status bar --------------------------------------

impl Browser {
    fn render_tab_strip(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();

        // Each tab reports its left edge from inside its own prefix slot,
        // which is the only element in a `Tab` this crate owns.
        self.tab_lefts
            .lock()
            .unwrap()
            .resize(self.tabs.len(), px(0.));

        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
                let dragging = self.tab_drag == Some(ix);
                let lefts = self.tab_lefts.clone();
                let mut t = Tab::new()
                    .prefix(
                        div()
                            .on_prepaint(move |bounds, _, _| {
                                if let Some(slot) = lefts.lock().unwrap().get_mut(ix) {
                                    *slot = bounds.origin.x;
                                }
                            })
                            .child(Icon::default().path("icons/file/folder-base.svg")),
                    )
                    .label(title.to_string())
                    .when(dragging, |t| t.opacity(0.6));
                if self.tabs.len() > 1 {
                    let this = cx.weak_entity();
                    t = t.suffix(
                        Button::new(SharedString::from(format!("close-tab-{ix}")))
                            .ghost()
                            .xsmall()
                            .icon(gpui_component::IconName::Close)
                            .on_click(move |_, _, cx| {
                                if let Some(this) = this.upgrade() {
                                    this.update(cx, |this, cx| this.close_tab(ix, cx));
                                }
                            }),
                    );
                }
                t
            })
            .collect::<Vec<_>>();

        let this = cx.weak_entity();
        let active_dir = self.active().folder.read(cx).dir.clone();
        let plus = Button::new("new-tab")
            .ghost()
            .xsmall()
            .icon(gpui_component::IconName::Plus)
            .tooltip("New Tab")
            .on_click(move |_, window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        let dir = this.active().folder.read(cx).dir.clone();
                        let _ = &active_dir;
                        this.push_tab(dir, window, cx);
                    });
                }
            });

        h_flex()
            .id("tab-strip")
            .items_center()
            .h(px(32.))
            .px(px(6.))
            .gap(px(4.))
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
            // The whole drag lives on mouse-move: `Tab` stops propagation of
            // its own mouse-down, so a press is never observable here — but a
            // move with the button held is, and the first one still lands on
            // the tab that was pressed.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if event.pressed_button != Some(gpui::MouseButton::Left) {
                    this.end_tab_drag(cx);
                    return;
                }
                let Some(ix) = this.tab_at(event.position.x) else {
                    return;
                };
                match this.tab_drag {
                    None => {
                        this.tab_drag = Some(ix);
                        cx.notify();
                    }
                    Some(_) => this.move_dragged_tab_to(ix, cx),
                }
            }))
            .child(
                TabBar::new("browser-tabs")
                    .selected_index(self.active)
                    .text_size(px(12.5))
                    .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                        if *ix < this.tabs.len() {
                            this.active = *ix;
                            cx.notify();
                        }
                    }))
                    .children(tabs),
            )
            .child(plus)
    }

    /// Which tab sits under window-space `x`: the last one whose left edge is
    /// at or before the pointer. `None` before the first paint, or left of the
    /// first tab.
    fn tab_at(&self, x: gpui::Pixels) -> Option<usize> {
        let lefts = self.tab_lefts.lock().unwrap();
        lefts
            .iter()
            .rposition(|&left| left <= x)
            .filter(|_| lefts.iter().any(|&left| left > px(0.)))
    }

    /// Live tab reordering: the dragged tab takes `to`'s slot and everything
    /// between shifts, which is what makes the strip follow the pointer.
    fn move_dragged_tab_to(&mut self, to: usize, cx: &mut Context<Self>) {
        let Some(from) = self.tab_drag else {
            return;
        };
        if from == to || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = to;
        self.tab_drag = Some(to);
        cx.notify();
    }

    fn end_tab_drag(&mut self, cx: &mut Context<Self>) {
        if self.tab_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Reorder favourites live while one is dragged over another.
    fn move_dragged_favorite_to(&mut self, to: usize, cx: &mut Context<Self>) {
        let Some(from) = self.favorite_drag else {
            return;
        };
        if from == to {
            return;
        }
        let state = cx.global_mut::<FerretState>();
        let favorites = &mut state.settings.sidebar.favorites;
        if from >= favorites.len() || to >= favorites.len() {
            return;
        }
        let moved = favorites.remove(from);
        favorites.insert(to, moved);
        self.favorite_drag = Some(to);
        cx.notify();
    }

    /// Tab cycles sidebar → path bar → file pane (§8).
    ///
    /// The path bar only holds focus while it is being edited, so Tab opens
    /// the editor when it lands there rather than skipping a region that
    /// exists but has nothing focusable in it.
    fn focus_region(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable as _;

        let pane_focus = self.active().pane.read(cx).focus_handle(cx);
        let editing_path = self.path_editing.is_some();
        let on_sidebar = self.sidebar_focus.is_focused(window);

        // Where focus is now decides where it goes; the sidebar drops out of
        // the cycle entirely when it is hidden.
        let next = if forward {
            if on_sidebar {
                Region::PathBar
            } else if editing_path {
                Region::Pane
            } else if self.sidebar_visible {
                Region::Sidebar
            } else {
                Region::PathBar
            }
        } else if on_sidebar {
            Region::Pane
        } else if editing_path && self.sidebar_visible {
            Region::Sidebar
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
        }
        cx.notify();
    }

    /// Release ends whichever drag is in flight; a pointer-up carries no clue
    /// as to which one it was, so all three are settled together.
    fn end_drags(&mut self, cx: &mut Context<Self>) {
        self.end_sidebar_drag(cx);
        self.end_tab_drag(cx);
        self.end_favorite_drag(cx);
    }

    /// Written once, on release: a drag would otherwise rewrite the settings
    /// file on every frame it crosses a row.
    fn end_favorite_drag(&mut self, cx: &mut Context<Self>) {
        if self.favorite_drag.take().is_none() {
            return;
        }
        cx.global_mut::<FerretState>().save_settings();
        cx.notify();
    }

    fn render_path_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        // The filter rides the path bar in both states (§2: "inline filter"),
        // so narrowing a listing never costs you the breadcrumb.
        let filter = div()
            .w(px(160.))
            .flex_shrink_0()
            .child(Input::new(&self.filter_input).small().cleanable(true));

        let bar = h_flex()
            .h(px(30.))
            .px(px(8.))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background);

        if self.path_editing.is_some() {
            let completions = self.render_completions(cx);
            let input = self.path_editing.as_ref().map(|edit| edit.input.clone());
            return bar
                .key_context(actions::context::PATH_BAR)
                .when_some(input, |bar, input| {
                    bar.child(
                        div()
                            .relative()
                            .flex_1()
                            .child(Input::new(&input).small())
                            // Deferred so it paints above the file pane below
                            // it: an absolutely-positioned sibling would still
                            // be covered by elements that come later.
                            .child(gpui::deferred(completions)),
                    )
                })
                .child(filter)
                .into_any_element();
        }

        let dir = self.active().folder.read(cx).dir.clone();
        let crumbs = breadcrumb_targets(&dir);
        let this = cx.weak_entity();

        let mut items = Vec::with_capacity(crumbs.len());
        for (label, target) in crumbs {
            let is_last = target == dir;
            items.push(BreadcrumbItem::new(label).when(!is_last, |item| {
                let this = this.clone();
                let target = target.clone();
                item.on_click(move |_, window, cx| {
                    // A crumb navigates; without this the click would also
                    // reach the bar behind it and open the editor instead.
                    cx.stop_propagation();
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let target = target.clone();
                    this.update(cx, |this, cx| {
                        this.navigate(target, window, cx);
                    });
                })
            }));
        }

        // §4: the bar itself becomes a text input on click — the pencil is one
        // hit target for that, not the only one. The filter and the pencil sit
        // outside this region so clicking them does not start a path edit.
        let crumbs = h_flex()
            .id("path-bar-crumbs")
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_2()
            .on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.begin_path_edit(window, cx)),
            )
            .child(
                Icon::default()
                    .path("icons/file/folder-base.svg")
                    .size_4()
                    .text_color(BrowserTint::folder(cx)),
            )
            .child(Breadcrumb::new().children(items).flex_1().truncate());

        bar.child(crumbs)
            .child(tool_button_pencil(window, cx, this))
            .child(filter)
            .into_any_element()
    }

    /// The completion list under the path field (§6). Empty until a query
    /// lands, so it never flashes an empty box while the listing runs.
    fn render_completions(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let Some(edit) = &self.path_editing else {
            return div().into_any_element();
        };
        if edit.completions.is_empty() {
            return div().into_any_element();
        }

        let highlighted = edit.highlighted;
        let rows: Vec<_> = edit
            .completions
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let label = path.to_string_lossy().into_owned();
                let target = path.clone();
                h_flex()
                    .id(("completion", ix))
                    .w_full()
                    .h(px(24.))
                    .px(px(8.))
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .when(ix == highlighted, |row| row.bg(theme.selection))
                    .when(ix != highlighted, |row| {
                        row.hover(|row| row.bg(theme.secondary))
                    })
                    .child(
                        Icon::default()
                            .path("icons/file/folder-base.svg")
                            .size_4()
                            .text_color(BrowserTint::folder(cx)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if ix == highlighted {
                                crate::theme::text_on(theme.selection, theme.foreground)
                            } else {
                                theme.foreground
                            })
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if let Some(edit) = this.path_editing.as_mut() {
                            edit.highlighted = ix;
                        }
                        let _ = &target;
                        this.accept_completion(window, cx);
                    }))
            })
            .collect();

        v_flex()
            .absolute()
            .top_full()
            .left_0()
            .mt(px(4.))
            .w(px(420.))
            .p(px(4.))
            .gap(px(1.))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_md()
            .children(rows)
            .into_any_element()
    }

    fn render_gone_banner(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dir = self.active().folder.read(cx).dir.clone();
        h_flex()
            .h(px(36.))
            .px(px(12.))
            .gap_3()
            .items_center()
            .bg(theme.warning.opacity(0.14))
            .border_b_1()
            .border_color(theme.warning.opacity(0.5))
            .child(
                gpui_component::Icon::new(gpui_component::IconName::TriangleAlert)
                    .size_4()
                    .text_color(theme.warning),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .text_size(px(13.))
                    .child(SharedString::from(format!(
                        "Ferret can't find this folder. It may have been deleted or moved: {dir}"
                    ))),
            )
            .child(
                Button::new("gone-go-up")
                    .small()
                    .label("Go to Parent")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.go_to_existing_parent(window, cx);
                    })),
            )
    }

    fn render_status_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = self.active().folder.read(cx);
        let loading = folder.is_loading();
        let items = folder.row_count();
        let selected = folder.selection_count();
        let size = folder.selection_size();

        let mut left = format!("{items} items");
        if loading {
            left.push_str(" · loading…");
        }
        let right = if selected > 0 {
            format!("{selected} selected — {}", crate::format::format_size(size))
        } else {
            String::new()
        };

        StatusBar::new().left(left).right(right)
    }
}

/// Tiny shim so the path-bar pencil can reach the browser without another
/// listener context.
fn tool_button_pencil(
    _window: &mut Window,
    _cx: &mut Context<'_, Browser>,
    this: WeakEntity<Browser>,
) -> Button {
    Button::new("edit-path")
        .ghost()
        .xsmall()
        .icon(Icon::default().path(toolbar_icon::PENCIL))
        .tooltip("Edit Path")
        .on_click(move |_, window, cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.begin_path_edit(window, cx));
            }
        })
}

struct BrowserTint;

impl BrowserTint {
    fn folder(cx: &App) -> gpui::Hsla {
        crate::theme::BrowserPalette::resolve(cx).folder
    }
}

/// `(label, target)` pairs from root to `dir`, with the home prefix folded
/// into a single `~`.
fn breadcrumb_targets(dir: &VfsPath) -> Vec<(String, VfsPath)> {
    let Some(local) = dir.as_local() else {
        return vec![];
    };
    let home = dirs::home_dir();

    let mut stack: Vec<PathBuf> = local.ancestors().map(Path::to_path_buf).collect();
    stack.reverse(); // root first

    // Fold the home prefix away when we are inside it.
    if let Some(home) = home.as_deref()
        && local.starts_with(home)
        && local != home
    {
        stack.retain(|p| !p.starts_with(home));
        let mut out = vec![("~".to_string(), VfsPath::local(home.to_path_buf()))];
        let suffix = local.strip_prefix(home).unwrap_or(local);
        let mut acc = home.to_path_buf();
        for component in suffix.components() {
            acc.push(component.as_os_str());
            out.push((
                component.as_os_str().to_string_lossy().into_owned(),
                VfsPath::local(acc.clone()),
            ));
        }
        return out;
    }

    stack
        .into_iter()
        .map(|p| {
            let label = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string());
            (label, VfsPath::local(p))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Root render

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let folder_gone = self.active_folder_is_gone(cx);
        let dragging =
            self.sidebar_drag.is_some() || self.tab_drag.is_some() || self.favorite_drag.is_some();

        // Keyboard actions that belong to the shell rather than the pane.
        v_flex()
            .id("browser-root")
            .size_full()
            .key_context("Browser")
            // Drags start on a 5 px divider or a tab but continue wherever the
            // pointer goes, so both are followed from the root.
            .when(dragging, |el| {
                el.on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.update_sidebar_drag(event.position.x, cx);
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.end_drags(cx)),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.end_drags(cx)),
                )
            })
            .on_action(cx.listener(|this, _: &actions::Back, window, cx| this.go_back(window, cx)))
            .on_action(
                cx.listener(|this, _: &actions::Forward, window, cx| this.go_forward(window, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::Up, window, cx| this.go_up(window, cx)))
            .on_action(cx.listener(|this, _: &actions::Refresh, _, cx| this.refresh(cx)))
            .on_action(cx.listener(|this, _: &actions::ToggleHidden, _, cx| this.toggle_hidden(cx)))
            .on_action(
                cx.listener(|this, _: &actions::ToggleSidebar, _, cx| this.toggle_sidebar(cx)),
            )
            .on_action(cx.listener(|this, _: &actions::NewTab, window, cx| {
                let dir = this.active().folder.read(cx).dir.clone();
                this.push_tab(dir, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::CloseTab, _, cx| this.close_tab(this.active, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::FocusPathBar, window, cx| {
                this.begin_path_edit(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::FocusFilter, window, cx| {
                use gpui::Focusable as _;
                let handle = this.filter_input.read(cx).focus_handle(cx).clone();
                window.focus(&handle, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ClearFilter, window, cx| {
                // Both halves, explicitly: `set_value` emits no change event,
                // so the field and the listing would otherwise disagree.
                this.filter_input.update(cx, |state, cx| {
                    state.set_value("", window, cx);
                });
                this.apply_filter(None, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::CancelEdit, _, cx| this.cancel_path_edit(cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::FocusNextRegion, window, cx| {
                    this.focus_region(true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::FocusPrevRegion, window, cx| {
                    this.focus_region(false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::CompletionNext, _, cx| this.move_completion(1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::CompletionPrev, _, cx| {
                    this.move_completion(-1, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::AcceptCompletion, window, cx| {
                    this.accept_completion(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ViewAsList, window, cx| {
                this.set_view_mode(ViewMode::List, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ViewAsColumns, window, cx| {
                this.set_view_mode(ViewMode::Columns, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ViewAsIcons, window, cx| {
                this.set_view_mode(ViewMode::Icons, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::ToggleTheme, window, cx| {
                this.toggle_theme(window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::AddToFavorites, _, cx| {
                this.add_current_to_favorites(cx)
            }))
            .on_action(cx.listener(|this, _: &actions::Copy, _, cx| this.copy_selection(false, cx)))
            .on_action(cx.listener(|this, _: &actions::Cut, _, cx| this.copy_selection(true, cx)))
            .on_action(
                cx.listener(|this, _: &actions::Paste, window, cx| {
                    this.paste_clipboard(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::Trash, window, cx| {
                    this.trash_selection(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::DeletePermanently, window, cx| {
                    this.delete_selection_permanently(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::NewFolder, window, cx| this.new_folder(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::NewFile, window, cx| this.new_file(window, cx)),
            )
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .when(self.sidebar_visible, |el| {
                        el.child(
                            div()
                                .w(px(self.sidebar_width))
                                .h_full()
                                .flex_shrink_0()
                                .child(self.render_sidebar(window, cx)),
                        )
                        .child(self.render_sidebar_divider(cx))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .bg(bg)
                            .child(self.render_tab_strip(window, cx))
                            .child(self.render_path_bar(window, cx))
                            .when(folder_gone, |el| el.child(self.render_gone_banner(cx)))
                            .child(self.render_active_pane(window, cx))
                            .child(self.render_status_bar(cx)),
                    ),
            )
    }
}

impl Browser {
    /// The pane of the active tab, focused when switched to.
    fn render_active_pane(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> AnyPane {
        let pane = self.active().pane.clone();
        let _ = window;
        AnyPane { pane }
    }
}

struct AnyPane {
    pane: Entity<FilePane>,
}

impl IntoElement for AnyPane {
    type Element = <Entity<FilePane> as IntoElement>::Element;

    fn into_element(self) -> Self::Element {
        self.pane.into_element()
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

    #[test]
    fn success_produces_no_toast() {
        assert_eq!(summarize_failures("paste", &[]), None);
    }

    #[test]
    fn the_breadcrumb_folds_the_home_prefix_into_a_tilde() {
        let Some(home) = dirs::home_dir() else {
            return; // No home on this machine; nothing to fold.
        };
        let crumbs = breadcrumb_targets(&VfsPath::local(home.join("Projects/ferret")));
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["~", "Projects", "ferret"]);

        // The tilde still has to navigate somewhere real.
        assert_eq!(crumbs[0].1, VfsPath::local(home));
    }

    fn dir_entry(name: &str, hidden: bool) -> ferret_vfs::Entry {
        ferret_vfs::Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind: ferret_vfs::EntryKind::Dir,
            metadata: Some(ferret_vfs::Metadata {
                hidden,
                ..Default::default()
            }),
            link_target: None,
        }
    }

    fn file_entry(name: &str) -> ferret_vfs::Entry {
        ferret_vfs::Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind: ferret_vfs::EntryKind::File,
            metadata: Some(ferret_vfs::Metadata::default()),
            link_target: None,
        }
    }

    #[test]
    fn a_trailing_separator_lists_the_directory_itself() {
        let (dir, prefix) = completion_target("/usr/local/").expect("a directory to list");
        assert_eq!(dir, PathBuf::from("/usr/local"));
        assert_eq!(prefix, "", "everything inside it matches");

        // Without one, the last segment is what is being completed.
        let (dir, prefix) = completion_target("/usr/lo").expect("a directory to list");
        assert_eq!(dir, PathBuf::from("/usr"));
        assert_eq!(prefix, "lo");
    }

    #[test]
    fn completion_expands_the_home_prefix() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let (dir, prefix) = completion_target("~/Doc").expect("a directory to list");
        assert_eq!(dir, home);
        assert_eq!(prefix, "Doc");
    }

    #[test]
    fn only_directories_complete_a_path() {
        let entries = vec![
            dir_entry("Documents", false),
            dir_entry("Downloads", false),
            file_entry("Doc.txt"),
            dir_entry("Music", false),
        ];

        // A file can never be navigated to, so offering it would produce a
        // completion that cannot be accepted.
        let hits = matching_directories(&entries, "Do");
        let names: Vec<_> = hits
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Documents", "Downloads"]);

        // Matching is case-insensitive.
        assert_eq!(matching_directories(&entries, "do").len(), 2);
    }

    #[test]
    fn hidden_directories_need_to_be_asked_for() {
        let entries = vec![dir_entry(".config", true), dir_entry("code", false)];
        assert_eq!(matching_directories(&entries, "").len(), 1, "code only");

        // Typing the dot reaches for them explicitly.
        let hits = matching_directories(&entries, ".");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with(".config"));
    }

    #[test]
    fn the_completion_list_is_capped() {
        let entries: Vec<_> = (0..50)
            .map(|i| dir_entry(&format!("dir-{i:02}"), false))
            .collect();
        assert_eq!(matching_directories(&entries, "dir").len(), MAX_COMPLETIONS);
    }

    #[test]
    fn a_bare_name_is_not_guessed_into_a_path() {
        assert!(resolve_path_input("   ").is_none());
        assert_eq!(
            resolve_path_input("/usr/local"),
            Some(VfsPath::local("/usr/local"))
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolve_path_input("~"), Some(VfsPath::local(home)));
        }
    }
}
