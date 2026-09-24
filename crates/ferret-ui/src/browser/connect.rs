//! Connecting a saved endpoint and mounting it (R-4).
//!
//! The whole flow lives in one place because it is one conversation: dial,
//! and when the server asks something back — an unrecognised host key, a
//! passphrase, a password — put the question to the person and dial again with
//! the answer. Spreading that across handlers would make it impossible to see
//! that the retry loop terminates.
//!
//! Two rules the loop enforces, both of them security rather than polish:
//!
//! * A host key is confirmed **before** anything is sent, and a *changed* key
//!   is not a question at all — it is a refusal (R-D08).
//! * A secret is only ever kept when the person ticks the box, and then only
//!   in the OS keychain (R-D07).

use super::*;

use std::cell::RefCell;
use std::rc::Rc;

use ferret_remote::{
    Connection, ConnectionId, Credentials, HostKeyPolicy, PromptKind, RemoteError, Secret,
    SecretKind,
};
use gpui::{AnyWindowHandle, Focusable as _};
use gpui_component::dialog::DialogButtonProps;

use crate::mounts::MountStatus;

/// How many times the dial loop may be answered before it gives up.
///
/// A person answering a prompt is a normal step, not an error, but the loop
/// must terminate even if a server keeps asking: three questions is more than
/// any real login needs.
const MAX_PROMPTS: usize = 3;

impl Browser {
    /// Open a saved connection: navigate if it is already mounted, otherwise
    /// dial it and navigate when it lands.
    pub(super) fn open_connection(
        &mut self,
        record: Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_connection_with(record, false, window, cx);
    }

