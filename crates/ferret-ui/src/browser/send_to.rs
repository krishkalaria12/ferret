//! "Send to" — transferring a selection to a saved server (R-6, F-22).
//!
//! The gesture the reference product shows is a right-click away from a
//! transfer, so the menu is the entry point and the destination sheet is the
//! only thing between it and the copy. Dropping a selection on a connected
//! sidebar row is the same operation with the destination already answered.
//!
//! Progress lives on the browser rather than in a dialog: a transfer that
//! blocks the window while it runs would make a large copy unusable, and the
//! whole point of the queue is that browsing continues.

use super::*;

use std::cell::RefCell;
use std::rc::Rc;

use ferret_remote::{
    Conflict, Connection, ConnectionId, Direction, TransferDestination, TransferEvent,
    TransferSource,
};
use gpui::Focusable as _;
use gpui_component::dialog::DialogButtonProps;

/// How often a queued "Send to" checks whether its dial has landed.
///
/// Coarse on purpose: nothing is being drawn from this, and the dial itself
/// takes hundreds of milliseconds at best.
const CONNECT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long a queued "Send to" waits before giving up on its dial.
///
/// Generous, because the wait may include a person reading a fingerprint or
/// finding a hardware token — but bounded, so a dialog left open overnight
/// does not surface a destination sheet in the morning.
const CONNECT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// What the status bar shows while a transfer runs.
#[derive(Clone, Debug)]
pub(super) struct TransferProgress {
    pub(super) label: SharedString,
    pub(super) bytes_done: u64,
    pub(super) bytes_total: u64,
    /// Set when the job has finished, so the summary can linger briefly
    /// instead of vanishing the instant the last byte lands.
    pub(super) finished: bool,
}

impl TransferProgress {
    /// Completion in `0.0..=1.0`, or `None` when the total is not yet known.
    pub(super) fn fraction(&self) -> Option<f32> {
        (self.bytes_total > 0)
            .then(|| (self.bytes_done as f64 / self.bytes_total as f64) as f32)
            .map(|value| value.clamp(0., 1.))
    }
}

