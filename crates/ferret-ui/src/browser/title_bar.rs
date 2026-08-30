//! The title bar: the window's own chrome, the navigation and view
//! buttons, and the menus hung off them.

use super::*;

/// The rounded housing every toolbar group sits in.
fn segmented_group(theme: &gpui_component::Theme) -> gpui::Div {
    holds_pointer(
        h_flex()
            .flex_shrink_0()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(theme.secondary.opacity(0.7)),
    )
}

/// Swallow the press so the title bar's own window gestures don't fire under
/// a control.
///
/// `TitleBar` puts a window drag and a double-click-to-zoom on its whole
/// width, and `gpui-component`'s `Button` doesn't stop propagation — so
/// double-clicking a toolbar button zooms the window, and nudging the pointer
/// mid-press drags it. Mouse events bubble innermost-first, so the button has
/// already had the press by the time this runs; only the bar behind it loses
/// it, which is the point.
fn holds_pointer(el: gpui::Div) -> gpui::Div {
    el.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

impl Browser {
    pub(super) fn render_title_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tab = self.active();
        let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
        let back_enabled = !tab.back.is_empty();
        let forward_enabled = !tab.forward.is_empty();
        let show_hidden = cx.global::<FerretState>().settings.browser.show_hidden;
        let mode = tab.pane.read(cx).view_mode();
        let dark = gpui_component::Theme::global(cx).mode.is_dark();

        // Which view you are in was never shown — the mode was read and
        // discarded. The reference fills the active segment in the accent
        // colour, which is the only thing making the group a control rather
        // than three loose buttons.
        //
        // The segments call `set_view_mode` outright rather than dispatching
        // the action they mirror: `App::dispatch_action` re-enters the active
        // window, which is already borrowed inside a click handler, and it
        // logs that failure instead of surfacing it — so the buttons did
        // nothing at all. The keybindings still route through the actions.
        let view_group = segmented_group(theme)
            .child(
                segment_button(
                    "view-list",
                    toolbar_icon::VIEW_LIST,
                    "List",
                    mode == ViewMode::List,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.set_view_mode(ViewMode::List, window, cx);
                })),
            )
            .child(
                segment_button(
                    "view-columns",
                    toolbar_icon::VIEW_COLUMNS,
                    "Columns",
                    mode == ViewMode::Columns,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.set_view_mode(ViewMode::Columns, window, cx);
                })),
            )
            .child(
                segment_button(
                    "view-icons",
                    toolbar_icon::VIEW_GRID,
                    "Icons",
                    mode == ViewMode::Icons,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.set_view_mode(ViewMode::Icons, window, cx);
                })),
            );

        // Back / forward / up read as one control, so they are housed as one.
        let nav_group = segmented_group(theme)
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
            );

        // Everything that acts on the open folder, housed the same way the
        // nav and view groups are — six loose glyphs floating at the right
        // edge read as debris rather than as a toolbar.
        let action_group = segmented_group(theme)
            .child(
                Button::new("power-search-toggle")
                    .ghost()
                    .with_size(px(TOOLBAR_BUTTON_SIZE))
                    .icon(Icon::new(gpui_component::IconName::Search))
                    .tooltip("Power Search")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.toggle_power_search(window, cx)),
                    ),
            )
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
                .selected(show_hidden)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
            )
            .child(
                tool_button("refresh", toolbar_icon::REFRESH, "Refresh")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                Button::new("terminal-toggle")
                    .with_size(px(TOOLBAR_BUTTON_SIZE))
                    .icon(Icon::new(gpui_component::IconName::SquareTerminal))
                    .tooltip("Toggle Terminal")
                    .when(self.terminal_visible, |button| button.primary())
                    .when(!self.terminal_visible, |button| button.ghost())
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(window, cx))),
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
            // A hairline before the overflow menu: it opens a list rather
            // than doing something, so it is the one member of the group
            // that is not a peer of the rest.
            .child(
                div()
                    .w(px(1.))
                    .h(px(16.))
                    .mx(px(3.))
                    .flex_shrink_0()
                    .bg(theme.border),
            )
            .child(
                Button::new("more-menu")
                    .ghost()
                    .with_size(px(TOOLBAR_BUTTON_SIZE))
                    .icon(Icon::new(gpui_component::IconName::EllipsisVertical))
                    .dropdown_menu(|menu, _, _| {
                        menu.menu("Select All", Box::new(actions::SelectAll))
                            .menu("Paste", Box::new(actions::Paste))
                            .separator()
                            .menu("Open in System Terminal", Box::new(actions::OpenInTerminal))
                            .separator()
                            .menu("Show Hidden Files", Box::new(actions::ToggleHidden))
                            .separator()
                            .menu("Add Folder to Favorites", Box::new(actions::AddToFavorites))
                    }),
            );

        TitleBar::new().h(px(super::TITLE_BAR_HEIGHT)).child(
            h_flex()
                .w_full()
                .items_center()
                .gap_1()
                .px_1()
                // The sidebar toggle governs the sidebar, so it sits over it —
                // and takes up exactly that column, so the nav group after it
                // starts on the content's left edge rather than floating
                // somewhere inside the sidebar. `TitleBar` has already inset
                // us past the window controls, so that much comes off.
                //
                // The column tracks the sidebar's live width, not its target,
                // so the nav group slides with the panel instead of jumping to
                // its final place on the first frame of the animation. It
                // never shrinks below the button, though: a narrow sidebar —
                // or a shut one — otherwise gives the column less room than
                // the toggle needs, and the nav group is laid out on top of
                // it, where it swallows the presses meant for the toggle.
                .child(
                    holds_pointer(h_flex())
                        .flex_shrink_0()
                        .w(px((self.sidebar_reveal_width() + SIDEBAR_DIVIDER_WIDTH
                            - TITLE_BAR_LEFT_INSET)
                            .max(TOOLBAR_BUTTON_SIZE)))
                        .child(
                            tool_button("sidebar-toggle", toolbar_icon::SIDEBAR, "Toggle Sidebar")
                                .selected(self.sidebar_visible)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx))),
                        ),
                )
                .child(nav_group)
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
                .child(action_group),
        )
    }
}

/// One segment of a segmented group.
///
/// The active one is filled in the accent colour, the way the reference does
/// it. `.selected()` on a ghost button is the library's own idea of active,
/// but against the group's housing it paints nothing you can see — so the
/// variant is swapped outright instead.
fn segment_button(id: &'static str, icon_path: &str, tooltip: &str, active: bool) -> Button {
    let button = Button::new(id)
        .with_size(px(TOOLBAR_BUTTON_SIZE))
        .icon(Icon::default().path(icon_path))
        .tooltip(tooltip);
    if active {
        button.primary()
    } else {
        button.ghost()
    }
}

fn tool_button(id: &'static str, icon_path: &str, tooltip: &str) -> Button {
    Button::new(id)
        .ghost()
        .with_size(px(TOOLBAR_BUTTON_SIZE))
        .icon(Icon::default().path(icon_path))
        .tooltip(tooltip)
}
