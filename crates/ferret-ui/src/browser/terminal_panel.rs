//! The docked terminal panel and its bridge to `ferret-terminal`.

use super::*;

const MIN_TERMINAL_HEIGHT: f32 = ferret_settings::config::MIN_TERMINAL_HEIGHT;
const MAX_TERMINAL_HEIGHT: f32 = ferret_settings::config::MAX_TERMINAL_HEIGHT;
const TERMINAL_HEADER_HEIGHT: f32 = 26.;
const TERMINAL_SPLITTER_HEIGHT: f32 = 5.;

/// Repaint cadence while the panel is on screen — one frame at 60 Hz.
const TERMINAL_VISIBLE_TICK: Duration = Duration::from_millis(16);
/// Cadence while the panel is hidden but the session is still alive. Nothing
/// is being drawn, so the only job left is retrying a held `cd`.
const TERMINAL_HIDDEN_TICK: Duration = Duration::from_millis(250);
/// D-26. With no shell integration, silence is the only evidence we have that
/// no program owns the PTY, so a held `cd` waits out a full second of it.
const TERMINAL_SYNC_IDLE: Duration = Duration::from_secs(1);

impl Browser {
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = !self.terminal_visible;
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.terminal.visible = self.terminal_visible;
            state.touch_settings();
        }
        crate::state::arm_config_save(cx);
        if self.terminal_visible {
            self.show_terminal(window, cx);
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
            state.touch_settings();
            crate::state::arm_config_save(cx);
            // Opening the panel has to start a shell here too. The poller
            // cannot create one, so skipping this leaves the panel reading
            // "Starting terminal…" until it is closed and reopened.
            self.show_terminal(window, cx);
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

    /// The one path that makes an opened panel usable: a session if there is
    /// none, focus, and the poll that draws its output.
    fn show_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_session.is_none() {
            self.restart_terminal(cx);
        } else {
            self.start_terminal_poll(cx);
        }
        window.focus(&self.terminal_focus, cx);
    }

    pub(super) fn toggle_terminal_sync(&mut self, cx: &mut Context<Self>) {
        self.terminal_sync = self.terminal_sync.toggle();
        if self.terminal_sync.is_enabled() {
            let dir = self.active().folder.read(cx).dir.clone();
            self.sync_terminal_to(&dir, cx);
        } else {
            self.terminal_pending_sync = None;
        }
        cx.notify();
    }

    pub(super) fn restart_terminal(&mut self, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let Some(local) = dir.as_local() else {
            self.terminal_session = None;
            self.terminal_poll = None;
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
            backend: match settings.backend {
                ferret_settings::TerminalBackend::Auto => TerminalBackend::Auto,
                ferret_settings::TerminalBackend::Ghostty => TerminalBackend::Ghostty,
                ferret_settings::TerminalBackend::Alacritty => TerminalBackend::Alacritty,
            },
        };
        self.terminal_pending_sync = None;
        self.terminal_input_dirty = false;
        self.terminal_last_key = None;
        match TerminalSession::spawn(local, &options) {
            Ok(session) => {
                self.terminal_error = None;
                if let Err(error) = session.resize(self.terminal_columns, self.terminal_rows, 0, 0)
                {
                    self.terminal_error = Some(error.to_string());
                }
                self.terminal_session = Some(session);
                self.terminal_generation = 0;
                // The shell was spawned in this directory, so nothing is owed.
                self.terminal_synced_to = Some(local.to_path_buf());
                self.start_terminal_poll(cx);
            }
            Err(error) => {
                self.terminal_session = None;
                self.terminal_poll = None;
                self.terminal_error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    /// Queue a directory for the shell to follow (F-11).
    ///
    /// The pane has already navigated by the time this runs and never waits on
    /// the result: if the shell cannot take the `cd` right now the newest
    /// target simply sits here until [`Self::flush_terminal_sync`] finds a safe
    /// moment.
    pub(super) fn sync_terminal_to(&mut self, dir: &VfsPath, cx: &mut Context<Self>) {
        if !self.terminal_sync.is_enabled() || self.terminal_session.is_none() {
            return;
        }
        let Some(local) = dir.as_local() else {
            return;
        };
        if self.terminal_synced_to.as_deref() == Some(local) {
            self.terminal_pending_sync = None;
            return;
        }
        self.terminal_pending_sync = Some(local.to_path_buf());
        self.flush_terminal_sync(cx);
    }

    /// Write the queued `cd` if — and only if — every §6 precondition holds.
    ///
    /// In the ordinary case, a shell sitting at a prompt with nothing typed,
    /// all of them pass at once and the `cd` goes out immediately.
    fn flush_terminal_sync(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.terminal_pending_sync.clone() else {
            return;
        };
        if !self.terminal_sync.is_enabled() {
            self.terminal_pending_sync = None;
            return;
        }
        let Some(session) = &self.terminal_session else {
            return;
        };
        // A full-screen program owns the grid and its stdin: this is the check
        // that keeps a `cd` out of `vim`, `less` and `cat > file`.
        if session.snapshot().alternate_screen {
            return;
        }
        // Ferret has no OSC 133 marks yet, so it cannot see the input line
        // directly. What it can see is its own keystrokes, and a newline is
        // what returns the line to empty.
        if self.terminal_input_dirty {
            return;
        }
        let typing_recently = self
            .terminal_last_key
            .is_some_and(|at| at.elapsed() < TERMINAL_SYNC_IDLE);
        if typing_recently || session.idle_for() < TERMINAL_SYNC_IDLE {
            return;
        }
        match session.change_directory(&target) {
            Ok(()) => {
                self.terminal_pending_sync = None;
                self.terminal_synced_to = Some(target);
            }
            Err(error) => {
                // An unquotable path or a dead PTY: dropping the queue stops
                // the poller retrying the same failure forever.
                self.terminal_error = Some(error.to_string());
                self.terminal_pending_sync = None;
                self.terminal_sync = TerminalSyncState::Off;
            }
        }
        cx.notify();
    }

    /// Drive repaints and the held `cd`, for exactly as long as there is a
    /// session to drive.
    ///
    /// Both jobs need a timer and neither exists without a session, so an idle
    /// file-manager window — the common case, with the panel closed and no
    /// shell ever started — schedules nothing at all.
    fn start_terminal_poll(&mut self, cx: &mut Context<Self>) {
        // Always replaces rather than skipping when one is already held:
        // dropping the previous task cancels it, so opening the panel twice
        // cannot leave two pollers ticking against the same session. The
        // loop below never touches this field — cancelling a task from
        // inside itself is exactly the shape that deadlocks.
        self.terminal_poll = Some(cx.spawn(async move |this, cx| {
            let mut interval = TERMINAL_VISIBLE_TICK;
            loop {
                cx.background_executor().timer(interval).await;
                let alive = this
                    .update(cx, |this, cx| {
                        // The session is what there was to poll for. When it
                        // goes, so does the timer; the next one that starts
                        // gets a fresh task from here.
                        if this.terminal_session.is_none() {
                            return false;
                        }
                        this.flush_terminal_sync(cx);
                        interval = if this.terminal_visible {
                            TERMINAL_VISIBLE_TICK
                        } else {
                            TERMINAL_HIDDEN_TICK
                        };
                        if this.terminal_visible {
                            let generation = this
                                .terminal_session
                                .as_ref()
                                .map_or(0, |session| session.snapshot().generation);
                            if generation != this.terminal_generation {
                                this.terminal_generation = generation;
                                cx.notify();
                            }
                        }
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        }));
    }

    /// Paste the clipboard into the shell (§9), rather than encoding the paste
    /// shortcut itself and sending that.
    pub(super) fn paste_into_terminal(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let Some(session) = &self.terminal_session else {
            return;
        };
        if let Err(error) = session.paste(&text) {
            self.terminal_error = Some(error.to_string());
        }
        // Pasted text lands on the input line exactly as typed text would, and
        // a trailing newline submits it.
        self.terminal_input_dirty = !text.ends_with('\n') && !text.ends_with('\r');
        self.terminal_last_key = Some(Instant::now());
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
        state.touch_settings();
        crate::state::arm_config_save(cx);
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
        let font = gpui::font(font_family.clone());
        let measure_run = gpui::TextRun {
            len: 1,
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let cell_width = f32::from(
            window
                .text_system()
                .layout_line("M", px(font_size), &[measure_run], None)
                .width,
        );
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
                            let cell_width = cell_width.max(1.);
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
                    .child(self.render_terminal_content(
                        cwd,
                        palette,
                        font_family,
                        line_height,
                        cell_width,
                        focused,
                    )),
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
        // A held `cd` is not a failure and not "off" — it is the third state
        // §6 asks the control to show, so the user can see the shell is busy
        // rather than assuming sync has silently stopped working.
        let (label, tooltip, bg) = match (self.terminal_sync, self.terminal_pending_sync.is_some())
        {
            (TerminalSyncState::On, true) => {
                ("Sync…", "Waiting for the shell to be ready", theme.warning)
            }
            (TerminalSyncState::On, false) => ("Sync", "Follow Folder", theme.primary),
            (TerminalSyncState::Off, _) => ("Sync Off", "Follow Folder", theme.secondary),
        };
        Button::new("terminal-sync")
            .small()
            .label(label)
            .bg(bg)
            .text_color(crate::theme::text_on(bg, theme.foreground))
            .tooltip(tooltip)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_terminal_sync(cx)))
    }

    fn render_terminal_content(
        &self,
        cwd: SharedString,
        palette: crate::theme::TerminalPalette,
        font_family: String,
        line_height: f32,
        cell_width: f32,
        focused: bool,
    ) -> impl IntoElement {
        let snapshot = self
            .terminal_session
            .as_ref()
            .map(TerminalSession::snapshot);
        let mut body = v_flex().relative().size_full().font_family(font_family);
        if let Some(error) = &self.terminal_error {
            return body.child(
                div()
                    .text_color(palette.bright_red)
                    .child(SharedString::from(error.clone())),
            );
        }
        if let Some(snapshot) = snapshot {
            for row in snapshot.rows {
                let mut line = h_flex()
                    .h(px(line_height))
                    .w_full()
                    .flex_shrink_0()
                    .items_center();
                for run in row.runs {
                    line = line.child(render_terminal_run(run, palette, cell_width, line_height));
                }
                body = body.child(line);
            }
            body = body.when(snapshot.cursor.visible, |body| {
                body.child(render_terminal_cursor(
                    snapshot.cursor.row,
                    snapshot.cursor.column,
                    snapshot.cursor.shape,
                    focused,
                    palette,
                    cell_width,
                    line_height,
                ))
            });
            return body;
        }
        body.child(
            div()
                .text_color(palette.bright_black)
                .child(SharedString::from(format!("Starting terminal in {cwd}…"))),
        )
    }

    fn handle_terminal_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let Some(session) = &self.terminal_session else {
            return;
        };
        let Some(bytes) = terminal_key_bytes(event, session.snapshot().application_cursor_keys)
        else {
            return;
        };
        if let Err(error) = session.write(&bytes) {
            self.terminal_error = Some(error.to_string());
        }
        // Whether the input line is now dirty is the §6 precondition Ferret
        // can answer without shell integration, and this is where it changes.
        self.terminal_input_dirty = !submits_or_clears_the_input_line(&bytes);
        self.terminal_last_key = Some(Instant::now());
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

fn render_terminal_run(
    run: TerminalRun,
    palette: crate::theme::TerminalPalette,
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    let TerminalStyle {
        foreground,
        background,
        bold,
        dim,
        italic,
        underline,
        strikeout,
        inverse,
    } = run.style;
    let mut foreground = resolve_terminal_color(foreground, palette);
    let mut background = resolve_terminal_color(background, palette);
    if inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    if dim {
        foreground = foreground.opacity(0.62);
    }

    div()
        .h(px(line_height))
        .w(px(run.columns as f32 * cell_width))
        .flex_shrink_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .bg(background)
        .text_color(foreground)
        .when(bold, |el| el.font_weight(gpui::FontWeight::BOLD))
        .when(italic, |el| el.italic())
        .when(underline, |el| el.underline())
        .when(strikeout, |el| el.line_through())
        .child(SharedString::from(run.text))
}

fn render_terminal_cursor(
    row: usize,
    column: usize,
    shape: CursorShape,
    focused: bool,
    palette: crate::theme::TerminalPalette,
    cell_width: f32,
    line_height: f32,
) -> impl IntoElement {
    let hollow = !focused || shape == CursorShape::HollowBlock;
    div()
        .absolute()
        .left(px(column as f32 * cell_width))
        .top(px(row as f32 * line_height))
        .w(px(match shape {
            CursorShape::Beam => 2.,
            _ => cell_width,
        }))
        .h(px(match shape {
            CursorShape::Underline => 2.,
            _ => line_height,
        }))
        .when(shape == CursorShape::Underline, |el| {
            el.top(px((row + 1) as f32 * line_height - 2.))
        })
        .when(hollow, |el| {
            el.border_1().border_color(palette.cursor.opacity(0.9))
        })
        .when(!hollow, |el| el.bg(palette.cursor.opacity(0.72)))
}

fn resolve_terminal_color(
    color: TerminalColor,
    palette: crate::theme::TerminalPalette,
) -> gpui::Hsla {
    let named = [
        palette.black,
        palette.red,
        palette.green,
        palette.yellow,
        palette.blue,
        palette.magenta,
        palette.cyan,
        palette.white,
        palette.bright_black,
        palette.bright_red,
        palette.bright_green,
        palette.bright_yellow,
        palette.bright_blue,
        palette.bright_magenta,
        palette.bright_cyan,
        palette.bright_white,
    ];
    match color {
        TerminalColor::Foreground => palette.foreground,
        TerminalColor::Background => palette.background,
        TerminalColor::Cursor => palette.cursor,
        TerminalColor::Named(index) => named[index.min(15) as usize],
        TerminalColor::Indexed(index @ 0..=15) => named[index as usize],
        TerminalColor::Indexed(index @ 16..=231) => {
            let index = index - 16;
            let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            terminal_rgb(
                component(index / 36),
                component((index % 36) / 6),
                component(index % 6),
            )
        }
        TerminalColor::Indexed(index) => {
            let gray = 8 + (index - 232) * 10;
            terminal_rgb(gray, gray, gray)
        }
        TerminalColor::Rgb { red, green, blue } => terminal_rgb(red, green, blue),
    }
}

fn terminal_rgb(red: u8, green: u8, blue: u8) -> gpui::Hsla {
    gpui::Rgba {
        r: red as f32 / 255.,
        g: green as f32 / 255.,
        b: blue as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn terminal_key_bytes(
    event: &gpui::KeyDownEvent,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
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
        "up" if application_cursor_keys => "\x1bOA",
        "down" if application_cursor_keys => "\x1bOB",
        "right" if application_cursor_keys => "\x1bOC",
        "left" if application_cursor_keys => "\x1bOD",
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

/// Whether these bytes leave the shell's input line empty again.
///
/// Deliberately conservative: anything not on this list is treated as typing,
/// because the cost of guessing wrong is a `cd` appended to a half-written
/// `rm`. Real emptiness comes from OSC 133's `B` mark, which is what replaces
/// this once shell integration lands.
fn submits_or_clears_the_input_line(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"\r"      // Enter — runs the line
            | b"\n"
            | [0x03] // Ctrl-C — abandons it
            | [0x15] // Ctrl-U — kills it
            | [0x1b, 0x03]
            | [0x1b, 0x15]
    )
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
            terminal_key_bytes(&key("a", Some("a"), gpui::Modifiers::default()), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(&key("enter", None, gpui::Modifiers::default()), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(&key("left", None, gpui::Modifiers::default()), false),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn application_cursor_mode_uses_ss3_navigation_sequences() {
        assert_eq!(
            terminal_key_bytes(&key("left", None, gpui::Modifiers::default()), true),
            Some(b"\x1bOD".to_vec())
        );
    }

    #[test]
    fn control_c_reaches_the_shell_but_command_shortcuts_do_not() {
        assert_eq!(
            terminal_key_bytes(
                &key(
                    "c",
                    None,
                    gpui::Modifiers {
                        control: true,
                        ..Default::default()
                    },
                ),
                false
            ),
            Some(vec![3])
        );
        assert_eq!(
            terminal_key_bytes(
                &key(
                    "c",
                    None,
                    gpui::Modifiers {
                        platform: true,
                        ..Default::default()
                    },
                ),
                false
            ),
            None
        );
    }

    /// The gate that keeps an injected `cd` from completing a half-typed
    /// command: only the keys that empty the line clear the flag.
    #[test]
    fn only_submitting_or_killing_the_line_leaves_it_clean() {
        assert!(submits_or_clears_the_input_line(b"\r"));
        assert!(submits_or_clears_the_input_line(&[0x03]));
        assert!(submits_or_clears_the_input_line(&[0x15]));

        assert!(!submits_or_clears_the_input_line(b"r"));
        assert!(!submits_or_clears_the_input_line(b"\x7f"));
        assert!(!submits_or_clears_the_input_line(b"\x1b[D"));
        // A `\r` inside a longer sequence is not a submission.
        assert!(!submits_or_clears_the_input_line(b"rm \r"));
    }
}
