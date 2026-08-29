//! The docked terminal panel and its bridge to `ferret-terminal`.

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
            if self.terminal_session.is_none() {
                self.restart_terminal(cx);
            }
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
        if self.terminal_sync.is_enabled() {
            let dir = self.active().folder.read(cx).dir.clone();
            self.sync_terminal_to(&dir, cx);
        }
        cx.notify();
    }

    pub(super) fn restart_terminal(&mut self, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let Some(local) = dir.as_local() else {
            self.terminal_session = None;
            self.terminal_error =
                Some("Terminal sessions currently require a local folder.".into());
            cx.notify();
            return;
        };
        match TerminalSession::spawn(local) {
            Ok(session) => {
                self.terminal_session = Some(session);
                self.terminal_error = None;
                self.terminal_generation = 0;
            }
            Err(error) => {
                self.terminal_session = None;
                self.terminal_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    /// Forward sync is an explicit command injected into the one persistent
    /// shell (F-11). This first slice does not expose keyboard input yet, so
    /// injection cannot overwrite a partially typed command.
    pub(super) fn sync_terminal_to(&mut self, dir: &VfsPath, cx: &mut Context<Self>) {
        if !self.terminal_sync.is_enabled() || self.terminal_session.is_none() {
            return;
        }
        let Some(local) = dir.as_local() else {
            return;
        };
        if let Some(session) = &self.terminal_session
            && let Err(error) = session.change_directory(local)
        {
            self.terminal_error = Some(error.to_string());
            self.terminal_sync = TerminalSyncState::Off;
            cx.notify();
        }
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
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.restart_terminal(cx);
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
                    .child(self.render_terminal_content(cwd, palette)),
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

    fn render_terminal_content(
        &self,
        cwd: SharedString,
        palette: crate::theme::TerminalPalette,
    ) -> impl IntoElement {
        let snapshot = self
            .terminal_session
            .as_ref()
            .map(TerminalSession::snapshot);
        let mut body = v_flex().gap(px(1.)).font_family("monospace");
        if let Some(error) = &self.terminal_error {
            return body.child(
                div()
                    .text_color(palette.bright_red)
                    .child(SharedString::from(error.clone())),
            );
        }
        if let Some(snapshot) = snapshot {
            for line in snapshot.lines {
                body = body.child(
                    div()
                        .h(px(16.))
                        .whitespace_nowrap()
                        .child(SharedString::from(if line.is_empty() {
                            " "
                        } else {
                            &line
                        })),
                );
            }
            return body;
        }
        body.child(
            div()
                .text_color(palette.bright_black)
                .child(SharedString::from(format!("Starting terminal in {cwd}…"))),
        )
    }
}