impl Browser {
    /// The status-bar segment for a running transfer, or `None` when there is
    /// none.
    ///
    /// A bar rather than a percentage on its own: a copy is the one operation
    /// where "how much is left" is the whole question, and a number alone
    /// makes the user do the comparison every time they glance at it.
    pub(super) fn render_transfer_segment(&self, cx: &App) -> Option<AnyElement> {
        let progress = self.transfer.as_ref()?;
        let theme = cx.theme();
        let fraction = progress.fraction();

        Some(
            h_flex()
                .gap_2()
                .items_center()
                .min_w_0()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(progress.label.clone()),
                )
                .when_some(fraction, |el, fraction| {
                    el.child(
                        // Hand-drawn rather than a component: at 4 px tall in a
                        // 24 px bar, a progress widget's own padding and radius
                        // would dominate what it is trying to show.
                        div()
                            .w(px(72.))
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(theme.secondary)
                            .child(
                                div()
                                    .w(gpui::relative(fraction))
                                    .h_full()
                                    .rounded(px(2.))
                                    .bg(if progress.finished {
                                        theme.muted_foreground
                                    } else {
                                        theme.primary
                                    }),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    /// Begin a "Send to": connect if necessary, ask where, then transfer.
    pub(super) fn send_to_connection(
        &mut self,
        record: Connection,
        sources: Vec<VfsPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let locals: Vec<std::path::PathBuf> = sources
            .iter()
            .filter_map(|path| path.as_local().map(Path::to_path_buf))
            .collect();
        if locals.is_empty() {
            window.push_notification(
                Notification::error("Only local files can be sent to a server."),
                cx,
            );
            return;
        }
        let initiating_folder = self.active().folder.clone();

        // Connecting first means there is a home directory to pre-fill and a
        // backend to hand the files to. A drop onto an idle row therefore
        // dials and then carries on by itself — the gesture was "send this
        // there", and making the user repeat it after watching a spinner
        // would be answering a different question.
        if !cx.global::<FerretState>().mounts.is_connected(&record.id) {
            self.open_connection(record.clone(), window, cx);
            self.send_when_connected(record, locals, initiating_folder, window, cx);
            return;
        }

        let default_dir = cx
            .global::<FerretState>()
            .mounts
            .home(&record.id)
            .map(|home| home.to_string())
            .unwrap_or_else(|| "/".to_string());

        self.ask_destination(record, locals, default_dir, initiating_folder, window, cx);
    }

    /// Wait for an in-flight dial, then ask where the files should go.
    ///
    /// Polled rather than driven by a signal because the dial may stop for a
    /// person — a fingerprint to read, a passphrase to type — and those are
    /// dialogs of their own. Asking for a destination on top of one would
    /// stack two, so this waits for the row to settle and only then continues.
    fn send_when_connected(
        &mut self,
        record: Connection,
        sources: Vec<std::path::PathBuf>,
        initiating_folder: Entity<Folder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let deadline = std::time::Instant::now() + CONNECT_WAIT;
            loop {
                cx.background_executor().timer(CONNECT_POLL).await;

                let status = cx.update(|cx| {
                    let mounts = &cx.global::<FerretState>().mounts;
                    (mounts.is_connected(&record.id), mounts.status(&record.id))
                });
                match status {
                    (true, _) => break,
                    // The dial failed or was cancelled; it has already said so.
                    (false, state) if !state.is_busy() => return,
                    _ if std::time::Instant::now() >= deadline => return,
                    _ => continue,
                }
            }

            let default_dir = cx
                .update(|cx| cx.global::<FerretState>().mounts.home(&record.id))
                .map(|home| home.to_string())
                .unwrap_or_else(|| "/".to_string());

            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.ask_destination(
                        record.clone(),
                        sources,
                        default_dir,
                        initiating_folder.clone(),
                        window,
                        cx,
                    );
                });
            });
        })
        .detach();
    }

    /// The destination sheet: which remote directory, and what to do about
    /// names that are already taken.
    fn ask_destination(
        &mut self,
        record: Connection,
        sources: Vec<std::path::PathBuf>,
        default_dir: String,
        initiating_folder: Entity<Folder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("/srv/incoming");
            state.set_value(default_dir, window, cx);
            state
        });
        let focus = input.read(cx).focus_handle(cx).clone();

        let replace = Rc::new(std::cell::Cell::new(false));
        let this = cx.weak_entity();
        let initiating_folder = initiating_folder.clone();
        let count = sources.len();
        let sources = Rc::new(RefCell::new(Some(sources)));
        let title = format!("Send {count} item(s) to {}", record.display_name());

        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let theme = cx.theme();
            let input = input.clone();
            let confirm_input = input.clone();
            let replace_state = replace.clone();
            let confirm_replace = replace.clone();
            let this = this.clone();
            let record = record.clone();
            let sources = sources.clone();
            modal
                .w(px(520.))
                .close_button(false)
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_size(rem_from_ui_px(14.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(title.clone()),
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .w(px(92.))
                                        .text_color(theme.muted_foreground)
                                        .child("Folder"),
                                )
                                .child(Input::new(&input).flex_1()),
                        )
                        .child(
                            gpui_component::checkbox::Checkbox::new("replace-existing")
                                .label("Replace files that already exist")
                                .checked(replace_state.get())
                                .on_click(move |checked, _, _| replace_state.set(*checked)),
                        )
                        .child(
                            div()
                                .text_size(rem_from_ui_px(11.))
                                .text_color(theme.muted_foreground)
                                .child(
                                    "Unchecked, anything already there is left alone \
                                     and reported as skipped.",
                                ),
                        ),
                )
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Send")
                        .cancel_text("Cancel"),
                )
                .on_ok({
                    let initiating_folder = initiating_folder.clone();
                    move |_, window, cx| {
                        let directory = confirm_input.read(cx).value().trim().to_string();
                        // An absolute path is the only thing the server will
                        // accept, and guessing a root for a relative one would put
                        // files somewhere the user did not name.
                        if !directory.starts_with('/') {
                            return false;
                        }
                        let Some(sources) = sources.borrow_mut().take() else {
                            return true;
                        };
                        let conflict = if confirm_replace.get() {
                            Conflict::Replace
                        } else {
                            Conflict::Skip
                        };
                        if let Some(browser) = this.upgrade() {
                            let record = record.clone();
                            browser.update(cx, |browser, cx| {
                                browser.start_upload(
                                    record,
                                    sources,
                                    directory,
                                    conflict,
                                    initiating_folder.clone(),
                                    window,
                                    cx,
                                );
                            });
                        }
                        true
                    }
                })
        });
    }

    /// Run the transfer and pump its progress into the status bar.
    #[allow(clippy::too_many_arguments)]
    fn start_upload(
        &mut self,
        record: Connection,
        sources: Vec<std::path::PathBuf>,
        directory: String,
        conflict: Conflict,
        initiating_folder: Entity<Folder>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = cx.global::<FerretState>().mounts.backend(&record.id) else {
            window.push_notification(
                Notification::error(format!("{} isn't connected.", record.display_name())),
                cx,
            );
            return;
        };

        let endpoint = VfsPath::remote(record.mount, directory.as_str());
        let job = backend.transfer(
            Direction::Upload,
            sources.into_iter().map(TransferSource::Local).collect(),
            TransferDestination::RemoteDir(directory),
            conflict,
        );
        self.pump_transfer(
            job,
            record.id,
            initiating_folder,
            vec![endpoint],
            window,
            cx,
        );
    }

    /// Forward transfer events onto the status bar until the job ends.
    ///
    /// The task is held on the browser, so navigating away does not cancel a
    /// transfer — but closing the window does, which is the right lifetime for
    /// something the user can still see.
    fn pump_transfer(
        &mut self,
        job: ferret_remote::Transfer,
        id: ConnectionId,
        initiating_folder: Entity<Folder>,
        remote_endpoints: Vec<VfsPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let window_handle = window.window_handle();
        let fs = cx.global::<FerretState>().fs.clone();
        self.transfer = Some(TransferProgress {
            label: "Preparing…".into(),
            bytes_done: 0,
            bytes_total: 0,
            finished: false,
        });
        cx.notify();

        self.transfer_task = Some(cx.spawn(async move |this, cx| {
            let mut summary = None;
            while let Some(event) = job.next().await {
                let update = match event {
                    TransferEvent::Started {
                        name,
                        index,
                        total_files,
                    } => Some(format!("Sending {name} ({index}/{total_files})")),
                    TransferEvent::Progress {
                        bytes_done,
                        bytes_total,
                    } => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(progress) = &mut this.transfer {
                                progress.bytes_done = bytes_done;
                                progress.bytes_total = bytes_total;
                            }
                            cx.notify();
                        });
                        None
                    }
                    TransferEvent::Skipped { name } => Some(format!("Skipped {name}")),
                    TransferEvent::Failed { name, reason } => {
                        let message = if name.is_empty() {
                            reason
                        } else {
                            format!("{name}: {reason}")
                        };
                        let _ = window_handle.update(cx, |_, window, cx| {
                            window.push_notification(Notification::error(message), cx);
                        });
                        None
                    }
                    TransferEvent::Finished {
                        copied,
                        skipped,
                        failed,
                    } => {
                        summary = Some(format!(
                            "Sent {copied} item(s){}{}",
                            if skipped > 0 {
                                format!(", skipped {skipped}")
                            } else {
                                String::new()
                            },
                            if failed > 0 {
                                format!(", {failed} failed")
                            } else {
                                String::new()
                            }
                        ));
                        break;
                    }
                };
                if let Some(label) = update {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(progress) = &mut this.transfer {
                            progress.label = label.into();
                        }
                        cx.notify();
                    });
                }
            }

            Browser::reload_transfer_folder(&initiating_folder, &fs, &remote_endpoints, cx);

            let _ = this.update(cx, |this, cx| {
                if let Some(progress) = &mut this.transfer {
                    progress.label = summary.unwrap_or_else(|| "Transfer stopped".into()).into();
                    progress.finished = true;
                }
                cx.notify();
            });

            // The summary is worth reading, and then it is clutter.
            cx.background_executor()
                .timer(std::time::Duration::from_secs(6))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.transfer.as_ref().is_some_and(|p| p.finished) {
                    this.transfer = None;
                    cx.notify();
                }
            });
            let _ = id;
        }));
    }

    /// Run a drop that crosses between local disk and a mount.
    ///
    /// The router refuses these pairs on purpose — moving those bytes is a
    /// network transfer, not a rename — so the gesture is completed here
    /// instead of failing. The destination is already known, so unlike "Send
    /// to" there is nothing to ask: the drop said where.
    ///
    /// Conflicts resolve to Skip. A drop is a quick gesture and the pane it
    /// landed in may not even be visible; silently overwriting on the strength
    /// of a drag is not a trade this makes.
    pub(super) fn transfer_across_backends(
        &mut self,
        sources: Vec<VfsPath>,
        dest: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mount = match (&dest, sources.first()) {
            (VfsPath::Remote { mount, .. }, _) => *mount,
            (VfsPath::Local(_), Some(VfsPath::Remote { mount, .. })) => *mount,
            _ => return,
        };
        if matches!(dest, VfsPath::Remote { .. }) && sources.iter().any(|source| !source.is_local())
        {
            window.push_notification(
                Notification::error("A remote-to-remote drop needs two transfers, not one drag."),
                cx,
            );
            return;
        }
        if matches!(dest, VfsPath::Local(_))
            && sources
                .iter()
                .filter_map(|source| match source {
                    VfsPath::Remote { mount, .. } => Some(*mount),
                    VfsPath::Local(_) => None,
                })
                .any(|source_mount| source_mount != mount)
        {
            window.push_notification(
                Notification::error("A download cannot combine files from different mounts."),
                cx,
            );
            return;
        }
        let initiating_folder = self.active().folder.clone();
        let remote_endpoints = super::mutations::transfer_endpoints(&sources, &dest);
        let state = cx.global::<FerretState>();
        let Some(record) =
            crate::mounts::connection_for_mount(state.connections.connections(), mount).cloned()
        else {
            return;
        };
        let Some(backend) = state.mounts.backend(&record.id) else {
            window.push_notification(
                Notification::error(format!("{} isn't connected.", record.display_name())),
                cx,
            );
            return;
        };

        let job = match &dest {
            // Local selection into a remote folder.
            VfsPath::Remote { path, .. } => backend.transfer(
                Direction::Upload,
                sources
                    .iter()
                    .filter_map(|source| {
                        source
                            .as_local()
                            .map(|path| TransferSource::Local(path.to_path_buf()))
                    })
                    .collect(),
                TransferDestination::RemoteDir(path.to_string()),
                Conflict::Skip,
            ),
            // Remote selection into a local folder.
            VfsPath::Local(path) => backend.transfer(
                Direction::Download,
                sources
                    .iter()
                    .filter_map(|source| match source {
                        VfsPath::Remote { path, .. } => {
                            Some(TransferSource::Remote(path.to_string()))
                        }
                        VfsPath::Local(_) => None,
                    })
                    .collect(),
                TransferDestination::LocalDir(path.to_path_buf()),
                Conflict::Skip,
            ),
        };
        self.pump_transfer(
            job,
            record.id,
            initiating_folder,
            remote_endpoints,
            window,
            cx,
        );
    }

    /// Handle the menu's `SendToConnection`: look the record up and start.
    ///
    /// The action carries an id rather than a record because a menu is built
    /// from a list the user can edit while it is open; resolving here means a
    /// deleted connection is a no-op instead of a stale copy of its details.
    pub(super) fn send_selection_to(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let record = cx
            .global::<FerretState>()
            .connections
            .connections()
            .iter()
            .find(|connection| connection.id.0 == id)
            .cloned();
        let Some(record) = record else {
            return;
        };
        let sources = self.active().pane.read(cx).selected_paths(cx);
        if sources.is_empty() {
            return;
        }
        self.send_to_connection(record, sources, window, cx);
    }
}
