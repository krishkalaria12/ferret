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
        // Refresh.
        KeyBinding::new("cmd-r", browser_actions::Refresh, None),
        KeyBinding::new("f5", browser_actions::Refresh, None),
        // Hidden files.
        KeyBinding::new("cmd-shift-.", browser_actions::ToggleHidden, None),
        KeyBinding::new("ctrl-h", browser_actions::ToggleHidden, None),
        // View modes.
        KeyBinding::new("cmd-1", browser_actions::ViewAsList, None),
        KeyBinding::new("ctrl-1", browser_actions::ViewAsList, None),
        KeyBinding::new("cmd-2", browser_actions::ViewAsColumns, None),
        KeyBinding::new("ctrl-2", browser_actions::ViewAsColumns, None),
        KeyBinding::new("cmd-3", browser_actions::ViewAsIcons, None),
        KeyBinding::new("ctrl-3", browser_actions::ViewAsIcons, None),
        // Tabs.
        KeyBinding::new("cmd-t", browser_actions::NewTab, None),
        KeyBinding::new("ctrl-t", browser_actions::NewTab, None),
        KeyBinding::new("cmd-w", browser_actions::CloseTab, None),
        KeyBinding::new("ctrl-w", browser_actions::CloseTab, None),
        // Path bar.
        KeyBinding::new("cmd-l", browser_actions::FocusPathBar, None),
        KeyBinding::new("ctrl-l", browser_actions::FocusPathBar, None),
        KeyBinding::new(
            "escape",
            browser_actions::CancelEdit,
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

    match cx.global::<FerretState>().settings.appearance.theme {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

fn open_main_window(cx: &mut App) {
    // `TitleBar::window_options()` is the required base for a window that
    // draws its own title bar: it makes the title bar own dragging and
    // double-click instead of the OS.
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some("dev.ferret.Ferret".into()),
        ..gpui_component::TitleBar::window_options()
    };

    // Opening from a spawned task lets gpui-component finish loading its
    // theme and assets before the first frame is built.
    cx.spawn(async move |cx| {
        let _window = cx
            .open_window(options, |window, cx| {
                let view = cx.new(|cx| RootView::new(window, cx));
                // The first entity in the window must be a `Root`; it hosts
                // the layers that dialogs, popovers, notifications and
                // tooltips render into.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open the Ferret window");

        // Must follow `open_window`: activating before the window exists
        // brings a windowless app to the front and leaves Ferret behind
        // whatever was focused.
        cx.update(|cx| cx.activate(true));
    })
    .detach();
}
