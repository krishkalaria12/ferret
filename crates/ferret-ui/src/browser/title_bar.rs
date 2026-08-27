//! The title bar: the window's own chrome, the navigation and view
//! buttons, and the menus hung off them.

use super::*;

/// The rounded housing both segmented groups sit in.
fn segmented_group(theme: &gpui_component::Theme) -> gpui::Div {
    h_flex()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(8.))
        .bg(theme.secondary.opacity(0.7))
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
        let view_group = segmented_group(theme)
            .child(
                segment_button(
                    "view-list",
                    toolbar_icon::VIEW_LIST,
                    "List",
                    mode == ViewMode::List,
                )
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.dispatch_action(&actions::ViewAsList);
                })),
            )
            .child(
                segment_button(
                    "view-columns",
                    toolbar_icon::VIEW_COLUMNS,
                    "Columns",
                    mode == ViewMode::Columns,
                )
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.dispatch_action(&actions::ViewAsColumns);
                })),
            )
            .child(
                segment_button(
                    "view-icons",
                    toolbar_icon::VIEW_GRID,
                    "Icons",
                    mode == ViewMode::Icons,
                )
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.dispatch_action(&actions::ViewAsIcons);
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
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .when(self.sidebar_visible, |el| {
                            el.w(px((self.sidebar_width + SIDEBAR_DIVIDER_WIDTH
                                - TITLE_BAR_LEFT_INSET)
                                .max(0.)))
                        })
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
                        .with_size(px(TOOLBAR_BUTTON_SIZE))
                        .icon(Icon::new(gpui_component::IconName::EllipsisVertical))
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
