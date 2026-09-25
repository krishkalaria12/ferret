//! The operations queue's two faces: a segment in the status bar that is
//! always there when something is in flight, and a docked panel that holds the
//! whole list (F-60).
//!
//! ## Why the panel is a dock and not a dialog
//!
//! A dialog is modal, and a queue is the one thing that must never be modal: a
//! copy that takes forty minutes cannot own the window for forty minutes. So
//! the panel docks below the pane the way the terminal does, the job keeps
//! running when it is closed, and the status-bar segment keeps reporting while
//! it is.
//!
//! ## Where the work is
//!
//! None of it. Every row reads a [`Job`] snapshot the queue already holds, and
//! nothing here touches the filesystem, clones a path, or walks a listing. The
//! panel repaints on every progress event — ten or twenty a second during a
//! large copy — and the cost of that is a few dozen `AnyElement`s, which is
//! the whole reason a queue can be a live list rather than a spinner.
//!
//! The one rule that makes it safe: the queue is bounded
//! ([`ferret_ops::config::HISTORY_LIMIT`]), so the row count is a constant and
//! not a function of how long the session has been running.

use super::*;

use std::time::Duration;

use crate::typography::rem_from_ui_px;
use ferret_ops::{Job, JobId, JobKind, JobProgress, JobState};

/// Bounds on the panel, mirroring the docked terminal's geometry.
const MIN_OPERATIONS_HEIGHT: f32 = ferret_settings::config::MIN_OPERATIONS_HEIGHT;
const MAX_OPERATIONS_HEIGHT: f32 = ferret_settings::config::MAX_OPERATIONS_HEIGHT;
const OPERATIONS_HEADER_HEIGHT: f32 = 26.;
const OPERATIONS_SPLITTER_HEIGHT: f32 = 5.;

/// Row height. Generous enough that a row's two buttons are real targets
/// rather than something you have to aim at, which matters more here than
/// density: this is the row you press Cancel on.
const OPERATIONS_ROW_HEIGHT: f32 = 52.;

/// The progress bar's height. Hand-drawn, like the status-bar segment: at 4 px
/// inside a 52 px row, a component's own padding and radius would dominate the
/// thing it is showing.
const OPERATIONS_BAR_HEIGHT: f32 = 4.;

/// Bar width. Wide enough to read as a proportion rather than as a light.
const OPERATIONS_BAR_WIDTH: f32 = 132.;

/// Type size for the row's detail line, in the same terms as the status bar's
/// `CHROME_FONT_SIZE` — through [`rem_from_ui_px`], so it follows the user's UI
/// text size like everything else on screen.
const OPERATIONS_DETAIL_SIZE: f32 = 11.5;

/// How many unread wake-ups may pile up before the panel looks again.
///
/// Small, and it carries nothing: the payload is worthless on its own, since
/// the panel re-reads the queue. A deep backlog would only delay the repaint
/// that would have made it irrelevant.
const EVENT_BUFFER: usize = 8;

/// The floor between repaints while a job is running.
///
/// Half a second. The throughput figure is averaged over
/// [`ferret_ops::config::RATE_WINDOW`], so anything much faster would draw the
/// same number twice; anything much slower would let the rate and the time left
/// visibly stall on a slow disk, which is the case a progress readout exists
/// for.
const REPAINT_FLOOR: Duration = Duration::from_millis(500);