    fn open_connection_with(
        &mut self,
        record: Connection,
        as_root: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = cx.global::<FerretState>();
        if let Some(home) = state.mounts.home(&record.id) {
            // Already connected: a second click is navigation, not a re-dial.
            self.navigate(home, window, cx);
            return;
        }
        if state.mounts.status(&record.id).is_busy() {
            return;
        }

        cx.global_mut::<FerretState>().mounts.begin(&record.id);
        cx.notify();

        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let outcome = dial(record.clone(), as_root, handle, cx).await;
            let _ = handle.update(cx, |_, window, cx| match outcome {
                Ok(Some(home)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.watch_mount(record.clone(), window, cx);
                        this.navigate(home, window, cx);
                        cx.notify();
                    });
                }
                // The person cancelled a prompt. Not a failure: leave the row
                // idle and say nothing.
                Ok(None) => {
                    drop(cx.global_mut::<FerretState>().mounts.take(&record.id));
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                Err(error) => {
                    let message = error.to_string();
                    cx.global_mut::<FerretState>()
                        .mounts
                        .fail(&record.id, message.clone());
                    window.push_notification(Notification::error(message), cx);
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            });
        })
        .detach();
    }

    /// Connect as root, once someone has typed the host's name to say so
    /// (F-23).
    ///
    /// This is the sharpest edge in the product: it hands a file manager
    /// unrestricted write access to a machine, where one mis-aimed drag
    /// deletes something no trash will give back. Three things guard it, and
    /// all three are deliberate friction:
    ///
    /// * The confirmation cannot be dismissed by pressing Return. The host's
    ///   name has to be typed, which is impossible to do by accident and
    ///   impossible to do without reading what it says.
    /// * It is per-connect. Nothing persists it — `Connection::sanitised`
    ///   makes the saved record incapable of expressing it — so it can never
    ///   come back on from restored state, and a reconnect drops it.
    /// * The mount is marked wherever it appears, for as long as it lasts.
    pub(super) fn connect_as_root(
        &mut self,
        record: Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(record.host.clone()));
        let focus = input.read(cx).focus_handle(cx).clone();

        let this = cx.weak_entity();
        let expected = record.host.clone();
        let name = record.display_name();

        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let theme = cx.theme();
            let confirm_input = input.clone();
            let this = this.clone();
            let record = record.clone();
            let expected = expected.clone();
            modal
                .w(px(520.))
                .close_button(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.danger)
                                .child(format!("Mount {name} as root?")),
                        )
                        .child(div().text_color(theme.muted_foreground).child(
                            "Every file on that server becomes writable and deletable from \
                             this window, and there is no trash to undo it with. The \
                             account needs passwordless sudo for the SFTP server.",
                        ))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(format!("Type {expected} to continue.")),
                        )
                        .child(Input::new(&input)),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Mount as root")
                        .ok_variant(gpui_component::button::ButtonVariant::Danger)
                        .cancel_text("Cancel"),
                )
                .on_ok(move |_, window, cx| {
                    // Anything other than the host's exact name keeps the
                    // dialog open. There is no Return-to-confirm here.
                    if confirm_input.read(cx).value().trim() != expected {
                        return false;
                    }
                    if let Some(browser) = this.upgrade() {
                        let record = record.clone();
                        browser.update(cx, |browser, cx| {
                            browser.open_connection_with(record, true, window, cx);
                        });
                    }
                    true
                })
        });
    }

    /// Watch a live mount, and re-dial once if the link goes away.
    ///
    /// One attempt, not a loop: a server that is down does not become up
    /// because it was asked four times, and a banner offering Reconnect is
    /// more useful than a spinner that never resolves.
    ///
    /// The watcher holds the closed *signal*, never the session — a strong
    /// reference here would keep the connection alive after the user asked to
    /// close it, since the actor only shuts down once its last sender drops.
    fn watch_for_disconnect(
        &mut self,
        record: Connection,
        closed: ferret_remote::ClosedSignal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            closed.wait().await;

            // Deliberate disconnects come through here too. The mount is
            // already gone from the registry in that case, and re-dialling a
            // server the user just closed would be the worst possible answer.
            if !cx.update(|cx| cx.global::<FerretState>().mounts.is_connected(&record.id)) {
                return;
            }

            cx.update(|cx| {
                let state = cx.global_mut::<FerretState>();
                state.fs.unmount(record.mount);
                state.mounts.begin(&record.id);
            });
            let _ = this.update(cx, |_, cx| cx.notify());

            cx.background_executor()
                .timer(ferret_remote::config::RECONNECT_DELAY)
                .await;

            // A fresh dial, with a fresh backend and therefore a fresh cache:
            // a link that dropped proves nothing about what changed on the
            // other side while it was gone.
            // Deliberately not `as_root`: F-23 requires that the switch
            // cannot come back on without someone asking for it again, and a
            // reconnect is not asking.
            match dial(record.clone(), false, handle, cx).await {
                Ok(Some(_)) => {
                    let _ = handle.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.watch_mount(record.clone(), window, cx);
                            // Whatever is on screen was listed by the session
                            // that just died.
                            this.refresh(cx);
                            cx.notify();
                        });
                    });
                }
                outcome => {
                    let message = match outcome {
                        Err(error) => error.to_string(),
                        _ => format!("Lost the connection to {}.", record.display_name()),
                    };
                    let _ = handle.update(cx, |_, window, cx| {
                        cx.global_mut::<FerretState>()
                            .mounts
                            .fail(&record.id, message.clone());
                        window.push_notification(Notification::error(message), cx);
                        let _ = this.update(cx, |_, cx| cx.notify());
                    });
                }
            }
        })
        .detach();
    }

    /// Start watching a mount that has just come up.
    fn watch_mount(&mut self, record: Connection, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = cx.global::<FerretState>().mounts.closed_signal(&record.id) else {
            return;
        };
        self.watch_for_disconnect(record, closed, window, cx);
    }

    /// Close a mount and stop routing its paths.
    ///
    /// Tabs inside it keep their path and show the unmounted state rather than
    /// closing themselves — a disconnect should not lose the user's place.
    pub(super) fn disconnect_connection(&mut self, record: &Connection, cx: &mut Context<Self>) {
        let state = cx.global_mut::<FerretState>();
        state.fs.unmount(record.mount);
        let session = state.mounts.take(&record.id);
        if let Some(session) = session {
            // Ask politely, then let the handle drop. The disconnect is a
            // courtesy to the server's logs; the drop is what actually frees
            // the connection.
            cx.background_executor()
                .spawn(async move {
                    let _ = session.disconnect().await;
                })
                .detach();
        }
        cx.notify();
    }

    /// Forget a saved connection entirely.
    ///
    /// Disconnects first, then removes the record, then clears anything it put
    /// in the keychain — in that order, so no credential outlives the record
    /// that explained what it was for.
    pub(super) fn remove_connection(&mut self, record: &Connection, cx: &mut Context<Self>) {
        self.disconnect_connection(record, cx);
        let state = cx.global_mut::<FerretState>();
        state.connections.remove(&record.id);
        crate::state::arm_config_save(cx);
        ferret_remote::forget_all_secrets(&record.id);
        cx.notify();
    }

    /// Disconnect everything, for application shutdown.
    pub(crate) fn disconnect_all(cx: &mut App) {
        let state = cx.global_mut::<FerretState>();
        for id in state.mounts.connected_ids() {
            if let Some(connection) = state.connections.get(&id) {
                let mount = connection.mount;
                state.fs.unmount(mount);
            }
            drop(state.mounts.take(&id));
        }
    }

    pub(super) fn mount_status(&self, id: &ConnectionId, cx: &App) -> MountStatus {
        cx.global::<FerretState>().mounts.status(id)
    }
}

