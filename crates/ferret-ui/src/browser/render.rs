//! The browser shell: how the regions are stacked, the actions the
//! window binds, and the two strips that sit below the pane.

use super::*;

impl Browser {
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

    /// The pane of the active tab, focused when switched to.
    fn render_active_pane(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> AnyPane {
        let pane = self.active().pane.clone();
        let _ = window;
        AnyPane { pane }
    }
}

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

struct AnyPane {
    pane: Entity<FilePane>,
}

impl IntoElement for AnyPane {
    type Element = <Entity<FilePane> as IntoElement>::Element;

    fn into_element(self) -> Self::Element {
        self.pane.into_element()
    }
}
