use ferret_ui::actions as browser_actions;
use ferret_ui::{Assets, FerretState, RootView};
use gpui::{
    App, AppContext as _, KeyBinding, QuitMode, WindowBounds, WindowOptions, actions, px, size,
};
use gpui_component::Root;

actions!(ferret, [Quit]);

fn main() {
    // Names the main thread in the profiler timeline. Compiles to nothing
    // without `--features profile`; see that feature's comment in Cargo.toml.
    #[cfg(feature = "profile")]
    profiling::register_thread!("main");

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

            // The thumbnail cache is bounded off the render path: a sweep on
            // startup, then daily. Eviction failures stay silent — a cache
            // that cannot be read stays big, which is not an error the user
            // can act on.
            {
                use ferret_ui::THUMB_EVICT_INTERVAL;
                let thumbs = cx.global::<FerretState>().thumb_cache.clone();
                let exec = cx.background_executor().clone();
                exec.clone()
                    .spawn(async move {
                        thumbs.evict();
                        loop {
                            exec.timer(THUMB_EVICT_INTERVAL).await;
                            thumbs.evict();
                        }
                    })
                    .detach();
            }

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

    // Config writes are debounced (A-09), so quitting inside the debounce
    // window would lose the last sort click. This is the flush, and the only
    // place a config write is allowed to block.
    cx.on_app_quit(|cx: &mut App| {
        ferret_ui::flush_pending_saves(cx);
        async {}
    })
    .detach();
}

