use ferret_ui::actions as browser_actions;
use ferret_ui::{Assets, FerretState, RootView};
use gpui::{
    App, AppContext as _, KeyBinding, QuitMode, WindowBounds, WindowOptions, actions, px, size,
};
use gpui_component::Root;

actions!(ferret, [Quit]);

fn main() {
    // `Assets` supplies the SVGs behind `IconName` and Ferret's own icons;
    // without it every icon renders blank.
    gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            // Must run before any gpui-component type is constructed.
            gpui_component::init(cx);

            // Global app state before any view reads it.
            cx.set_global(ferret_ui_state());

            register_actions(cx);
            bind_keys(cx);
            apply_startup_theme(cx);

            open_main_window(cx);
        });
}

fn ferret_ui_state() -> FerretState {
    ferret_ui::load_state()
}

fn register_actions(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    // Window lifecycle belongs to the binary, not to a view.
    cx.on_action(|_: &browser_actions::NewWindow, cx| open_main_window(cx));
}

fn bind_keys(cx: &mut App) {
    use browser_actions::context;

    cx.bind_keys([
        // Quit — both platforms, always.
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, None),
        // History and hierarchy.
        KeyBinding::new("cmd-[", browser_actions::Back, None),
        KeyBinding::new("alt-left", browser_actions::Back, None),
        KeyBinding::new("cmd-]", browser_actions::Forward, None),
        KeyBinding::new("alt-right", browser_actions::Forward, None),
        KeyBinding::new("cmd-up", browser_actions::Up, None),
        KeyBinding::new("alt-up", browser_actions::Up, None),
        // The mirror of Up: descend into the cursor row.
        KeyBinding::new(
            "cmd-down",
            browser_actions::EnterDirectory,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "alt-down",
            browser_actions::EnterDirectory,
            Some(context::FILE_PANE),
        ),
        // Refresh.
        KeyBinding::new("cmd-r", browser_actions::Refresh, None),
        KeyBinding::new("f5", browser_actions::Refresh, None),
        // Hidden files.
        // The action was declared and handled but never bound, so the sidebar
        // could only be toggled from the toolbar button.
        KeyBinding::new("cmd-b", browser_actions::ToggleSidebar, None),
        KeyBinding::new("ctrl-b", browser_actions::ToggleSidebar, None),
        KeyBinding::new("cmd-j", browser_actions::ToggleTerminal, None),
        KeyBinding::new("ctrl-j", browser_actions::ToggleTerminal, None),
        KeyBinding::new("ctrl-`", browser_actions::FocusTerminalOrPane, None),
        KeyBinding::new("cmd-shift-.", browser_actions::ToggleHidden, None),
        KeyBinding::new("ctrl-h", browser_actions::ToggleHidden, None),
        // View modes.
        KeyBinding::new("cmd-1", browser_actions::ViewAsList, None),
        KeyBinding::new("ctrl-1", browser_actions::ViewAsList, None),
        KeyBinding::new("cmd-2", browser_actions::ViewAsColumns, None),
        KeyBinding::new("ctrl-2", browser_actions::ViewAsColumns, None),
        KeyBinding::new("cmd-3", browser_actions::ViewAsIcons, None),
        KeyBinding::new("ctrl-3", browser_actions::ViewAsIcons, None),
        // Windows.
        KeyBinding::new("cmd-n", browser_actions::NewWindow, None),
        KeyBinding::new("ctrl-n", browser_actions::NewWindow, None),
        // Tabs.
        KeyBinding::new("cmd-t", browser_actions::NewTab, None),
        KeyBinding::new("ctrl-t", browser_actions::NewTab, None),
        KeyBinding::new("cmd-w", browser_actions::CloseTab, None),
        KeyBinding::new("ctrl-w", browser_actions::CloseTab, None),
        // Path bar.
        KeyBinding::new("cmd-l", browser_actions::FocusPathBar, None),
        KeyBinding::new("ctrl-l", browser_actions::FocusPathBar, None),
        // Tab moves between sidebar, path bar and file pane (§8). Bound in
        // each region's own context — including the table's, which otherwise
        // claims Tab to walk its columns and would trap focus in the pane.
        KeyBinding::new("tab", browser_actions::FocusNextRegion, None),
        KeyBinding::new("shift-tab", browser_actions::FocusPrevRegion, None),
        KeyBinding::new(
            "tab",
            browser_actions::FocusNextRegion,
            Some(context::DATA_TABLE),
        ),
        KeyBinding::new(
            "shift-tab",
            browser_actions::FocusPrevRegion,
            Some(context::DATA_TABLE),
        ),
        KeyBinding::new(
            "tab",
            browser_actions::FocusNextRegion,
            Some(context::SIDEBAR),
        ),
        KeyBinding::new(
            "shift-tab",
            browser_actions::FocusPrevRegion,
            Some(context::SIDEBAR),
        ),
        // Inline filter.
        KeyBinding::new("cmd-f", browser_actions::FocusFilter, None),
        KeyBinding::new("ctrl-f", browser_actions::FocusFilter, None),
        KeyBinding::new(
            "escape",
            browser_actions::CancelEdit,
            Some(context::PATH_BAR),
        ),
        // Path completion: Tab accepts, arrows move through the list. Enter
        // stays reserved for "go to what I typed", which is what a path bar
        // is for.
        KeyBinding::new(
            "tab",
            browser_actions::AcceptCompletion,
            Some(context::PATH_BAR),
        ),
        KeyBinding::new(
            "down",
            browser_actions::CompletionNext,
            Some(context::PATH_BAR),
        ),
        KeyBinding::new(
            "up",
            browser_actions::CompletionPrev,
            Some(context::PATH_BAR),
        ),
        // File pane: open, rename, delete.
        KeyBinding::new("enter", browser_actions::Open, Some(context::FILE_PANE)),
        KeyBinding::new("f2", browser_actions::Rename, Some(context::FILE_PANE)),
        KeyBinding::new(
            "cmd-backspace",
            browser_actions::Trash,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new("delete", browser_actions::Trash, Some(context::FILE_PANE)),
        KeyBinding::new(
            "cmd-shift-backspace",
            browser_actions::DeletePermanently,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "shift-delete",
            browser_actions::DeletePermanently,
            Some(context::FILE_PANE),
        ),
        // Clipboard and selection inside the pane.
        KeyBinding::new("cmd-c", browser_actions::Copy, Some(context::FILE_PANE)),
        KeyBinding::new("ctrl-c", browser_actions::Copy, Some(context::FILE_PANE)),
        KeyBinding::new("cmd-x", browser_actions::Cut, Some(context::FILE_PANE)),
        KeyBinding::new("ctrl-x", browser_actions::Cut, Some(context::FILE_PANE)),
        KeyBinding::new("cmd-v", browser_actions::Paste, Some(context::FILE_PANE)),
        KeyBinding::new("ctrl-v", browser_actions::Paste, Some(context::FILE_PANE)),
        KeyBinding::new(
            "cmd-a",
            browser_actions::SelectAll,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "ctrl-a",
            browser_actions::SelectAll,
            Some(context::FILE_PANE),
        ),
        // Arrow-key selection in icon/column view (the list view's table
        // context binds its own, deeper, and wins there).
        KeyBinding::new("up", browser_actions::CursorUp, Some(context::FILE_PANE)),
        KeyBinding::new(
            "down",
            browser_actions::CursorDown,
            Some(context::FILE_PANE),
        ),
        // Shift extends the range from the anchor. The table binds plain
        // arrows in its own deeper context but not these, so they reach the
        // pane in every view.
        KeyBinding::new(
            "shift-up",
            browser_actions::ExtendUp,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "shift-down",
            browser_actions::ExtendDown,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "left",
            browser_actions::CursorLeft,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "right",
            browser_actions::CursorRight,
            Some(context::FILE_PANE),
        ),
        // Create.
        KeyBinding::new(
            "cmd-shift-n",
            browser_actions::NewFolder,
            Some(context::FILE_PANE),
        ),
        KeyBinding::new(
            "ctrl-shift-n",
            browser_actions::NewFolder,
            Some(context::FILE_PANE),
        ),
    ]);
}