impl Browser {
    /// Show or hide the panel.
    ///
    /// Opening it while nothing is queued is allowed and shows an empty list,
    /// because the shortcut is how somebody looks for "did that copy?" long
    /// after the copy finished.
    pub(super) fn toggle_operations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.operations_visible = !self.operations_visible;
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.operations.visible = self.operations_visible;
            state.touch_settings();
        }
        crate::state::arm_config_save(cx);
        // Opened *and* focused. The panel's two shortcuts are scoped to its own
        // key context, and a context nothing is focused in is one they can never
        // reach — so a panel that opened without the keyboard would be a panel
        // whose keyboard shortcuts silently did nothing.
        if self.operations_visible {
            window.focus(&self.operations_focus, cx);
        } else {
            use gpui::Focusable as _;
            let pane_focus = self.active().pane.read(cx).focus_handle(cx);
            window.focus(&pane_focus, cx);
        }
        cx.notify();
    }

    /// Open the panel. Used by the status-bar segment, and by every gesture
    /// that starts work somebody will want to watch.
    pub(super) fn show_operations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.operations_visible {
            return;
        }
        self.toggle_operations(window, cx);
    }

    /// Watch the queue and repaint when it moves.
    ///
    /// Two tasks, each doing one thing.
    ///
    /// ## Why a channel and not a direct update
    ///
    /// The queue's observer is a plain `Fn + Send + Sync` and it runs on the
    /// copy's own thread, so it cannot touch a view — and neither can
    /// `cx.to_async()`, which is the handle that would otherwise do it, because
    /// `AsyncApp` holds an `Rc` and is not `Send`. A channel is the bridge that
    /// exists for exactly this shape: the producer sends, a task on the GPUI
    /// executor receives and wakes the window.
    ///
    /// ## Why dropping an event is fine
    ///
    /// The channel is bounded and the send is `try_send`, so a slow window
    /// loses events rather than growing a backlog. Nothing is lost with them:
    /// the receiver wakes the window and the repaint re-reads the queue, so the
    /// worst a dropped event can cost is one frame. An unbounded queue of
    /// progress readings would be the opposite trade — unbounded memory to
    /// deliver information that is stale by the time it arrives.
    ///
    /// ## Why there is a second task
    ///
    /// Progress is published every [`ferret_ops::config::PROGRESS_STEP_BYTES`],
    /// which is throttled in *bytes* so that a slow job still reports at all.
    /// A job moving at 40 KB/s therefore publishes every thirteen seconds, and
    /// the throughput and time-left beside the bar would visibly stop. A
    /// repaint driven only by events cannot fix that, because the events are the
    /// thing that is late.
    ///
    /// Racing the event against a timer in one task was the first attempt and it
    /// does not work: sequenced, the timer never elapses unless an event
    /// arrives — precisely the case it exists for — and racing them needs
    /// `FusedFuture` bounds neither future satisfies. Two tasks of five lines
    /// each is the honest shape.
    ///
    /// Held on the browser, so all three die with the window. An observer that
    /// outlived its window would keep being called for the life of the process.
    pub(super) fn start_operations_observer(&mut self, cx: &mut Context<Self>) {
        let queue = cx.global::<FerretState>().ops.clone();

        // Wake-ups. Nothing is done with the event itself: the panel re-reads
        // the queue when it repaints, so a payload carried across threads could
        // only ever be a frame out of date.
        let (sender, receiver) = async_channel::bounded::<()>(EVENT_BUFFER);
        self._operations_observer = Some(queue.observe(move |_| {
            let _ = sender.try_send(());
        }));
        self.operations_events = Some(cx.spawn(async move |this, cx| {
            loop {
                if receiver.recv().await.is_err() {
                    return;
                }
                // Drained rather than read one at a time: a burst of progress
                // events is one repaint, not twenty.
                while receiver.try_recv().is_ok() {}
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));

        // The floor under them.
        self.operations_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REPAINT_FLOOR).await;
                let alive = this.update(cx, |this, cx| {
                    // Only while there is something to read. Looking at an idle
                    // queue twice a second would cost a frame for nothing, for
                    // the life of the session.
                    if this.operations_queue(cx).is_active() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Whether anything is queued or running, and the panel's own state.
    fn operations_queue(&self, cx: &App) -> ferret_ops::Queue {
        cx.global::<FerretState>().ops.clone()
    }

    // ------------------------------------------------------------------
    // Status bar

    /// The status-bar segment, or nothing when there is nothing to say.
    ///
    /// A finished job leaves the bar at once. The segment answers "what is
    /// happening", and a result that lingers in it competes with the next
    /// thing somebody starts; `settings.operations.keep_finished` is where
    /// "did that work?" is answered instead.
    pub(super) fn render_operations_segment(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let queue = self.operations_queue(cx);
        let current = queue.current()?;
        if current.state.is_finished() {
            return None;
        }

        let theme = cx.theme();
        let palette = crate::theme::BrowserPalette::resolve(cx);
        let pending = queue.pending();
        let label = SharedString::from(current.status());
        let fraction = current.progress.fraction_of(current.kind);
        let detail = operations_detail(
            current.kind,
            current.state,
            &current.progress,
            current.failures.len(),
        );
        let color = match current.state {
            JobState::Paused => theme.muted_foreground,
            JobState::Cancelled => theme.muted_foreground,
            _ => theme.primary,
        };

        Some(
            h_flex()
                .id("operations-segment")
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .pr(px(8.))
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(cx.listener(|this, _, window, cx| this.show_operations(window, cx)))
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(260.))
                        .truncate()
                        .text_size(rem_from_ui_px(CHROME_FONT_SIZE))
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .when_some(fraction, |el, fraction| {
                    el.child(
                        div()
                            .w(px(OPERATIONS_BAR_WIDTH))
                            .h(px(OPERATIONS_BAR_HEIGHT))
                            .rounded(px(2.))
                            .bg(theme.secondary)
                            .child(
                                div()
                                    .w(gpui::relative(fraction))
                                    .h_full()
                                    .rounded(px(2.))
                                    .bg(color),
                            ),
                    )
                })
                .when(!detail.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(rem_from_ui_px(CHROME_FONT_SIZE))
                            .text_color(theme.muted_foreground)
                            // §3: the counts here change under a watcher, so
                            // proportional figures would make the segment
                            // jitter sideways as the digits change width.
                            .font_features(crate::theme::tabular_numerals())
                            .child(SharedString::from(detail)),
                    )
                })
                .when(pending > 0, |el| {
                    el.child(
                        div()
                            .text_size(rem_from_ui_px(CHROME_FONT_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(format!("· {pending} waiting"))),
                    )
                })
                .when(current.state == JobState::Paused, |el| {
                    el.child(
                        div()
                            .text_size(rem_from_ui_px(CHROME_FONT_SIZE))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(palette.git_branch)
                            .child("Paused"),
                    )
                })
                .into_any_element(),
        )
    }

    // ------------------------------------------------------------------
    // The panel

    pub(super) fn render_operations_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let queue = self.operations_queue(cx);
        let theme = cx.theme();
        // Newest first: the job somebody just started is the one they are
        // looking at, and it is the one at the top.
        let mut jobs = queue.jobs();
        jobs.reverse();

        v_flex()
            .id("operations-panel")
            // The two queue-wide shortcuts are scoped to this, and a binding
            // with no context behind it can never fire.
            .key_context(actions::context::OPERATIONS)
            .track_focus(&self.operations_focus)
            // Clicking the panel focuses it, so its two shortcuts are reachable
            // without tabbing to something. Without this the keyboard lives in
            // the panel's *buttons* only, and "focus must be visible" (§8) has
            // nothing to show on the panel itself.
            .on_mouse_down(gpui::MouseButton::Left, {
                let focus = self.operations_focus.clone();
                cx.listener(move |_, _, window, cx| window.focus(&focus, cx))
            })
            .h(px(self.operations_height))
            .min_h(px(MIN_OPERATIONS_HEIGHT))
            .max_h(px(MAX_OPERATIONS_HEIGHT))
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .child(self.render_operations_splitter(cx))
            .child(self.render_operations_header(&jobs, cx))
            .child(
                // A plain scrolling box rather than the library's
                // `Scrollable`: a queue is at most `HISTORY_LIMIT` rows, so a
                // virtual list would be machinery for a list that fits on a
                // phone screen. The `id` is what makes the box stateful, and
                // `overflow_y_scroll` is a method on the stateful element.
                // `v_flex`, not `div`: the empty state inside is `flex_1`, and
                // a block parent's child measures at its own height, so the
                // message would sit hard against the top of the panel with
                // nothing centring it.
                v_flex()
                    .id("operations-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(if jobs.is_empty() {
                        empty_state(cx.theme().clone()).into_any_element()
                    } else {
                        v_flex()
                            .children(jobs.iter().map(|job| self.render_job_row(job, cx)))
                            .into_any_element()
                    }),
            )
    }

    fn render_operations_header(
        &mut self,
        jobs: &[Job],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let active = jobs.iter().filter(|job| !job.state.is_finished()).count();
        let finished = jobs.len() - active;
        let keep_finished = cx.global::<FerretState>().settings.operations.keep_finished;

        h_flex()
            .h(px(OPERATIONS_HEADER_HEIGHT))
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px(px(10.))
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border.opacity(0.8))
            .child(
                Icon::new(gpui_component::IconName::LoaderCircle)
                    .size_4()
                    .text_color(if active > 0 {
                        theme.primary
                    } else {
                        theme.muted_foreground
                    }),
            )
            .child(
                div()
                    .text_size(rem_from_ui_px(12.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Operations"),
            )
            .child(
                div()
                    .text_size(rem_from_ui_px(CHROME_FONT_SIZE))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(if active > 0 {
                        format!("{active} running")
                    } else {
                        format!("{finished} finished")
                    })),
            )
            .child(div().flex_1())
            // "Cancel all" only while there is something to cancel. A disabled
            // button in a header is noise; so is one that is hidden while three
            // jobs are queued behind each other, which is exactly when it is
            // wanted.
            .when(active > 0, |el| {
                el.child(
                    Button::new("operations-cancel-all")
                        .ghost()
                        .small()
                        .label("Cancel All")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_all_operations(cx))),
                )
            })
            .when(keep_finished && finished > 0, |el| {
                el.child(
                    Button::new("operations-clear")
                        .ghost()
                        .small()
                        .label("Clear")
                        // A queue that forgets results the instant they land
                        // cannot answer "did that work?", so clearing is always
                        // a deliberate act and never automatic.
                        .on_click(cx.listener(|this, _, _, cx| this.clear_finished_operations(cx))),
                )
            })
            .child(
                Button::new("operations-close")
                    .ghost()
                    .small()
                    .icon(Icon::new(gpui_component::IconName::Close))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.toggle_operations(window, cx)),
                    ),
            )
    }

    /// One job.
    ///
    /// A finished job loses its bar and gains its outcome, rather than sitting
    /// at 100% forever: "Copied" and a full bar say the same thing twice, and
    /// only one of them survives the session.
    fn render_job_row(&mut self, job: &Job, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = job.id;
        let finished = job.state.is_finished();
        // Captured as a `bool`, not as a borrow of the row's job: the listener
        // outlives this call, and a closure holding `&Job` would make the whole
        // snapshot live for as long as the button does.
        let resume = job.state == JobState::Paused;
        let failed = job.state == JobState::Failed;

        let bar_colour = match job.state {
            JobState::Failed => theme.danger,
            JobState::Cancelled => theme.muted_foreground,
            JobState::Paused => theme.muted_foreground,
            _ => theme.primary,
        };

        let detail = if finished {
            job.outcome()
        } else {
            operations_detail(job.kind, job.state, &job.progress, job.failures.len())
        };

        h_flex()
            .id(SharedString::from(format!("operations-job-{id:?}")))
            .flex_shrink_0()
            .items_center()
            .gap_3()
            .h(px(OPERATIONS_ROW_HEIGHT))
            .px(px(10.))
            .border_b_1()
            .border_color(theme.border.opacity(0.4))
            .child(
                Icon::new(job_kind_icon(job.kind))
                    .size_4()
                    .flex_shrink_0()
                    .text_color(match job.state {
                        JobState::Failed => theme.danger,
                        JobState::Succeeded => theme.success,
                        JobState::Queued | JobState::Measuring => theme.muted_foreground,
                        _ => theme.foreground,
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(rem_from_ui_px(12.5))
                            .child(SharedString::from(job.label())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(rem_from_ui_px(OPERATIONS_DETAIL_SIZE))
                            .text_color(if failed {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .font_features(crate::theme::tabular_numerals())
                            .child(SharedString::from(detail)),
                    ),
            )
            .child(bar_for(job, bar_colour, cx))
            .when(!finished, |el| {
                el.child(
                    Button::new(SharedString::from(format!("operations-pause-{id:?}")))
                        .ghost()
                        .small()
                        .icon(Icon::new(if resume {
                            gpui_component::IconName::Play
                        } else {
                            gpui_component::IconName::Pause
                        }))
                        // The row's own state is the target: it is what the
                        // button is drawn as, so it is what the person pressing
                        // it means.
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.pause_operation(id, !resume, cx)
                            }),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("operations-cancel-{id:?}")))
                        .ghost()
                        .small()
                        .icon(Icon::new(gpui_component::IconName::CircleX))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.cancel_operation(id, window, cx)
                        })),
                )
            })
            .when(finished, |el| {
                el.child(
                    Button::new(SharedString::from(format!("operations-clear-{id:?}")))
                        .ghost()
                        .small()
                        .icon(Icon::new(gpui_component::IconName::Close))
                        .on_click(cx.listener(move |this, _, _, cx| this.clear_operation(id, cx))),
                )
            })
            .into_any_element()
    }

    // ------------------------------------------------------------------
    // The row's buttons

    /// Stop a job, and say so if there was nothing to stop.
    ///
    /// The toast is the point of the `Err` arm. A refused cancel — "already
    /// succeeded" — produces no visible change at all otherwise, and a button
    /// that does nothing when pressed is indistinguishable from a broken one.
    fn cancel_operation(&mut self, id: JobId, window: &mut Window, cx: &mut Context<Self>) {
        let queue = cx.global::<FerretState>().ops.clone();
        if let Err(error) = queue.cancel(id) {
            window.push_notification(Notification::error(error.to_string()), cx);
        }
        cx.notify();
    }

    fn pause_operation(&mut self, id: JobId, paused: bool, cx: &mut Context<Self>) {
        let queue = cx.global::<FerretState>().ops.clone();
        let _ = queue.set_paused(id, paused);
        cx.notify();
    }

    fn clear_operation(&mut self, id: JobId, cx: &mut Context<Self>) {
        cx.global::<FerretState>().ops.clear(id);
        cx.notify();
    }

    /// Empty the list of everything that has finished.
    pub(super) fn clear_finished_operations(&mut self, cx: &mut Context<Self>) {
        cx.global::<FerretState>().ops.clear_finished();
        cx.notify();
    }

    /// Panic button: stop what is running, drop what has not started.
    pub(super) fn cancel_all_operations(&mut self, cx: &mut Context<Self>) {
        cx.global::<FerretState>().ops.cancel_all();
        cx.notify();
    }

    fn render_operations_splitter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("operations-splitter")
            .h(px(OPERATIONS_SPLITTER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeUpDown)
            .bg(theme.border.opacity(0.55))
            .hover(|el| el.bg(cx.theme().primary.opacity(0.4)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.operations_drag = Some(OperationsDrag {
                        start_y: event.position.y,
                        start_height: this.operations_height,
                    });
                    cx.notify();
                }),
            )
    }

    pub(super) fn update_operations_drag(&mut self, y: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.operations_drag else {
            return;
        };
        let delta = f32::from(drag.start_y - y);
        let height =
            (drag.start_height + delta).clamp(MIN_OPERATIONS_HEIGHT, MAX_OPERATIONS_HEIGHT);
        if height != self.operations_height {
            self.operations_height = height;
            cx.notify();
        }
    }

    pub(super) fn end_operations_drag(&mut self, cx: &mut Context<Self>) {
        if self.operations_drag.take().is_none() {
            return;
        }
        let height = self.operations_height;
        let state = cx.global_mut::<FerretState>();
        state.settings.operations.height = height;
        state.touch_settings();
        crate::state::arm_config_save(cx);
        cx.notify();
    }
}