fn bind_keys(cx: &mut App) {
    use browser_actions::context;

    // `docs/terminal.html` §9: inside a focused terminal, keys belong to the
    // terminal. Only the intercept list below survives there; every other
    // application binding is scoped out of the terminal with this, so Ctrl-R
    // reaches shell history rather than refreshing the file pane, Ctrl-L
    // clears the screen, Ctrl-W deletes a word, and Ctrl-C stays SIGINT.
    //
    // `Some(context::…)` bindings need no such guard — a context that is not
    // the terminal's cannot be active while the terminal has focus.
    const OUTSIDE_TERMINAL: Option<&str> = Some("!Terminal");

    cx.bind_keys([
        // Quit — both platforms, always. Ctrl-Q is XON/XOFF flow control in a
        // terminal, so it stops being a quit key while one has focus.
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("ctrl-q", Quit, OUTSIDE_TERMINAL),
        // History and hierarchy. Alt is Meta to a shell, so the Alt pairs are
        // scoped out alongside the Ctrl ones.
        KeyBinding::new("cmd-[", browser_actions::Back, None),
        KeyBinding::new("alt-left", browser_actions::Back, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-]", browser_actions::Forward, None),
        KeyBinding::new("alt-right", browser_actions::Forward, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-up", browser_actions::Up, None),
        KeyBinding::new("alt-up", browser_actions::Up, OUTSIDE_TERMINAL),
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
        // Refresh. Ctrl-R is reverse history search in a shell.
        KeyBinding::new("cmd-r", browser_actions::Refresh, None),
        KeyBinding::new("f5", browser_actions::Refresh, None),
        // Power Search. Cmd/Ctrl-Shift-F mirrors editors without stealing the
        // file pane's lightweight in-folder filter.
        KeyBinding::new("cmd-shift-f", browser_actions::TogglePowerSearch, None),
        KeyBinding::new(
            "ctrl-shift-f",
            browser_actions::TogglePowerSearch,
            OUTSIDE_TERMINAL,
        ),
        // Hidden files.
        // The action was declared and handled but never bound, so the sidebar
        // could only be toggled from the toolbar button.
        KeyBinding::new("cmd-b", browser_actions::ToggleSidebar, None),
        KeyBinding::new("ctrl-b", browser_actions::ToggleSidebar, OUTSIDE_TERMINAL),
        // The two panel bindings from §9's intercept list, and the only ones
        // that stay global: they are how you get back out of a focused
        // terminal in the first place.
        KeyBinding::new("cmd-j", browser_actions::ToggleTerminal, None),
        KeyBinding::new("ctrl-j", browser_actions::ToggleTerminal, None),
        KeyBinding::new("ctrl-`", browser_actions::FocusTerminalOrPane, None),
        KeyBinding::new("cmd-shift-.", browser_actions::ToggleHidden, None),
        KeyBinding::new("ctrl-h", browser_actions::ToggleHidden, OUTSIDE_TERMINAL),
        // Commit (F-52). Not Cmd-K: that is "clear" in every terminal, and
        // this window has one docked in it.
        KeyBinding::new("cmd-shift-c", browser_actions::CommitChanges, None),
        KeyBinding::new(
            "ctrl-shift-c",
            browser_actions::CommitChanges,
            OUTSIDE_TERMINAL,
        ),
        // Sync toggle, mirroring the panel's header control.
        KeyBinding::new("cmd-shift-s", browser_actions::ToggleTerminalSync, None),
        KeyBinding::new("ctrl-shift-s", browser_actions::ToggleTerminalSync, None),
        // Terminal clipboard. Plain Ctrl-V is a literal-next in readline and
        // Ctrl-C is SIGINT, which is why Windows uses the Shift forms.
        KeyBinding::new(
            "cmd-v",
            browser_actions::TerminalPaste,
            Some(context::TERMINAL),
        ),
        KeyBinding::new(
            "ctrl-shift-v",
            browser_actions::TerminalPaste,
            Some(context::TERMINAL),
        ),
        // Preview pane (F-30). The toggle stays global like the terminal's:
        // it is how you get the pane back when it has focus nowhere else.
        KeyBinding::new("cmd-alt-p", browser_actions::TogglePreview, None),
        KeyBinding::new(
            "ctrl-alt-p",
            browser_actions::TogglePreview,
            OUTSIDE_TERMINAL,
        ),
        KeyBinding::new("cmd-alt-right", browser_actions::FocusPreview, None),
        KeyBinding::new(
            "ctrl-alt-right",
            browser_actions::FocusPreview,
            OUTSIDE_TERMINAL,
        ),
        // `Esc` returns focus to the list, but only from inside the preview —
        // elsewhere it keeps whatever meaning it already had.
        KeyBinding::new(
            "escape",
            browser_actions::FocusFilePane,
            Some(context::PREVIEW),
        ),
        // Image/PDF zoom. Cmd pairs stay global like the terminal toggle;
        // Ctrl pairs stay out of the terminal, where they belong to the shell.
        KeyBinding::new("cmd-=", browser_actions::PreviewZoomIn, None),
        KeyBinding::new("ctrl-=", browser_actions::PreviewZoomIn, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd--", browser_actions::PreviewZoomOut, None),
        KeyBinding::new("ctrl--", browser_actions::PreviewZoomOut, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-0", browser_actions::PreviewZoomReset, None),
        KeyBinding::new(
            "ctrl-0",
            browser_actions::PreviewZoomReset,
            OUTSIDE_TERMINAL,
        ),
        // View modes.
        KeyBinding::new("cmd-1", browser_actions::ViewAsList, None),
        KeyBinding::new("ctrl-1", browser_actions::ViewAsList, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-2", browser_actions::ViewAsColumns, None),
        KeyBinding::new("ctrl-2", browser_actions::ViewAsColumns, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-3", browser_actions::ViewAsIcons, None),
        KeyBinding::new("ctrl-3", browser_actions::ViewAsIcons, OUTSIDE_TERMINAL),
        // Windows.
        KeyBinding::new("cmd-n", browser_actions::NewWindow, None),
        KeyBinding::new("ctrl-n", browser_actions::NewWindow, OUTSIDE_TERMINAL),
        // Tabs. Ctrl-W deletes a word in a shell; Ctrl-T transposes characters.
        KeyBinding::new("cmd-t", browser_actions::NewTab, None),
        KeyBinding::new("ctrl-t", browser_actions::NewTab, OUTSIDE_TERMINAL),
        KeyBinding::new("cmd-w", browser_actions::CloseTab, None),
        KeyBinding::new("ctrl-w", browser_actions::CloseTab, OUTSIDE_TERMINAL),
        // Path bar. Ctrl-L clears the screen.
        KeyBinding::new("cmd-l", browser_actions::FocusPathBar, None),
        KeyBinding::new("ctrl-l", browser_actions::FocusPathBar, OUTSIDE_TERMINAL),
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
        // Inline filter. Ctrl-F moves the cursor forward in a shell.
        KeyBinding::new("cmd-f", browser_actions::FocusFilter, None),
        KeyBinding::new("ctrl-f", browser_actions::FocusFilter, OUTSIDE_TERMINAL),
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
    let font_size = cx.global::<FerretState>().settings.appearance.ui_font_size;
    ferret_ui::register_theme(vibrancy(cx), font_size, cx);

    match cx.global::<FerretState>().settings.appearance.theme {
        ThemeChoice::System => Theme::sync_system_appearance(None, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, None, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}

/// Keep the window's theme in step with the OS while Ferret runs (A-31).
///
/// `apply_startup_theme` reads the appearance once, which is not what
/// `theme: "system"` promises — and "system" is the default, so before this
/// every user on macOS's own Auto appearance had a window that was the wrong
/// colour for half of each day.
///
/// Guarded on the setting rather than always following: someone who has
/// explicitly chosen Light or Dark has said they do not want the OS deciding,
/// and that has to survive sunset. The check is inside the callback, not
/// outside it, so changing the setting at runtime takes effect without
/// re-registering anything.
fn follow_system_appearance(window: &mut gpui::Window, _cx: &mut App) -> gpui::Subscription {
    use ferret_settings::ThemeChoice;
    use gpui_component::Theme;

    window.observe_window_appearance(|window, cx| {
        let follows = matches!(
            cx.global::<FerretState>().settings.appearance.theme,
            ThemeChoice::System
        );
        if follows {
            // `Some(window)` rather than `None`: the library reads the
            // window's own appearance when it has one, which is the value
            // that just changed.
            Theme::sync_system_appearance(Some(window), cx);
        }
    })
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

        // Detached rather than held: the observer lives in the window's own
        // subscriber set, so it is dropped when the window is. Holding the
        // handle here would only add a second thing to keep alive for exactly
        // as long.
        let _ = window.update(cx, |_, window, cx| {
            follow_system_appearance(window, cx).detach();
        });

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

#[cfg(test)]
mod tests {
    use gpui::{KeyBinding, KeyBindingContextPredicate, KeyContext};

    /// `KeyBinding::new` panics on a predicate it cannot parse, and every one
    /// here runs at startup — a typo in `!Terminal` would be a launch crash
    /// rather than a wrong keystroke.
    #[test]
    fn the_terminal_guard_parses_and_excludes_only_the_terminal() {
        let predicate = KeyBindingContextPredicate::parse("!Terminal")
            .expect("the guard on every application binding must parse");

        let context = |name| vec![KeyContext::try_from(name).expect("a bare context name parses")];

        assert!(
            predicate.depth_of(&context("Terminal")).is_none(),
            "Ctrl-R and friends must reach the shell"
        );
        assert!(
            predicate.depth_of(&context("FilePane")).is_some(),
            "and must still work everywhere else"
        );
    }

    /// `KeyBinding::new` panics on a keystroke string it cannot parse, and
    /// every binding here runs at startup — a typo would be a launch crash
    /// rather than a wrong keystroke. The preview bindings use modifier
    /// combinations (`cmd-alt-p`) no other binding exercises, so they get
    /// their own construction test.
    #[test]
    fn the_preview_bindings_parse() {
        use ferret_ui::actions as browser_actions;

        // Construction is the assertion: an unparseable keystroke panics.
        let _ = KeyBinding::new("cmd-alt-p", browser_actions::TogglePreview, None);
        let _ = KeyBinding::new(
            "ctrl-alt-p",
            browser_actions::TogglePreview,
            Some("!Terminal"),
        );
        let _ = KeyBinding::new("cmd-alt-right", browser_actions::FocusPreview, None);
        let _ = KeyBinding::new(
            "ctrl-alt-right",
            browser_actions::FocusPreview,
            Some("!Terminal"),
        );
        let _ = KeyBinding::new(
            "escape",
            browser_actions::FocusFilePane,
            Some(browser_actions::context::PREVIEW),
        );
        let _ = KeyBinding::new("cmd-=", browser_actions::PreviewZoomIn, None);
        let _ = KeyBinding::new("cmd--", browser_actions::PreviewZoomOut, None);
        let _ = KeyBinding::new("cmd-0", browser_actions::PreviewZoomReset, None);

        // And the Preview context itself parses, or the `Esc` binding above
        // is scoped to a context that never matches.
        let preview = vec![
            KeyContext::try_from(browser_actions::context::PREVIEW)
                .expect("the Preview context must parse"),
        ];
        let predicate = KeyBindingContextPredicate::parse(browser_actions::context::PREVIEW)
            .expect("the Preview predicate must parse");
        assert!(predicate.depth_of(&preview).is_some());
    }
}
