//! The commit dialog (F-52).
//!
//! Deliberately blunt: a message and a "stage everything first" checkbox. Per
//! hunk staging is explicitly out of scope — that is a git client, not a file
//! manager, and the terminal panel is docked under this window for the cases it
//! does not cover.
//!
//! The dialog is a **view entity**, not a closure (D-46). `open_dialog` takes an
//! `Fn` builder that runs on every frame the dialog is up, so holding the
//! message or the checkbox in it would rebuild the input each frame and lose
//! the caret. The builder here mounts this entity and nothing else.

use super::*;

use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::{Cancel, Confirm};
use gpui_component::input::{Textarea, TextareaState};

/// Height of the message field.
///
/// Five rows' worth: enough for a subject, a blank line and a short body, which
/// is the shape of a good commit message. Set as a height rather than through
/// `TextareaState::rows`, which only governs the auto-grow ceiling — the field
/// renders one line tall without this.
const MESSAGE_HEIGHT: f32 = 108.;
/// Dialog width. Wider than the library's 448 default, because a commit message
/// wrapped at 448 px reads as a chat box.
const DIALOG_WIDTH: f32 = 460.;

/// What `Dialog` draws around the body, in logical pixels.
///
/// Read off `Dialog::render`: the popup pads 16 above and below and draws a
/// one-pixel border. There is no inter-child gap to account for because the body
/// is its only child — which is the reason the action buttons live in this
/// view's own content rather than in `Dialog::footer`. With them inside, one
/// measurement covers the whole dialog and the centring below is exact.
const DIALOG_CHROME: f32 = 34.;

/// Where the dialog is assumed to sit until its first frame has been measured.
///
/// Only ever used for one frame, and only the first time a window opens the
/// dialog; the height is content-driven but fixed, so every later open is
/// already exact. Measured at the default text size, so that first frame is
/// within a pixel or two rather than visibly settling.
const ESTIMATED_HEIGHT: f32 = 240.;

/// Never nearer the top of the window than this.
///
/// A window shorter than the dialog would otherwise centre it off the top edge,
/// taking the title and the message field with it. Below this the dialog stops
/// centring and simply starts at the top; the body scrolls.
const MIN_MARGIN_TOP: f32 = 24.;

pub(super) struct CommitDialog {
    message: Entity<TextareaState>,
    /// `git add -A` before committing. Checked by default: it is what the
    /// common case wants, and the alternative — a Commit button that succeeds
    /// and commits nothing — is the worst outcome available here.
    stage_all: bool,
    /// The repository name, shown under the title so a commit cannot be made
    /// into the wrong repository without the chance to notice.
    repository: SharedString,
    /// The body's painted height, written on prepaint and read by the builder
    /// that positions the dialog.
    ///
    /// Measured rather than assumed: the height depends on
    /// `appearance.ui_font_size`, so a constant would centre correctly at one
    /// text size and be visibly wrong at every other.
    height: Arc<Mutex<f32>>,
    _subscriptions: Vec<Subscription>,
}

impl CommitDialog {
    fn new(repository: SharedString, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message =
            cx.new(|cx| TextareaState::new(window, cx).placeholder("Describe what changed"));
        // Typing has to repaint the dialog, because the Commit button is
        // disabled while the message is blank and would otherwise stay
        // disabled until something else happened to cause a frame.
        let subscription = cx.subscribe(&message, |_, _, _: &InputEvent, cx| cx.notify());
        Self {
            message,
            stage_all: true,
            repository,
            height: Arc::new(Mutex::new(ESTIMATED_HEIGHT)),
            _subscriptions: vec![subscription],
        }
    }

    fn text(&self, cx: &App) -> String {
        self.message.read(cx).value().trim().to_string()
    }

    fn can_commit(&self, cx: &App) -> bool {
        !self.text(cx).is_empty()
    }
}