/// Live drag state for the panel's splitter. `pub(super)` because the root
/// follow handler in `render.rs` drives it, and it is the only thing in here
/// that leaves this module.
pub(super) struct OperationsDrag {
    pub(super) start_y: gpui::Pixels,
    pub(super) start_height: f32,
}

fn job_kind_icon(kind: JobKind) -> gpui_component::IconName {
    match kind {
        JobKind::Copy => gpui_component::IconName::Copy,
        JobKind::Move => gpui_component::IconName::ArrowRight,
        JobKind::Trash | JobKind::Delete => gpui_component::IconName::Delete,
    }
}

/// The bar, or a fixed-width spacer where one would be.
///
/// The spacer is deliberate: a row whose bar appears and disappears shifts
/// every control to its right, so a person reaching for Cancel on a finished
/// job presses a clear button instead. `HISTORY_LIMIT` rows of stable geometry
/// beat a bar that only sometimes exists.
fn bar_for(job: &Job, colour: gpui::Hsla, cx: &App) -> AnyElement {
    let theme = cx.theme();
    if job.state.is_finished() {
        return div()
            .w(px(OPERATIONS_BAR_WIDTH))
            .flex_shrink_0()
            .into_any_element();
    }
    let fraction = job.progress.fraction_of(job.kind);
    div()
        .w(px(OPERATIONS_BAR_WIDTH))
        .flex_shrink_0()
        .h(px(OPERATIONS_BAR_HEIGHT))
        .rounded(px(2.))
        .bg(theme.secondary)
        .child(
            div()
                // A job with no measured total draws no fill rather than a
                // spinner. The detail line beside it carries the byte count,
                // which is the number that is actually true.
                .when_some(fraction, |el, fraction| {
                    el.w(gpui::relative(fraction))
                        .h_full()
                        .rounded(px(2.))
                        .bg(colour)
                }),
        )
        .into_any_element()
}

