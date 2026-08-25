use ferret_ui::RootView;
use gpui::{
    App, Bounds, KeyBinding, Menu, MenuItem, QuitMode, TitlebarOptions, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, actions, prelude::*, px, size,
};
use gpui_platform::application;

actions!(ferret, [Quit]);

fn main() {
    application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(|cx: &mut App| {
            cx.on_action(|_: &Quit, cx| cx.quit());
            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);
            cx.set_menus([Menu::new("Ferret").items([MenuItem::action("Quit Ferret", Quit)])]);

            open_main_window(cx);
            cx.activate(true);
        });
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1180.), px(760.)), cx);

    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(760.), px(480.))),
            window_background: WindowBackgroundAppearance::Blurred,
            app_id: Some("dev.ferret.Ferret".into()),
            titlebar: Some(TitlebarOptions {
                title: Some("Ferret".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| {
            window.set_background_appearance(WindowBackgroundAppearance::Blurred);
            cx.new(|cx| RootView::new(window, cx))
        },
    )
    .expect("failed to open the Ferret window");
}