impl Render for CommitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let ready = self.can_commit(cx);
        let measured = self.height.clone();

        v_flex()
            .id("commit-dialog-body")
            .gap_3()
            // What the dialog's vertical position is computed from. The open
            // animation moves the popup's `top` and never its height, so this
            // is stable from the first frame.
            .on_prepaint(move |bounds, _, _| {
                *measured.lock() = f32::from(bounds.size.height);
            })
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .child(
                        Icon::new(gpui_component::IconName::CircleCheck)
                            .size_6()
                            .text_color(theme.primary),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Commit Changes"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(self.repository.clone()),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("Message"),
                    )
                    .child(Textarea::new(&self.message).h(px(MESSAGE_HEIGHT))),
            )
            .child(
                Checkbox::new("git-stage-all")
                    .label("Stage all changes first (git add -A)")
                    .checked(self.stage_all)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.stage_all = *checked;
                        cx.notify();
                    })),
            )
            // The actions live here rather than in `Dialog::footer` for two
            // reasons. They are inside the one element that is measured, which
            // is what makes `DIALOG_CHROME` a constant instead of a guess; and
            // `DialogClose`/`DialogAction` each wrap their child in a
            // `size_full` div, which stretches every button to half the footer.
            // The actions dispatched are the same ones those wrappers dispatch.
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("commit-cancel")
                            .outline()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("commit-ok")
                            .primary()
                            .label("Commit")
                            .disabled(!ready)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                            }),
                    ),
            )
    }
}

/// Where the dialog's top edge goes so that it sits in the middle of the window.
///
/// `body` is the measured height of the mounted view; the dialog's own padding
/// and border are the only things around it (see [`DIALOG_CHROME`]).
fn centred_top(body: f32, window: &Window) -> f32 {
    let viewport = f32::from(window.viewport_size().height);
    ((viewport - (body + DIALOG_CHROME)) / 2.).max(MIN_MARGIN_TOP)
}

impl Browser {
    /// Open the dialog, creating it the first time.
    ///
    /// The entity outlives each open so a message survives an accidental
    /// Escape — losing a half-written commit message to a stray keystroke is a
    /// small disaster with no undo.
    pub(super) fn open_commit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.git.repo() else {
            return;
        };
        // ⌘⇧C is a shortcut people repeat. `open_dialog` pushes onto a stack,
        // so without this a second press mounts a second copy of the same
        // dialog over the first and Cancel only dismisses one of them.
        if window.has_active_dialog(cx) {
            return;
        }
        let name = SharedString::from(repo.name().to_string());

        let dialog = match &self.commit_dialog {
            Some(existing) => {
                existing.update(cx, |dialog, cx| {
                    // The repository can have changed since the last open.
                    dialog.repository = name.clone();
                    cx.notify();
                });
                existing.clone()
            }
            None => {
                let created = cx.new(|cx| CommitDialog::new(name, window, cx));
                self.commit_dialog = Some(created.clone());
                created
            }
        };

        // The message field takes focus on open: this dialog exists to be
        // typed into, and a modal that needs a click before it accepts a
        // keystroke is a modal that gets closed again.
        use gpui::Focusable as _;
        let focus = dialog.read(cx).message.read(cx).focus_handle(cx).clone();
        window.focus(&focus, cx);

        let browser = cx.weak_entity();
        let measured = dialog.read(cx).height.clone();
        window.open_dialog(cx, move |modal, window, _cx| {
            let dialog = dialog.clone();
            let browser = browser.clone();
            modal
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                // `Dialog` centres itself horizontally and then puts its top at
                // a tenth of the viewport, because it cannot know how tall it
                // is going to be. This view does know — it measured itself last
                // frame — so it finishes the job.
                .margin_top(px(centred_top(*measured.lock(), window)))
                .child(dialog.clone())
                .on_ok(move |_, window, cx| {
                    let Some(browser) = browser.upgrade() else {
                        return true;
                    };
                    let (message, stage_all) =
                        dialog.read_with(cx, |dialog, cx| (dialog.text(cx), dialog.stage_all));
                    if message.is_empty() {
                        // Keep the dialog open rather than committing nothing.
                        return false;
                    }
                    browser.update(cx, |browser, cx| {
                        browser.git_commit(message, stage_all, window, cx);
                    });
                    true
                })
        });
        cx.notify();
    }

    /// Clear the message after a commit lands, so the next one starts empty.
    pub(super) fn clear_commit_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.commit_dialog.clone() else {
            return;
        };
        dialog.update(cx, |dialog, cx| {
            dialog.message.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        });
    }
}
