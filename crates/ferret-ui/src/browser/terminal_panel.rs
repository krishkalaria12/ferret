//! The docked terminal panel and its bridge to `ferret-terminal`.

use super::*;

const MIN_TERMINAL_HEIGHT: f32 = ferret_settings::config::MIN_TERMINAL_HEIGHT;
const MAX_TERMINAL_HEIGHT: f32 = ferret_settings::config::MAX_TERMINAL_HEIGHT;
const TERMINAL_HEADER_HEIGHT: f32 = 26.;
const TERMINAL_SPLITTER_HEIGHT: f32 = 5.;

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
        let settings = &cx.global::<FerretState>().settings.terminal;
        let options = TerminalOptions {
            shell: settings.shell.clone(),
            term: settings.term.clone(),
            scrollback_lines: settings.scrollback_lines,
        };
        match TerminalSession::spawn(local, &options) {
            Ok(session) => {
                self.terminal_error = None;
                if let Err(error) = session.resize(self.terminal_columns, self.terminal_rows, 0, 0)
                {
                    self.terminal_error = Some(error.to_string());
                }
                self.terminal_session = Some(session);
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
        let terminal_settings = &cx.global::<FerretState>().settings.terminal;
        let font_family = terminal_settings
            .font_family
            .clone()
            .unwrap_or_else(|| default_terminal_font(cx));
        let font_size = terminal_settings.font_size;
        let line_height = font_size * terminal_settings.line_height;
        let terminal_owner = cx.weak_entity();

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
                    .on_children_prepainted(
                        move |children: Vec<gpui::Bounds<gpui::Pixels>>, _, cx| {
                            let Some(bounds) = children.first() else {
                                return;
                            };
                            // A monospace advance is approximately 0.6 em.
                            // The run renderer will replace this estimate with
                            // shaped metrics; the PTY must still resize now.
                            let cell_width = (font_size * 0.6).max(1.);
                            let columns = ((f32::from(bounds.size.width) - 24.).max(cell_width)
                                / cell_width) as u16;
                            let rows = ((f32::from(bounds.size.height) - 16.).max(line_height)
                                / line_height) as u16;
                            if let Some(owner) = terminal_owner.upgrade() {
                                owner.update(cx, |this, cx| {
                                    this.resize_terminal_grid(columns.max(2), rows.max(1), cx);
                                });
                            }
                        },
                    )
                    .id("terminal-body")
                    .relative()
                    .key_context(actions::context::TERMINAL)
                    .track_focus(&self.terminal_focus)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            window.focus(&this.terminal_focus, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                        this.handle_terminal_key(event, cx);
                    }))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px(px(12.))
                    .py(px(8.))
                    .bg(palette.background)
                    .text_color(palette.foreground)
                    .text_size(px(font_size))
                    .line_height(px(line_height))
                    .when(focused, |el| {
                        el.border_1().border_color(palette.cursor.opacity(0.55))
                    })
                    .child(self.render_terminal_content(cwd, palette, font_family, line_height)),
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
        font_family: String,
        line_height: f32,
    ) -> impl IntoElement {
        let snapshot = self
            .terminal_session
            .as_ref()
            .map(TerminalSession::snapshot);
        let mut body = v_flex().size_full().font_family(font_family);
        if let Some(error) = &self.terminal_error {
            return body.child(
                div()
                    .text_color(palette.bright_red)
                    .child(SharedString::from(error.clone())),
            );
        }
        if let Some(snapshot) = snapshot {
            for line in snapshot.lines {
                body = body.child(div().h(px(line_height)).whitespace_nowrap().child(
                    SharedString::from(if line.is_empty() { " " } else { &line }),
                ));
            }
            return body;
        }
        body.child(
            div()
                .text_color(palette.bright_black)
                .child(SharedString::from(format!("Starting terminal in {cwd}…"))),
        )
    }

    fn handle_terminal_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(bytes) = terminal_key_bytes(event) else {
            return;
        };
        let Some(session) = &self.terminal_session else {
            return;
        };
        if let Err(error) = session.write(&bytes) {
            self.terminal_error = Some(error.to_string());
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn resize_terminal_grid(&mut self, columns: u16, rows: u16, cx: &mut Context<Self>) {
        if columns == self.terminal_columns && rows == self.terminal_rows {
            return;
        }
        self.terminal_columns = columns;
        self.terminal_rows = rows;
        let Some(session) = &self.terminal_session else {
            return;
        };
        if let Err(error) = session.resize(columns, rows, 0, 0) {
            self.terminal_error = Some(error.to_string());
        }
        cx.notify();
    }
}

fn terminal_key_bytes(event: &gpui::KeyDownEvent) -> Option<Vec<u8>> {
    let key = event.keystroke.key.as_str();
    let modifiers = event.keystroke.modifiers;

    // Command/Windows-key combinations remain application shortcuts.
    if modifiers.platform {
        return None;
    }

    let mut bytes = Vec::new();
    if modifiers.alt {
        bytes.push(0x1b);
    }

    if modifiers.control && key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii() {
            bytes.push(byte.to_ascii_uppercase() & 0x1f);
            return Some(bytes);
        }
    }

    let sequence = match key {
        "enter" => "\r",
        "backspace" => "\x7f",
        "tab" => "\t",
        "escape" => "\x1b",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "delete" => "\x1b[3~",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        _ => {
            let text = event
                .keystroke
                .key_char
                .as_deref()
                .or_else(|| (key.chars().count() == 1).then_some(key))?;
            bytes.extend_from_slice(text.as_bytes());
            return Some(bytes);
        }
    };
    bytes.extend_from_slice(sequence.as_bytes());
    Some(bytes)
}

fn default_terminal_font(cx: &App) -> String {
    let installed = cx.text_system().all_font_names();
    let first_installed = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|candidate| installed.iter().any(|font| font == **candidate))
            .map(|candidate| (*candidate).to_owned())
    };
    #[cfg(target_os = "macos")]
    {
        // Prefer a Nerd Font when present so configured prompts retain their
        // private-use glyphs, as they do in Ghostty. Menlo remains the clean
        // system fallback required by F-12.
        first_installed(&["JetBrainsMono Nerd Font Mono", "MesloLGS NF", "Menlo"])
            .unwrap_or_else(|| "monospace".into())
    }
    #[cfg(target_os = "windows")]
    {
        first_installed(&["CaskaydiaMono Nerd Font", "Cascadia Mono", "Consolas"])
            .unwrap_or_else(|| "monospace".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "monospace".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, character: Option<&str>, modifiers: gpui::Modifiers) -> gpui::KeyDownEvent {
        gpui::KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers,
                key: name.into(),
                key_char: character.map(str::to_owned),
            },
            is_held: false,
            prefer_character_input: false,
        }
    }

    #[test]
    fn printable_and_navigation_keys_are_encoded_for_the_pty() {
        assert_eq!(
            terminal_key_bytes(&key("a", Some("a"), gpui::Modifiers::default())),
            Some(b"a".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(&key("enter", None, gpui::Modifiers::default())),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(&key("left", None, gpui::Modifiers::default())),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn control_c_reaches_the_shell_but_command_shortcuts_do_not() {
        assert_eq!(
            terminal_key_bytes(&key(
                "c",
                None,
                gpui::Modifiers {
                    control: true,
                    ..Default::default()
                },
            )),
            Some(vec![3])
        );
        assert_eq!(
            terminal_key_bytes(&key(
                "c",
                None,
                gpui::Modifiers {
                    platform: true,
                    ..Default::default()
                },
            )),
            None
        );
    }
}
