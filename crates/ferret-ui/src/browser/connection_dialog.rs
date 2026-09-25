//! Saved SSH connection editor (F-20).

use super::*;

use ferret_remote::{Connection, ConnectionId};
use gpui::Focusable as _;
use gpui_component::dialog::{Cancel, Confirm};

const DIALOG_WIDTH: f32 = 620.;

pub(super) struct ConnectionDialog {
    editing: Option<ConnectionId>,
    name: Entity<InputState>,
    host: Entity<InputState>,
    user: Entity<InputState>,
    port: Entity<InputState>,
    key_path: Entity<InputState>,
    notes: Entity<InputState>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectionDialog {
    fn new(record: Option<Connection>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let record = record.unwrap_or_default();
        let values = [
            (record.name.clone(), "My server"),
            (record.host.clone(), "server.example.com"),
            (record.user.clone(), "Username"),
            (record.port.map(|p| p.to_string()).unwrap_or_default(), "22"),
            (
                record
                    .key_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "~/.ssh/id_ed25519 (leave blank for default)",
            ),
            (record.notes.clone(), "Optional notes"),
        ];
        let inputs: Vec<_> = values
            .into_iter()
            .map(|(value, placeholder)| {
                cx.new(|cx| {
                    let mut state = InputState::new(window, cx).placeholder(placeholder);
                    state.set_value(value, window, cx);
                    state
                })
            })
            .collect();
        let subscriptions = inputs
            .iter()
            .map(|input| cx.subscribe(input, |_, _, _: &InputEvent, cx| cx.notify()))
            .collect();
        Self {
            editing: (!record.id.0.is_empty()).then_some(record.id),
            name: inputs[0].clone(),
            host: inputs[1].clone(),
            user: inputs[2].clone(),
            port: inputs[3].clone(),
            key_path: inputs[4].clone(),
            notes: inputs[5].clone(),
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_string()
    }

    fn record(&self, cx: &App) -> std::result::Result<Connection, SharedString> {
        let port = Self::value(&self.port, cx);
        let port = if port.is_empty() || port == "22" {
            None
        } else {
            Some(
                port.parse::<u16>()
                    .map_err(|_| SharedString::from("Port must be a number from 1 to 65535."))?,
            )
        };
        let key = Self::value(&self.key_path, cx);
        let mut record = Connection {
            id: self
                .editing
                .clone()
                .unwrap_or_else(|| ConnectionId(String::new())),
            name: Self::value(&self.name, cx),
            host: Self::value(&self.host, cx),
            user: Self::value(&self.user, cx),
            port,
            key_path: (!key.is_empty()).then(|| PathBuf::from(key)),
            notes: Self::value(&self.notes, cx),
            ..Connection::default()
        };
        if let Some(existing) = self
            .editing
            .as_ref()
            .and_then(|id| cx.global::<FerretState>().connections.get(id))
        {
            record.mount = existing.mount;
            record.initial_path = existing.initial_path.clone();
            record.remember_secret = existing.remember_secret;
            record.unknown = existing.unknown.clone();
        }
        record.validate().map_err(SharedString::from)?;
        Ok(record)
    }
}

impl Render for ConnectionDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let field = |label: &'static str, input: &Entity<InputState>| {
            h_flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(92.))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(Input::new(input).flex_1())
        };
        let preview = self
            .record(cx)
            .map(|r| r.ssh_command())
            .unwrap_or_else(|_| "ssh …".into());
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Icon::new(gpui_component::IconName::Globe)
                            .size_6()
                            .text_color(theme.primary),
                    )
                    .child(
                        v_flex()
                            .child(
                                div()
                                    .text_size(rem_from_ui_px(15.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(if self.editing.is_some() {
                                        "Edit Connection"
                                    } else {
                                        "New Connection"
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(rem_from_ui_px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child("SSH remote server"),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(rem_from_ui_px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("SERVER"),
            )
            .child(field("Name", &self.name))
            .child(field("Host", &self.host))
            .child(field("Username", &self.user))
            .child(field("Port", &self.port))
            .child(
                div()
                    .text_size(rem_from_ui_px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("AUTHENTICATION"),
            )
            .child(field("SSH Key", &self.key_path))
            .child(field("Notes", &self.notes))
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .text_color(theme.danger)
                        .text_size(rem_from_ui_px(12.))
                        .child(error),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(rem_from_ui_px(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("SSH COMMAND"),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded(px(6.))
                            .bg(theme.secondary)
                            .font_family("monospace")
                            .child(preview),
                    ),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("connection-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("connection-save")
                            .primary()
                            .label("Save")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                            }),
                    ),
            )
    }
}

impl Browser {
    pub(super) fn open_connection_dialog(
        &mut self,
        record: Option<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let dialog = cx.new(|cx| ConnectionDialog::new(record, window, cx));
        self.connection_dialog = Some(dialog.clone());
        let focus = dialog.read(cx).name.read(cx).focus_handle(cx).clone();
        let browser = cx.weak_entity();
        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let dialog = dialog.clone();
            let browser = browser.clone();
            modal
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .child(dialog.clone())
                .on_ok(move |_, _, cx| {
                    let Ok(record) = dialog.read_with(cx, |dialog, cx| dialog.record(cx)) else {
                        dialog.update(cx, |dialog, cx| {
                            dialog.error = dialog.record(cx).err();
                            cx.notify();
                        });
                        return false;
                    };
                    let editing = dialog.read(cx).editing.is_some();
                    let state = cx.global_mut::<FerretState>();
                    if editing {
                        state.connections.update(record);
                    } else {
                        state.connections.insert(record);
                    }
                    crate::state::arm_config_save(cx);
                    if let Some(browser) = browser.upgrade() {
                        browser.update(cx, |_, cx| cx.notify());
                    }
                    true
                })
        });
        cx.notify();
    }
}