/// Dial until the server is satisfied, a question is declined, or something
/// fails for a reason no answer would fix.
///
/// `Ok(None)` means the person cancelled — distinct from `Err`, because a
/// cancelled login is not an error worth a red banner.
async fn dial(
    record: Connection,
    as_root: bool,
    window: AnyWindowHandle,
    cx: &mut gpui::AsyncApp,
) -> ferret_remote::Result<Option<VfsPath>> {
    let mut credentials = Credentials::default();
    let mut policy = HostKeyPolicy::Strict;

    // A remembered secret is offered to the first attempt so a saved password
    // does not re-prompt on every connect.
    if record.remember_secret {
        credentials.password = ferret_remote::load_secret(&record.id, SecretKind::Password)
            .ok()
            .flatten();
        credentials.key_passphrase =
            ferret_remote::load_secret(&record.id, SecretKind::KeyPassphrase)
                .ok()
                .flatten();
    }

    for _ in 0..=MAX_PROMPTS {
        match ferret_remote::connect(record.clone(), credentials.clone(), policy.clone()).await {
            Ok(session) => {
                return match mount(record, session, as_root, cx).await {
                    Ok(landing) => Ok(Some(landing)),
                    // The record went away mid-dial. Not a failure to report:
                    // the row it would have gone red on no longer exists.
                    Err(RemoteError::Cancelled) => Ok(None),
                    Err(error) => Err(error),
                };
            }

            // The one question a person can answer about identity. Accepting
            // writes the key to known_hosts; declining ends the attempt.
            Err(RemoteError::UnknownHostKey {
                host,
                port,
                fingerprint,
                algorithm,
            }) => {
                let accepted = ask_host_key(
                    window,
                    cx,
                    HostKeyQuestion {
                        host: host.clone(),
                        port,
                        fingerprint: fingerprint.clone(),
                        algorithm,
                    },
                )
                .await;
                if !accepted {
                    return Ok(None);
                }
                policy = HostKeyPolicy::AcceptFingerprint(fingerprint);
            }

            Err(RemoteError::AuthPrompt {
                host, prompt, kind, ..
            }) => {
                let Some(answer) = ask_secret(
                    window,
                    cx,
                    SecretQuestion {
                        host: host.clone(),
                        prompt,
                        kind,
                        offer_remember: true,
                    },
                )
                .await
                else {
                    return Ok(None);
                };
                let stored_kind = match kind {
                    PromptKind::KeyPassphrase => SecretKind::KeyPassphrase,
                    _ => SecretKind::Password,
                };
                if answer.remember {
                    // Best effort: a keychain that refuses to store must not
                    // stop a connection that is otherwise ready to go.
                    if ferret_remote::store_secret(&record.id, stored_kind, &answer.secret).is_ok()
                    {
                        // The flag is what the *next* dial reads to know there
                        // is anything to load. Storing the secret without
                        // setting it would prompt again every time and look
                        // like the checkbox did nothing.
                        let id = record.id.clone();
                        cx.update(move |cx| {
                            let state = cx.global_mut::<FerretState>();
                            if let Some(existing) = state.connections.get(&id) {
                                let mut updated = existing.clone();
                                updated.remember_secret = true;
                                state.connections.update(updated);
                            }
                            crate::state::arm_config_save(cx);
                        });
                    }
                }
                match kind {
                    PromptKind::KeyPassphrase => {
                        credentials.key_passphrase = Some(answer.secret);
                    }
                    _ => credentials.password = Some(answer.secret),
                }
            }

            Err(error) => return Err(error),
        }
    }

    Err(RemoteError::AuthFailed {
        host: record.host,
        user: record.user,
        tried: "every credential offered".into(),
    })
}