/// Apply the configured theme before the first frame so nothing flashes.
fn apply_startup_theme(cx: &mut App) {
    use ferret_settings::ThemeChoice;
    use gpui_component::{Theme, ThemeMode};

    // Ferret's palette first: `sync_system_appearance` and `change` both
    // re-apply whichever theme config is registered, so registering after
    // them would leave the library's colours on screen until the next toggle.
    ferret_ui::register_theme(vibrancy(cx), cx);

    match cx.global::<FerretState>().settings.appearance.theme {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// Whether to draw the window translucent over a blurred desktop.
///
/// Requested in settings *and* supported by the platform. Blur is a macOS
/// enhancement (D-19): Linux leaves it to the compositor and Windows gates it
/// on the release, and on both a window that asked for it and did not get it
/// composites its translucent palette against nothing.
///
/// **This currently produces nothing on macOS**, measured against the pinned
/// GPUI revision: the appearance reaches `WindowOptions`, and re-applying it
/// after the window opens changes nothing, and `Transparent` behaves the same
/// as `Blurred` — a surface drawn at 55% alpha over a bright backdrop comes
/// out at exactly the value it would have composited against black. So the
/// window layer stays opaque somewhere below us. `gpui-component` sets
/// `Transparent` only under `cfg(target_os = "linux")`, so the macOS path is
/// not one the stack exercises. Left wired because the call sites are the
/// ones the platform documents; until it works, turning the setting on only
/// darkens the palette, which is why it is off by default.
fn vibrancy(cx: &App) -> bool {
    cfg!(target_os = "macos") && cx.global::<FerretState>().settings.appearance.vibrancy
}

fn open_main_window(cx: &mut App) {
    // `TitleBar::window_options()` is the required base for a window that
    // draws its own title bar: it makes the title bar own dragging and
    // double-click instead of the OS.
    let mut base = gpui_component::TitleBar::window_options();

    // The traffic lights are placed once, at window creation, from a fixed
    // offset the library tunes for its own 34 px title bar. Ferret's bar is
    // taller, so left alone the lights sit high against everything drawn in
    // it. Centre them against the height actually used.
    let titlebar = gpui::TitlebarOptions {
        traffic_light_position: Some(gpui::point(
            px(9.),
            px((ferret_ui::TITLE_BAR_HEIGHT - ferret_ui::TRAFFIC_LIGHT_DIAMETER) / 2.),
        )),
        ..base.titlebar.take().unwrap_or_default()
    };

    // The palette carries the alpha; this is what puts something behind it to
    // show through. Opaque unless both the setting and the platform agree, so
    // the two can never disagree about which palette is live.
    let background = if vibrancy(cx) {
        gpui::WindowBackgroundAppearance::Blurred
    } else {
        gpui::WindowBackgroundAppearance::Opaque
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some("dev.ferret.Ferret".into()),
        titlebar: Some(titlebar),
        window_background: background,
        ..base
    };

    // Opening from a spawned task lets gpui-component finish loading its
    // theme and assets before the first frame is built.
    cx.spawn(async move |cx| {
        let window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| RootView::new(window, cx));
                // The first entity in the window must be a `Root`; it hosts
                // the layers that dialogs, popovers, notifications and
                // tooltips render into.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open the Ferret window");

        // Asked for a second time, after the renderer exists. This does not
        // currently change anything — see `vibrancy` — but it is the call the
        // platform documents, and it is where a fix would land.
        let _ = window.update(cx, |_, window, _| {
            window.set_background_appearance(background);
        });

        // Must follow `open_window`: activating before the window exists
        // brings a windowless app to the front and leaves Ferret behind
        // whatever was focused.
        cx.update(|cx| cx.activate(true));
    })
    .detach();
}
