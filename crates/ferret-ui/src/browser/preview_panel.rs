//! The preview pane's place in the shell: visibility, width, the divider
//! drag, selection plumbing and the preview-local actions.
//!
//! The pane itself (`crate::preview::PreviewPanel`) owns the pipeline —
//! debounce, background reads, the editor entity. This module owns the
//! shell around it: where it sits, how wide it is, and what feeds it.
//! Visibility and width persist in `settings.preview`; the divider mirrors
//! `render_sidebar_divider`'s shape (1 px hairline, grab slop either side,
//! hover tint), because two dividers that behave differently are a bug
//! report waiting to happen.

use super::*;
use crate::preview::{PreviewInput, SelectedInput};
use gpui::Focusable as _;

/// What the unsaved-changes dialog is holding for (PV-Q02).
///
/// The selection change is blocked, not queued: while dirty, nothing else
/// moves until the user answers Save / Discard / Cancel, and only the
/// explicitly requested transition resumes afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GuardPending {
    /// The list selection already moved; re-sync it after Save / Discard.
    Reselect,
    /// Navigate to a directory after the answer.
    Navigate(VfsPath),
    Back,
    Forward,
    /// Switch to a tab after the answer.
    ActivateTab(usize),
    /// Close a tab after the answer.
    CloseTab(usize),
    /// Collapse the preview pane after the answer.
    CollapsePreview,
    /// Return focus to the file pane after the answer.
    FocusFilePane,
}

/// Window narrower than this collapses the preview, before the sidebar.
///
/// The file pane has a base minimum it is never squeezed below; the preview
/// is what gives way first (`docs/plan.html` §3). Display-only: the setting
/// keeps its value, so widening the window brings the pane back.
const COLLAPSE_UNDER: f32 = 1000.;

/// Live drag state for the preview divider.
pub(super) struct PreviewDrag {
    start_x: gpui::Pixels,
    start_width: f32,
}

impl Browser {
    /// Follow a Markdown file link clicked in the preview pane.
    ///
    /// Relative links were already lexically vetted at the click; a target
    /// that is gone lands on the existing gone handling. A dirty edit buffer
    /// guards the navigation first, through `navigate` itself.
    pub(crate) fn navigate_preview_link(
        &mut self,
        path: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate(path, window, cx);
    }

    /// Whether the preview column paints right now.
    ///
    /// The setting decides, the window width vetoes. Read in `Render` (an
    /// O(1) bounds check) and in the toggle button, so the two never
    /// disagree about what is on screen.
    pub(super) fn preview_showing(&self, window: &Window) -> bool {
        self.preview_visible && f32::from(window.bounds().size.width) >= COLLAPSE_UNDER
    }