/// Throughput and time left, for a job that is moving.
///
/// Empty for a job that is not, and empty for one whose rate is not yet worth
/// quoting: `JobProgress::remaining` refuses to divide a total by the opening
/// chunk's speed, and a panel that printed the result anyway would be
/// predicting "a fortnight" on a copy that takes nine seconds.
fn operations_detail(
    kind: JobKind,
    state: JobState,
    progress: &JobProgress,
    failures: usize,
) -> String {
    if state == JobState::Measuring {
        return "Measuring…".to_string();
    }
    let mut parts = Vec::new();
    if kind.moves_bytes() {
        if let Some(total) = progress.bytes_total
            && total > 0
        {
            parts.push(format!(
                "{} of {}",
                crate::format::format_size(progress.bytes_done),
                crate::format::format_size(total)
            ));
        }
        if progress.bytes_per_second > 0 {
            parts.push(format!(
                "{}/s",
                crate::format::format_size(progress.bytes_per_second)
            ));
        }
        // Also only here. The rate behind this is sampled from bytes, and a
        // delete's remaining file count divided by a bytes-per-second figure
        // would be a number nobody could act on.
        if let Some(left) = progress.remaining() {
            parts.push(format!("{} left", short_duration(left)));
        }
    } else if let Some(total) = progress.entries_total
        && total > 0
    {
        // A delete's cost is files, not bytes: "12,304 of 40,000 files" is the
        // number that says whether the disk is busy, where a byte count would be
        // describing something a delete never touches.
        parts.push(format!(
            "{} of {} files",
            group_digits(progress.entries_done),
            group_digits(total)
        ));
    }
    if failures > 0 {
        parts.push(format!("{failures} failed"));
    }
    parts.join(" · ")
}