/// Open the SFTP channel, register the backend, and work out where to land.
async fn mount(
    record: Connection,
    session: ferret_remote::Session,
    as_root: bool,
    cx: &mut gpui::AsyncApp,
) -> ferret_remote::Result<VfsPath> {
    let sftp = if as_root {
        session
            .open_sftp_as_root(record.mount, record.host.clone())
            .await?
    } else {
        session.open_sftp(record.mount, record.host.clone()).await?
    };

    // The account's home unless the record names somewhere else. Resolved
    // through the server rather than assumed, because `/home/<user>` is a
    // guess that is wrong on plenty of systems.
    let landing = match &record.initial_path {
        Some(path) => VfsPath::remote(record.mount, path.clone()),
        None => sftp
            .home()
            .await
            .unwrap_or_else(|_| VfsPath::remote(record.mount, "/")),
    };

    let session = std::sync::Arc::new(session);
    // One backend, two handles: the router routes through the trait object,
    // and transfers need the concrete type for handles and offsets the trait
    // has no shape for. Both point at the same session.
    let backend = std::sync::Arc::new(sftp);
    let routed: std::sync::Arc<dyn ferret_vfs::Vfs> = backend.clone();
    let landed = landing.clone();

    // A dial takes seconds, and the record can be deleted or the attempt
    // abandoned while it runs. Installing a backend for a connection the
    // sidebar no longer shows would leave a mount nobody can reach the
    // disconnect for, so the attempt has to still be the current one.
    let installed = cx.update(move |cx| {
        let state = cx.global_mut::<FerretState>();
        let still_current = state.connections.get(&record.id).is_some()
            && state.mounts.status(&record.id).is_busy();
        if !still_current {
            return false;
        }
        // Registered with the router *before* the mount is marked connected,
        // so nothing can navigate to a path the router would refuse.
        state.fs.mount(record.mount, routed);
        state
            .mounts
            .finish(&record.id, session, backend, landed, as_root);
        true
    });
    if !installed {
        // Dropping the session here is what closes the connection that was
        // opened for a record that is now gone.
        return Err(RemoteError::Cancelled);
    }

    Ok(landing)
}

struct HostKeyQuestion {
    host: String,
    port: u16,
    fingerprint: String,
    algorithm: String,
}

struct SecretQuestion {
    host: String,
    prompt: String,
    kind: PromptKind,
    offer_remember: bool,
}

struct SecretAnswer {
    secret: Secret,
    remember: bool,
}