    pub(super) fn toggle_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Collapsing over unsaved changes asks first; a resize may hide the
        // pane but must never discard its buffer, and neither may this.
        if self.preview_visible && self.preview.read(cx).is_dirty() {
            self.open_edit_guard(GuardPending::CollapsePreview, window, cx);
            return;
        }
        self.preview_visible = !self.preview_visible;
        let (visible, width) = (self.preview_visible, self.preview_width);
        let state = cx.global_mut::<FerretState>();
        state.settings.preview.visible = visible;
        state.settings.preview.width = width;
        state.touch_settings();
        crate::state::arm_config_save(cx);
        if visible {
            // Opening on a selection must show it at once, not wait for the
            // next cursor move.
            let folder = self.active().folder.clone();
            self.sync_preview(&folder, false, window, cx);
        } else {
            // Closing cancels interest; a late result must not do work for
            // a pane nobody sees.
            self.preview.update(cx, |pane, _| {
                pane.cancel();
            });
        }
        cx.notify();
    }

    /// Push the active tab's selection into the pane.
    ///
    /// Called on `SelectionChanged` and `LoadFinished` for every tab's
    /// folder, and on tab switches and navigation. Only the active tab
    /// drives the pane; background tabs are ignored. The synchronous states
    /// apply at once and the background task brings its own window handle
    /// when the bytes land; `window` here serves the unsaved-changes dialog
    /// only, so the common path still costs no window round-trip.
    ///
    /// While dirty, a selection for anything but the edited file opens the
    /// unsaved-changes dialog instead of loading: the change is blocked, not
    /// queued. Re-selecting the edited file (a watcher refresh, a cancelled
    /// guard, a tab switch back) keeps the buffer untouched.
    pub(super) fn sync_preview(
        &mut self,
        folder: &Entity<Folder>,
        reload_totals: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preview_visible {
            return;
        }
        let active = self.active().folder.clone();
        if folder.read(cx).dir != active.read(cx).dir {
            return;
        }
        if self.preview.read(cx).is_dirty() {
            let still_there = {
                let folder = folder.read(cx);
                folder.selection_count() == 1
                    && folder.selected_entry().is_some_and(|entry| {
                        self.preview
                            .read(cx)
                            .editing_path()
                            .is_some_and(|path| path == entry.path)
                    })
            };
            if !still_there {
                self.open_edit_guard(GuardPending::Reselect, window, cx);
                return;
            }
        }
        let folder = folder.read(cx);
        let count = folder.selection_count();
        let selected = if count == 0 {
            SelectedInput::None
        } else if count == 1 {
            match folder.selected_entry() {
                Some(entry) => SelectedInput::Single(entry.clone()),
                None => SelectedInput::Many {
                    count,
                    bytes: folder.selection_size(),
                    breakdown: None,
                },
            }
        } else {
            SelectedInput::Many {
                count,
                bytes: folder.selection_size(),
                breakdown: (count <= crate::preview::BREAKDOWN_CAP)
                    .then(|| selection_breakdown(folder)),
            }
        };
        // Directory totals are O(the directory): on a finished load, or
        // once per directory when the pane's cache misses — never per
        // keystroke or mouse-move. A marquee drag changes the selection on
        // every move; this must stay flat while it does. Past the row cap
        // the bytes stay unknown (the line reads "N items") rather than
        // janking the first visit of a 200,000-entry directory on the
        // window thread; summing 200k rows belongs on a background task
        // with a snapshot the folder model does not currently lend out.
        let dir = folder.dir.clone();
        let dir_bytes = (reload_totals || self.preview.read(cx).wants_totals(&dir))
            .then(|| (folder.row_count() <= DIR_TOTAL_ROW_CAP).then(|| folder_total_bytes(folder)))
            .flatten();
        let input = PreviewInput {
            dir,
            row_count: folder.row_count(),
            dir_bytes,
            selected,
        };
        self.preview.update(cx, |pane, cx| {
            pane.set_input(input, cx);
        });
    }

    /// Follow the pointer while the preview divider is held.
    ///
    /// The divider is the pane's *left* edge, so dragging left grows it:
    /// the delta is subtracted, not added. Clamped here so the pane never
    /// paints outside the bounds `ferret-settings` would accept.
    pub(super) fn update_preview_drag(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.preview_drag else {
            return;
        };
        let delta = f32::from(x - drag.start_x);
        let width = (drag.start_width - delta).clamp(
            ferret_settings::config::MIN_PREVIEW_WIDTH,
            ferret_settings::config::MAX_PREVIEW_WIDTH,
        );
        if width != self.preview_width {
            self.preview_width = width;
            cx.notify();
        }
    }

    /// Persist on release, not on every move: a drag would otherwise rewrite
    /// the settings file once per frame.
    pub(super) fn end_preview_drag(&mut self, cx: &mut Context<Self>) {
        if self.preview_drag.take().is_none() {
            return;
        }
        let width = self.preview_width;
        let state = cx.global_mut::<FerretState>();
        state.settings.preview.width = width;
        state.touch_settings();
        crate::state::arm_config_save(cx);
        cx.notify();
    }

    /// The hairline on the preview's left edge, and the handle that resizes
    /// it. Same code shape as `render_sidebar_divider`.
    pub(super) fn render_preview_divider(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let line = cx.theme().border;
        let grabbed = cx.theme().primary.opacity(0.35);
        div()
            .relative()
            .w(px(SIDEBAR_DIVIDER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(line)
            .child(
                div()
                    .id("preview-divider")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-SIDEBAR_GRAB_SLOP))
                    .right(px(-SIDEBAR_GRAB_SLOP))
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .hover(|el| el.bg(grabbed))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            this.preview_drag = Some(PreviewDrag {
                                start_x: event.position.x,
                                start_width: this.preview_width,
                            });
                            cx.notify();
                        }),
                    ),
            )
    }

    pub(super) fn focus_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preview_visible {
            self.toggle_preview(window, cx);
        }
        let handle = self.preview.read(cx).editor_focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(super) fn focus_file_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Esc returns focus to the list — but only when the preview is not
        // in edit mode with unsaved changes. Then it asks first.
        if self.preview.read(cx).is_dirty() {
            self.open_edit_guard(GuardPending::FocusFilePane, window, cx);
            return;
        }
        // A clean edit session ends where it started: Esc leaves edit mode
        // but stays in the preview, and only the next Esc returns focus.
        if self.preview.read(cx).editing_path().is_some() {
            self.preview.update(cx, |pane, cx| {
                pane.discard_edit(cx);
            });
            return;
        }
        let handle = self.active().pane.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(super) fn preview_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.open_current(window, cx);
        });
    }

    pub(super) fn preview_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.reveal_current(window, cx);
        });
    }

    pub(super) fn preview_copy_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .preview
            .read(cx)
            .previewed_path()
            .map(|path| path.to_string())
        else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem {
            entries: vec![gpui::ClipboardEntry::String(gpui::ClipboardString::new(
                path,
            ))],
        });
    }

    pub(super) fn preview_toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.toggle_wrap(window, cx);
        });
    }

    pub(super) fn preview_toggle_source(&mut self, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.toggle_source(cx);
        });
    }

    pub(super) fn preview_toggle_csv_header(&mut self, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.toggle_csv_header(cx);
        });
    }

    pub(super) fn preview_pdf_step(
        &mut self,
        delta: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview.update(cx, |pane, cx| {
            pane.pdf_step(delta, window, cx);
        });
    }

    /// Toggle the full-window preview overlay (F-75).
    ///
    /// A second `space` closes rather than stacking: the dialog layer owns
    /// the presentation and this flag owns the toggle semantics. Opens only
    /// for a single selected file — directories never open, and special
    /// files are listed, never read.
    pub(super) fn toggle_preview_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay_open {
            self.close_preview_overlay(window, cx);
            return;
        }
        // Never steal Space from the inline rename editor: it lives inside
        // the file pane's own key context, so the binding fires while typing.
        if self.active().pane.read(cx).is_renaming() {
            return;
        }
        let entry = {
            let folder = self.active().folder.read(cx);
            if folder.selection_count() != 1 {
                return;
            }
            match folder.selected_entry().cloned() {
                Some(entry)
                    if !entry.is_dir_like()
                        && !matches!(entry.kind, ferret_vfs::EntryKind::Other) =>
                {
                    entry
                }
                _ => return,
            }
        };
        // Instant when the pane already shows this file: the snapshot reuses
        // the computed value with no read at all. Otherwise the overlay runs
        // the same pipeline on demand — including with the pane closed.
        let snapshot = self
            .preview
            .read(cx)
            .overlay_snapshot(cx)
            .filter(|snapshot| snapshot.path == entry.path);
        let on_demand = snapshot.is_none();
        let browser = cx.weak_entity();
        let overlay = cx.new(|cx| PreviewOverlay::new(browser, snapshot, window, cx));
        if on_demand {
            overlay.update(cx, |overlay, cx| {
                overlay.load_on_demand(entry, cx);
            });
        }
        let focus = overlay.read(cx).focus_handle(cx);
        self.preview_overlay = Some(overlay.clone());
        self.overlay_open = true;
        let closed = cx.weak_entity();
        window.open_dialog(cx, move |dialog, window, cx| {
            window.focus(&focus, cx);
            let viewport = window.viewport_size();
            let geometry = crate::preview::overlay_geometry(
                f32::from(viewport.width),
                f32::from(viewport.height),
            );
            let on_cancel = closed.clone();
            let on_close = closed.clone();
            dialog
                .w(px(geometry.body_width))
                .margin_top(px(geometry.margin_top))
                .p_0()
                .close_button(false)
                .on_cancel(move |_, _, cx| {
                    let _ = on_cancel.update(cx, |browser, cx| {
                        browser.overlay_closed(cx);
                    });
                    true
                })
                .on_close(move |_, _, cx| {
                    let _ = on_close.update(cx, |browser, cx| {
                        browser.overlay_closed(cx);
                    });
                })
                .child(
                    div()
                        .h(px(geometry.body_height))
                        .min_h_0()
                        .overflow_hidden()
                        .child(overlay.clone()),
                )
        });
        cx.notify();
    }

    /// Close the overlay from the toggle: the dialog goes and the flag with
    /// it, so the next `space` reopens.
    pub(super) fn close_preview_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        self.overlay_closed(cx);
    }

    /// Forget a dismissed overlay. Called for every close path — toggle,
    /// `esc`, the ✕ button, backdrop dismiss — so none of them leaves a
    /// ghost the next `space` would toggle instead of opening.
    pub(crate) fn overlay_closed(&mut self, cx: &mut Context<Self>) {
        self.preview_overlay = None;
        self.overlay_open = false;
        cx.notify();
    }

    pub(super) fn preview_zoom(&mut self, direction: i32, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.zoom_image(direction, cx);
        });
    }

    pub(super) fn preview_zoom_reset(&mut self, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.zoom_image_reset(cx);
        });
    }

    pub(super) fn preview_toggle_line_numbers(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_numbers = !cx.global::<FerretState>().settings.preview.line_numbers;
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.preview.line_numbers = line_numbers;
            state.touch_settings();
        }
        crate::state::arm_config_save(cx);
        self.preview.update(cx, |pane, cx| {
            pane.set_line_numbers(line_numbers, window, cx);
        });
    }

    pub(super) fn preview_begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.begin_edit(window, cx);
        });
    }

    pub(super) fn preview_save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.save_edit(window, cx);
        });
    }

    pub(super) fn preview_revert_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview.update(cx, |pane, cx| {
            pane.revert_edit(window, cx);
        });
    }

    /// Open the unsaved-changes dialog, holding `pending` until answered.
    ///
    /// A second trigger while the dialog stands replaces the pending action
    /// rather than stacking a second copy: the dialog describes the file,
    /// not the destination, so it stays correct.
    pub(super) fn open_edit_guard(
        &mut self,
        pending: GuardPending,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preview.read(cx).is_dirty() {
            return;
        }
        // A second trigger while the dialog stands replaces the held action
        // rather than stacking a second copy of the same dialog.
        self.guard_after_save = Some(pending);
        if self.guard_open {
            return;
        }
        self.guard_open = true;
        let name = self
            .preview
            .read(cx)
            .editing_path()
            .map(|path| {
                path.file_name()
                    .map(str::to_string)
                    .unwrap_or_else(|| path.to_string())
            })
            .unwrap_or_else(|| "This file".to_string());
        let browser = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            // One answering closure per button, each with its own handle:
            // the dialog builder runs every frame the dialog stands, so a
            // single moved-from closure would not survive the second frame.
            let answer = |browser: WeakEntity<Browser>, save: Option<bool>| {
                move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                    let Some(browser) = browser.upgrade() else {
                        return;
                    };
                    browser.update(cx, |browser, cx| {
                        browser.resolve_edit_guard(save, window, cx);
                    });
                }
            };
            let save = answer(browser.clone(), Some(true));
            let discard = answer(browser.clone(), Some(false));
            let cancel = answer(browser.clone(), None);
            dialog
                .w(px(400.))
                .close_button(false)
                .title(SharedString::from("Unsaved changes"))
                .child(div().text_size(px(13.)).child(SharedString::from(format!(
                    "{name} has unsaved changes. Save them before leaving?"
                ))))
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("guard-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(cancel),
                        )
                        .child(
                            Button::new("guard-discard")
                                .ghost()
                                .label("Discard")
                                .on_click(discard),
                        )
                        .child(
                            Button::new("guard-save")
                                .primary()
                                .label("Save")
                                .on_click(save),
                        ),
                )
                .on_cancel({
                    let browser = browser.clone();
                    move |_, window, cx| {
                        let Some(browser) = browser.upgrade() else {
                            return false;
                        };
                        browser.update(cx, |browser, cx| {
                            browser.resolve_edit_guard(None, window, cx);
                        });
                        true
                    }
                })
                .on_close({
                    let browser = browser.clone();
                    move |_, _, cx| {
                        let _ = browser.update(cx, |browser, cx| {
                            // No window in hand on backdrop close: Cancel
                            // restores the selection without one.
                            browser.cancel_edit_guard(cx);
                        });
                    }
                })
        });
        cx.notify();
    }

    /// Answer the unsaved-changes dialog: `Some(true)` saves, `Some(false)`
    /// discards, `None` cancels and restores the edited selection.
    pub(super) fn resolve_edit_guard(
        &mut self,
        save: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_dialog(cx);
        self.guard_open = false;
        match save {
            None => self.cancel_edit_guard(cx),
            Some(false) => {
                self.preview.update(cx, |pane, cx| {
                    pane.discard_edit(cx);
                });
                self.take_guard_pending(window, cx);
            }
            Some(true) => {
                self.preview.update(cx, |pane, cx| {
                    pane.save_edit(window, cx);
                });
                // The save is async: `after_edit_save` continues from here.
            }
        }
    }

    /// Cancel the guard: keep the buffer, keep focus, put the selection back
    /// on the edited file. Only the explicitly requested transition ever
    /// resumes — a cancelled one restores the original selection.
    pub(super) fn cancel_edit_guard(&mut self, cx: &mut Context<Self>) {
        self.guard_open = false;
        self.guard_after_save = None;
        let Some(path) = self.preview.read(cx).editing_path() else {
            return;
        };
        let folder = self.active().folder.clone();
        let restored = folder.update(cx, |folder, _cx| {
            folder
                .row_of(&path)
                .map(|row| {
                    folder.select_only(row);
                    true
                })
                .unwrap_or(false)
        });
        if restored {
            // Re-selecting fires `SelectionChanged`, which re-syncs; the
            // pane's same-path check keeps the session, dirty flag and all.
            cx.notify();
        }
    }

    /// Continue a guarded transition after an async save settled.
    ///
    /// Called from the pane with the outcome. Success resumes the held
    /// transition; failure drops it and leaves the conflict UI standing —
    /// the buffer and focus stay intact for the user to answer there.
    pub(crate) fn after_edit_save(
        &mut self,
        success: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !success {
            self.guard_after_save = None;
            self.focus_preview(window, cx);
            return;
        }
        // The dialog closed when Save was pressed; the save held the
        // transition, and this resumes exactly the requested one. With
        // nothing held (a header-button save), the re-sync refreshes the
        // header and footer off the new mtime.
        self.take_guard_pending(window, cx);
    }

    /// Run the held transition, if any, and forget it.
    fn take_guard_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.guard_after_save.take();
        match pending {
            None | Some(GuardPending::Reselect) => {
                let folder = self.active().folder.clone();
                self.sync_preview(&folder, false, window, cx);
            }
            Some(GuardPending::Navigate(path)) => self.navigate(path, window, cx),
            Some(GuardPending::Back) => self.go_back(window, cx),
            Some(GuardPending::Forward) => self.go_forward(window, cx),
            Some(GuardPending::ActivateTab(ix)) => self.activate_tab(ix, window, cx),
            Some(GuardPending::CloseTab(ix)) => self.close_tab(ix, window, cx),
            Some(GuardPending::CollapsePreview) => self.toggle_preview(window, cx),
            Some(GuardPending::FocusFilePane) => self.focus_file_pane(window, cx),
        }
    }

    /// Buffers with unsaved changes, for quit-time recovery.
    ///
    /// The window holds at most one preview pane, so at most one entry — but
    /// the shape is a list because the binary walks windows, not panes.
    pub(crate) fn dirty_edit_buffers(&self, cx: &App) -> Vec<(VfsPath, String)> {
        self.preview
            .read(cx)
            .dirty_buffer(cx)
            .map(|(path, text)| vec![(path, text)])
            .unwrap_or_default()
    }
}