/// `12304` as `12,304`.
///
/// Written out rather than pulled in: one loop, no locale, and Ferret's other
/// counts are not locale-formatted either — a status bar that changes shape
/// with the system language is a status bar whose width nobody can predict.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A duration as "4s", "12m", "1h 20m".
///
/// Coarse on purpose. A remaining time that ticks every second is noise, and
/// the bar beside it is what actually communicates progress; this line exists
/// to answer "is this going to take a coffee break", and that does not need
/// seconds.
fn short_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

fn empty_state(theme: gpui_component::Theme) -> impl IntoElement {
    div()
        .flex_1()
        .items_center()
        .justify_center()
        .px(px(20.))
        .text_size(rem_from_ui_px(12.))
        .text_color(theme.muted_foreground)
        .text_center()
        // Narrower than the panel, so the sentence reads as a sentence rather
        // than as a banner stretched across the window.
        .max_w(px(360.))
        .child("No operations yet. Copies, moves and deletes appear here while they run.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_reads_as_the_question_it_answers() {
        assert_eq!(short_duration(Duration::from_secs(9)), "9s");
        assert_eq!(short_duration(Duration::from_secs(90)), "1m");
        assert_eq!(short_duration(Duration::from_secs(59 * 60)), "59m");
        assert_eq!(short_duration(Duration::from_secs(80 * 60)), "1h 20m");
    }

    #[test]
    fn a_measuring_job_says_so_rather_than_showing_a_bar_at_zero() {
        let measuring = JobProgress::default();
        assert_eq!(
            operations_detail(JobKind::Copy, JobState::Measuring, &measuring, 0),
            "Measuring…"
        );

        // And once it is running with nothing measured, the line is empty
        // rather than a confident wrong number.
        assert_eq!(
            operations_detail(JobKind::Copy, JobState::Running, &measuring, 0),
            ""
        );
    }

    #[test]
    fn a_running_copy_reports_bytes_rate_and_time_left() {
        let progress = JobProgress {
            bytes_done: 50 << 20,
            bytes_total: Some(100 << 20),
            bytes_per_second: 10 << 20,
            entries_done: 1,
            entries_total: Some(1),
        };
        assert_eq!(
            operations_detail(JobKind::Copy, JobState::Running, &progress, 0),
            "50 MB of 100 MB · 10 MB/s · 5s left"
        );
    }

    #[test]
    fn a_delete_reports_files_because_that_is_what_it_removes() {
        // The same byte figures on a delete would read as a transfer of 50 MB
        // that never happened, so the byte half is not merely empty for a
        // delete — it is unreachable.
        let progress = JobProgress {
            bytes_done: 50 << 20,
            bytes_total: Some(100 << 20),
            bytes_per_second: 10 << 20,
            entries_done: 12_304,
            entries_total: Some(40_000),
        };
        assert_eq!(
            operations_detail(JobKind::Delete, JobState::Running, &progress, 0),
            "12,304 of 40,000 files"
        );
    }

    #[test]
    fn counts_are_grouped_because_five_digits_are_hard_to_read() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(12_304), "12,304");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }

    #[test]
    fn a_partial_failure_is_counted_rather_than_hidden() {
        let progress = JobProgress {
            bytes_done: 100,
            bytes_total: Some(200),
            ..Default::default()
        };
        assert_eq!(
            operations_detail(JobKind::Copy, JobState::Succeeded, &progress, 4),
            "100 B of 200 B · 4 failed"
        );
    }
}