/// Show the fingerprint and wait for an answer.
///
/// Default focus is Cancel and the accept button is not the primary style:
/// this dialog exists to be read, and a person who presses Return without
/// reading it should not have accepted a key.
async fn ask_host_key(
    window: AnyWindowHandle,
    cx: &mut gpui::AsyncApp,
    question: HostKeyQuestion,
) -> bool {
    let (tx, rx) = futures::channel::oneshot::channel();
    let sender = Rc::new(RefCell::new(Some(tx)));

    let opened = window.update(cx, |_, window, cx| {
        let endpoint = if question.port == 22 {
            question.host.clone()
        } else {
            format!("{}:{}", question.host, question.port)
        };
        let fingerprint = question.fingerprint.clone();
        let algorithm = question.algorithm.clone();
        window.open_dialog(cx, move |modal, _, cx| {
            let theme = cx.theme();
            let accept = sender.clone();
            let decline = sender.clone();
            modal
                .w(px(560.))
                .close_button(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(format!("{endpoint} isn't in your known_hosts file")),
                        )
                        .child(div().text_color(theme.muted_foreground).child(
                            "Check this fingerprint against one you trust before connecting. \
                             If it doesn't match, someone may be impersonating the server.",
                        ))
                        .child(
                            v_flex()
                                .gap_1()
                                .p_3()
                                .rounded(px(6.))
                                .bg(theme.secondary)
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme.muted_foreground)
                                        .child(algorithm.clone()),
                                )
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(12.))
                                        .child(fingerprint.clone()),
                                ),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Connect and remember")
                        // Deliberately not the primary style. This dialog
                        // exists to be read, and the eye goes to the accent.
                        .ok_variant(gpui_component::button::ButtonVariant::Secondary)
                        .cancel_text("Cancel"),
                )
                .on_ok(move |_, _, _| {
                    if let Some(tx) = accept.borrow_mut().take() {
                        let _ = tx.send(true);
                    }
                    true
                })
                .on_cancel(move |_, _, _| {
                    if let Some(tx) = decline.borrow_mut().take() {
                        let _ = tx.send(false);
                    }
                    true
                })
        });
    });

    if opened.is_err() {
        return false;
    }
    rx.await.unwrap_or(false)
}

/// Ask for a password or passphrase.
///
/// The prompt names the host and what the secret is for. A credential dialog
/// that does not say who is asking is a phishing surface, which is why the
/// text is built from the error the server produced rather than a constant.
async fn ask_secret(
    window: AnyWindowHandle,
    cx: &mut gpui::AsyncApp,
    question: SecretQuestion,
) -> Option<SecretAnswer> {
    let (tx, rx) = futures::channel::oneshot::channel();
    let sender = Rc::new(RefCell::new(Some(tx)));

    let opened = window.update(cx, |_, window, cx| {
        let placeholder = match question.kind {
            PromptKind::KeyPassphrase => "Key passphrase",
            PromptKind::Password => "Account password",
            PromptKind::Challenge => "Response",
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                // Masked, so a shoulder cannot read it. `unmask_value` is what
                // reads it back — `value` returns the dots.
                .masked(true)
                .placeholder(placeholder)
        });
        let remember = Rc::new(std::cell::Cell::new(false));
        let focus = input.read(cx).focus_handle(cx).clone();

        let title = format!("{} needs {}", question.host, question.prompt);
        let offer_remember = question.offer_remember;
        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let theme = cx.theme();
            let confirm_input = input.clone();
            let confirm_remember = remember.clone();
            let cancel = sender.clone();
            let accept = sender.clone();
            let checkbox_state = remember.clone();
            modal
                .w(px(480.))
                .close_button(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(title.clone()),
                        )
                        .child(Input::new(&input))
                        .when(offer_remember, |el| {
                            el.child(
                                gpui_component::checkbox::Checkbox::new("remember-secret")
                                    .label("Remember in my keychain")
                                    .checked(checkbox_state.get())
                                    .on_click(move |checked, _, _| checkbox_state.set(*checked)),
                            )
                        })
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(
                                    "Ferret never writes secrets to its own files — \
                                     remembering one stores it in the system keychain.",
                                ),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Connect")
                        .cancel_text("Cancel"),
                )
                .on_ok(move |_, _, cx| {
                    let value = confirm_input.read(cx).unmask_value().to_string();
                    if value.is_empty() {
                        return false; // Nothing to send; keep the dialog open.
                    }
                    if let Some(tx) = accept.borrow_mut().take() {
                        let _ = tx.send(Some(SecretAnswer {
                            secret: Secret::new(value),
                            remember: confirm_remember.get(),
                        }));
                    }
                    true
                })
                .on_cancel(move |_, _, _| {
                    if let Some(tx) = cancel.borrow_mut().take() {
                        let _ = tx.send(None);
                    }
                    true
                })
        });
    });

    if opened.is_err() {
        return None;
    }
    rx.await.ok().flatten()
}