/// Sum of file sizes in the browsed directory. Directories contribute
/// nothing rather than a lie — the same rule as the status bar.
///
/// Only called for directories at or under [`DIR_TOTAL_ROW_CAP`]: the scan
/// is O(the directory) on the window thread, so huge directories keep an
/// unknown total rather than janking their first visit.
fn folder_total_bytes(folder: &Folder) -> u64 {
    (0..folder.row_count())
        .filter_map(|row| folder.entry_at(row))
        .filter_map(|entry| entry.display_len())
        .sum()
}

/// Most rows a directory may hold for its total to be summed on the window
/// thread.
///
/// `folder_total_bytes` is one `entry_at` + `display_len` per row: fast per
/// row, but 200,000 of them on the first visit of `flat/` is a visible hitch
/// on the thread that also paints. Past this the Empty line reads "N items"
/// with no bytes — honest, and the status bar (which sums incrementally off
/// the render path) still shows its own total.
const DIR_TOTAL_ROW_CAP: usize = 50_000;

/// "4 folders · 8 images · …" for a bounded multi-selection.
///
/// Image extensions group as images, directories as folders, everything else
/// as files. Single-kind selections read as one clause ("8 images"), mixed
/// ones join with middots.
fn selection_breakdown(folder: &Folder) -> String {
    let mut folders = 0usize;
    let mut images = 0usize;
    let mut files = 0usize;
    for path in folder.selected_paths_shared().iter() {
        if folder.is_dir_like(path).unwrap_or(false) {
            folders += 1;
        } else if is_image_path(path) {
            images += 1;
        } else {
            files += 1;
        }
    }
    let mut parts = Vec::with_capacity(3);
    if folders > 0 {
        parts.push(format!("{folders} folder{}", plural(folders)));
    }
    if images > 0 {
        parts.push(format!("{images} image{}", plural(images)));
    }
    if files > 0 {
        parts.push(format!("{files} file{}", plural(files)));
    }
    parts.join(" · ")
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn is_image_path(path: &VfsPath) -> bool {
    matches!(
        path.extension().as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "tif"
                | "tiff"
                | "bmp"
                | "ico"
                | "avif"
                | "heic"
                | "heif"
                | "qoi"
                | "svg"
        )
    )
}
