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
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, TitleBar,
    WindowExt as _,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
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
    /// Logical pixels; drag-resize returns with the resizable-split pass.
    sidebar_width: f32,
    clipboard: Option<(Vec<VfsPath>, bool)>,

    /// `Some` while the path bar renders an editable input.
    path_editing: Option<PathEdit>,

    volumes_cache: Option<(Instant, Vec<Place>)>,
}

struct BrowserTab {
    folder: Entity<Folder>,
    pane: Entity<FilePane>,
    back: Vec<VfsPath>,
    forward: Vec<VfsPath>,
    /// Lives here so replacing the tab's entities detaches everything.
    _subscriptions: Vec<Subscription>,
}

struct PathEdit {
    input: Entity<InputState>,
    _subscription: Subscription,
}

impl Browser {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            tabs: Vec::new(),
            active: 0,
            sidebar_visible: cx.global::<FerretState>().settings.sidebar.visible,
            sidebar_width: cx.global::<FerretState>().settings.sidebar.width,
            clipboard: None,
            path_editing: None,
            volumes_cache: None,
        };
        this.push_tab(start_directory(), window, cx);
        this
    }

    // ------------------------------------------------------------------
    // Tab plumbing

    fn push_tab(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let (folder, pane, subscriptions) = self.build_tab_entities(dir, window, cx);
        self.tabs.push(BrowserTab {
            folder,
            pane,
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
    ) -> (Entity<Folder>, Entity<FilePane>, Vec<Subscription>) {
        let ((sort_key, sort_order), show_hidden) = {
            let state = cx.global_mut::<FerretState>();
            let view = state.folder_view(&dir);
            (
                view.sort_key_and_order(),
                state.settings.browser.show_hidden,
            )
        };

        let folder = cx.new(|_| Folder::new(dir.clone(), sort_key, sort_order, show_hidden));
        folder.update(cx, |f, cx| f.start_loading(cx));

        let pane = cx.new(|cx| FilePane::new(folder.clone(), window, cx));

        let mut subscriptions = vec![cx.subscribe_in(
            &pane,
            window,
            |this, _pane, event: &PaneEvent, window, cx| match event {
                PaneEvent::EnterDirectory(path) => this.navigate(path.clone(), window, cx),
                PaneEvent::OpenFile(path) => this.open_with_default(path.clone(), cx),
            },
        )];
        // Listing progress repaints the status bar and title.
        subscriptions.push(cx.subscribe_in(&folder, window, |_, _, _, _, cx| {
            cx.notify();
        }));

        (folder, pane, subscriptions)
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
        let (folder, pane, subscriptions) = self.build_tab_entities(dir, window, cx);

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
        tab._subscriptions = subscriptions;

        self.path_editing = None;
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

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // One tab always stays so `+` has something to clone.
        }
        self.tabs.remove(ix);
        self.active = self.active.min(self.tabs.len() - 1);
        cx.notify();
    }

    /// Persist the current folder's view preferences before leaving it.
    fn record_current_view(&mut self, cx: &mut Context<Self>) {
        let tab = self.active();
        let dir = tab.folder.read(cx).dir.clone();
        let (sort_key, sort_order) = {
            let f = tab.folder.read(cx);
            (f.sort_key(), f.sort_order())
        };
        let view = FolderView {
            mode: ViewMode::List,
            sort_key,
            sort_order,
            hidden_columns: Vec::new(),
        };
        let state = cx.global_mut::<FerretState>();
        state.remember_folder_view(&dir, view);
        state.save_prefs();
    }

    // ------------------------------------------------------------------
    // Mutations

    fn open_with_default(&self, path: VfsPath, cx: &mut Context<Self>) {
        let Some(local) = path.as_local().map(Path::to_path_buf) else {
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.open_with_default(&local).await {
                eprintln!("open failed: {err}");
            }
            cx.update(|_| {});
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
            eprintln!("create folder failed: {err}");
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
            eprintln!("create file failed: {err}");
            return;
        }
        self.active()
            .pane
            .update(cx, |pane, cx| pane.begin_rename(path, window, cx));
    }

    fn trash_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let platform = cx.global::<FerretState>().platform.clone();
        cx.spawn(async move |_, cx| {
            for path in paths {
                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                    continue;
                };
                if let Err(err) = platform.move_to_trash(&local).await {
                    eprintln!("move to trash failed: {err}");
                }
            }
            cx.update(|_| {});
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
                            for path in paths {
                                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                                    continue;
                                };
                                if let Err(err) = platform.remove_permanently(&local).await {
                                    eprintln!("delete failed: {err}");
                                }
                            }
                            cx.update(|_| {});
                        })
                        .detach();
                        true
                    }
                })
        });
    }

    fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if !paths.is_empty() {
            self.clipboard = Some((paths, cut));
        }
    }

    fn paste_clipboard(&mut self, cx: &mut Context<Self>) {
        let Some((sources, cut)) = self.clipboard.take() else {
            return;
        };
        let restore_on_copy = (!cut).then(|| sources.clone());
        let dest_dir = self.active().folder.read(cx).dir.clone();
        let taken = self.entry_names(cx);
        let fs = cx.global::<FerretState>().fs;

        cx.spawn(async move |_, cx| {
            let mut taken = taken;
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
                    eprintln!("paste failed for {src}: {err}");
                }
            }
            cx.update(|_| {});
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
        match mode {
            ViewMode::List => cx.notify(), // already list
            // Icons and columns land with their views (next pass); until
            // then the segmented control reflects intent without lying
            // about rendering something it does not.
            ViewMode::Columns | ViewMode::Icons => {
                let _ = window;
                cx.notify();
            }
        }
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

        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));

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
        });
        cx.notify();
    }

    fn cancel_path_edit(&mut self, cx: &mut Context<Self>) {
        if self.path_editing.take().is_some() {
            cx.notify();
        }
    }
}

