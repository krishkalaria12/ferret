use ferret_ui::RootView;
use gpui::{
    App, AppContext as _, KeyBinding, Menu, MenuItem, QuitMode, WindowBounds, WindowOptions,
    actions, px, size,
};
use gpui_component::{Root, TitleBar};
use gpui_component_assets::Assets;

actions!(ferret, [Quit]);

fn main() {
    // `Assets` supplies the SVGs behind `IconName`; without it every icon is blank.
    gpui_platform::application()
        .with_assets(Assets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            // Must run before any gpui-component type is constructed.
            gpui_component::init(cx);

            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            cx.set_menus([Menu::new("Ferret").items([MenuItem::action("Quit Ferret", Quit)])]);

            open_main_window(cx);
        });
}

fn open_main_window(cx: &mut App) {
    // `TitleBar::window_options()` is the required base for a window that draws its own
    // title bar: it makes the title bar own dragging and double-click instead of the OS.
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        app_id: Some("dev.ferret.Ferret".into()),
        ..TitleBar::window_options()
    };

    // Opening from a spawned task lets gpui-component finish loading its theme and
    // assets before the first frame is built.
    cx.spawn(async move |cx| {
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| RootView::new(window, cx));
            // The first entity in the window must be a `Root`; it hosts the layers
            // that dialogs, popovers, notifications and tooltips render into.
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("failed to open the Ferret window");

        // Must follow `open_window`: activating before the window exists brings a
        // windowless app to the front and leaves Ferret behind whatever was focused.
        cx.update(|cx| cx.activate(true));
    })
    .detach();
}
