//! The docked terminal panel shell.
//!
//! This is the F-10/F-11 UI boundary: chrome, focus, resizing and the current
//! directory follow the active file-pane tab. The PTY/grid engine will replace
//! the body behind this module once `ferret-terminal` lands.

use super::*;

const MIN_TERMINAL_HEIGHT: f32 = ferret_settings::config::MIN_TERMINAL_HEIGHT;
const MAX_TERMINAL_HEIGHT: f32 = ferret_settings::config::MAX_TERMINAL_HEIGHT;
const TERMINAL_HEADER_HEIGHT: f32 = 26.;
const TERMINAL_SPLITTER_HEIGHT: f32 = 5.;
const TERMINAL_FONT_SIZE: f32 = 13.;

impl Browser {
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = !self.terminal_visible;
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.terminal.visible = self.terminal_visible;
            state.save_settings();
        }
        if self.terminal_visible {
            window.focus(&self.terminal_focus, cx);
        } else {
            use gpui::Focusable as _;
            let pane_focus = self.active().pane.read(cx).focus_handle(cx);
            window.focus(&pane_focus, cx);
        }
        cx.notify();
    }

    pub(super) fn focus_terminal_or_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui::Focusable as _;
        if !self.terminal_visible {
            self.terminal_visible = true;
            let state = cx.global_mut::<FerretState>();
            state.settings.terminal.visible = true;
            state.save_settings();
            window.focus(&self.terminal_focus, cx);
            cx.notify();
            return;
        }

        if self.terminal_focus.is_focused(window) {
            let pane_focus = self.active().pane.read(cx).focus_handle(cx);
            window.focus(&pane_focus, cx);
        } else {
            self.cancel_path_edit(cx);
            window.focus(&self.terminal_focus, cx);
        }
        cx.notify();
    }

    pub(super) fn toggle_terminal_sync(&mut self, cx: &mut Context<Self>) {
        self.terminal_sync = self.terminal_sync.toggle();
        cx.notify();
    }

    pub(super) fn update_terminal_drag(&mut self, y: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.terminal_drag else {
            return;
        };
        let delta = f32::from(drag.start_y - y);
        let height = (drag.start_height + delta).clamp(MIN_TERMINAL_HEIGHT, MAX_TERMINAL_HEIGHT);
        if height != self.terminal_height {
            self.terminal_height = height;
            cx.notify();
        }
    }

    pub(super) fn end_terminal_drag(&mut self, cx: &mut Context<Self>) {
        if self.terminal_drag.take().is_none() {
            return;
        }
        let height = self.terminal_height;
        let state = cx.global_mut::<FerretState>();
        state.settings.terminal.height = height;
        state.save_settings();
        cx.notify();
    }

    pub(super) fn render_terminal_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = crate::theme::TerminalPalette::default();
        let theme = cx.theme();
        let border = theme.border;
        let title_bar = theme.title_bar;
        let muted_foreground = theme.muted_foreground;
        let foreground = theme.foreground;
        let dir = self.active().folder.read(cx).dir.clone();
        let cwd = SharedString::from(dir.to_string());
        let focused = self.terminal_focus.is_focused(window);

        v_flex()
            .id("terminal-panel")
            .h(px(self.terminal_height))
            .min_h(px(MIN_TERMINAL_HEIGHT))
            .max_h(px(MAX_TERMINAL_HEIGHT))
            .flex_shrink_0()
            .border_t_1()
            .border_color(border)
            .child(self.render_terminal_splitter(cx))
            .child(
                h_flex()
                    .h(px(TERMINAL_HEADER_HEIGHT))
                    .flex_shrink_0()
                    .items_center()
                    .gap_2()
                    .px(px(10.))
                    .bg(title_bar)
                    .border_b_1()
                    .border_color(border.opacity(0.8))
                    .child(
                        Icon::new(gpui_component::IconName::SquareTerminal)
                            .size_4()
                            .text_color(muted_foreground),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(foreground)
                            .child("Terminal"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(muted_foreground)
                            .child(cwd.clone()),
                    )
                    .child(self.sync_button(cx))
                    .child(
                        Button::new("terminal-new")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_component::IconName::Plus))
                            .tooltip("New Terminal")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_current_in_terminal(window, cx);
                            })),
                    )
                    .child(
                        Button::new("terminal-close")
                            .ghost()
                            .small()
                            .icon(Icon::new(gpui_component::IconName::Close))
                            .tooltip("Close Terminal")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_terminal(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("terminal-body")
                    .relative()
                    .key_context(actions::context::TERMINAL)
                    .track_focus(&self.terminal_focus)
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px(px(12.))
                    .py(px(8.))
                    .bg(palette.background)
                    .text_color(palette.foreground)
                    .text_size(px(TERMINAL_FONT_SIZE))
                    .when(focused, |el| {
                        el.border_1().border_color(palette.cursor.opacity(0.55))
                    })
                    .child(self.render_terminal_preview(cwd, palette)),
            )
    }

    fn render_terminal_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("terminal-splitter")
            .h(px(TERMINAL_SPLITTER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .bg(theme.border.opacity(0.55))
            .hover(|el| el.bg(cx.theme().primary.opacity(0.4)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.terminal_drag = Some(TerminalDrag {
                        start_y: event.position.y,
                        start_height: this.terminal_height,
                    });
                    cx.notify();
                }),
            )
    }

    fn sync_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (label, bg) = match self.terminal_sync {
            TerminalSyncState::On => ("Sync", theme.primary),
            TerminalSyncState::Off => ("Sync Off", theme.secondary),
        };
        Button::new("terminal-sync")
            .small()
            .label(label)
            .bg(bg)
            .text_color(crate::theme::text_on(bg, theme.foreground))
            .tooltip("Follow Folder")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_sync(cx)))
    }

    fn render_terminal_preview(
        &self,
        cwd: SharedString,
        palette: crate::theme::TerminalPalette,
    ) -> impl IntoElement {
        let prompt = SharedString::from(format!("ferret {} % ", cwd));
        v_flex()
            .gap(px(3.))
            .child(
                div()
                    .child("$ cd ")
                    .child(cwd)
                    .text_color(palette.bright_black),
            )
            .child(
                h_flex().items_center().child(div().child(prompt)).child(
                    div()
                        .ml(px(1.))
                        .w(px(7.))
                        .h(px(16.))
                        .bg(palette.cursor.opacity(0.85)),
                ),
            )
    }
}