enum HistoryOp {
    Push,
    Back,
    Forward,
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
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tab = self.active();
        let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
        let back_enabled = !tab.back.is_empty();
        let forward_enabled = !tab.forward.is_empty();
        let show_hidden = cx.global::<FerretState>().settings.browser.show_hidden;
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

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dir = self.active().folder.read(cx).dir.clone();

        let mut side = v_flex()
            .id("sidebar")
            .overflow_y_scroll()
            .w_full()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .pt(px(6.))
            .pb(px(8.));

        side = side.child(section_label("Places"));
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

        side = side.child(section_label("Favorites"));
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
                Some(fav),
                cx,
            ));
        }

        side = side.child(section_label("Volumes"));
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

    /// One h22 pill row. `removable` carries the favorites context menu.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_place_row(
        &self,
        id: SharedString,
        label: &str,
        icon: &'static str,
        target: PathBuf,
        active: bool,
        removable: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = if active {
            gpui::white()
        } else {
            theme.sidebar_foreground
        };

        let this = cx.weak_entity();
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

        match removable {
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

fn section_label(label: &'static str) -> impl IntoElement {
    div()
        .h(px(20.))
        .mt(px(8.))
        .px(px(14.))
        .text_size(px(10.5))
        .line_height(px(20.))
        .font_weight(gpui::FontWeight::BOLD)
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

        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
                let mut t = Tab::new()
                    .prefix(Icon::default().path("icons/file/folder-base.svg"))
                    .label(title.to_string());
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
            .items_center()
            .h(px(32.))
            .px(px(6.))
            .gap(px(4.))
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
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

    fn render_path_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        if let Some(edit) = &self.path_editing {
            return v_flex()
                .h(px(30.))
                .px(px(10.))
                .border_b_1()
                .border_color(theme.border)
                .justify_center()
                .child(Input::new(&edit.input).small().appearance(false))
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

        h_flex()
            .h(px(30.))
            .px(px(8.))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                Icon::default()
                    .path("icons/file/folder-base.svg")
                    .size_4()
                    .text_color(BrowserTint::folder(cx)),
            )
            .child(Breadcrumb::new().children(items).flex_1().truncate())
            .child(tool_button_pencil(window, cx, this))
            .into_any_element()
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

        // Keyboard actions that belong to the shell rather than the pane.
        v_flex()
            .id("browser-root")
            .size_full()
            .key_context("Browser")
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
            .on_action(
                cx.listener(|this, _: &actions::CancelEdit, _, cx| this.cancel_path_edit(cx)),
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
            .on_action(cx.listener(|this, _: &actions::Paste, _, cx| this.paste_clipboard(cx)))
            .on_action(cx.listener(|this, _: &actions::Trash, _, cx| this.trash_selection(cx)))
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
                                .child(self.render_sidebar(cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .bg(bg)
                            .child(self.render_tab_strip(window, cx))
                            .child(self.render_path_bar(window, cx))
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
