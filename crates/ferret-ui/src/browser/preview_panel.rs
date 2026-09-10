//! The preview pane's place in the shell: visibility, width, the divider
//! drag, selection plumbing and the preview-local actions.
//!
//! The pane itself (`crate::preview::PreviewPane`) owns the pipeline —
//! debounce, background reads, the editor entity. This module owns the
//! shell around it: where it sits, how wide it is, and what feeds it.
//! Visibility and width persist in `settings.preview`; the divider mirrors
//! `render_sidebar_divider`'s shape (1 px hairline, grab slop either side,
//! hover tint), because two dividers that behave differently are a bug
//! report waiting to happen.

use super::*;
use crate::preview::{PreviewInput, SelectedInput};
use gpui::Focusable as _;

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
    /// that is gone lands on the existing gone handling, and there is no
    /// dirty buffer to guard yet — that arrives with F-36.
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

    pub(super) fn toggle_preview(&mut self, cx: &mut Context<Self>) {
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
            self.sync_preview(&folder, false, cx);
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
    /// drives the pane; background tabs are ignored. Windowless on purpose:
    /// the synchronous states apply at once and the background task brings
    /// its own window handle when the bytes land.
    pub(super) fn sync_preview(
        &mut self,
        folder: &Entity<Folder>,
        reload_totals: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.preview_visible {
            return;
        }
        let active = self.active().folder.clone();
        if folder.read(cx).dir != active.read(cx).dir {
            return;
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
        // every move; this must stay flat while it does.
        let dir = folder.dir.clone();
        let dir_bytes = (reload_totals || self.preview.read(cx).wants_totals(&dir))
            .then(|| folder_total_bytes(folder));
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
            self.toggle_preview(cx);
        }
        let handle = self.preview.read(cx).editor_focus_handle(cx);
        window.focus(&handle, cx);
    }

    pub(super) fn focus_file_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
}

/// Sum of file sizes in the browsed directory. Directories contribute
/// nothing rather than a lie — the same rule as the status bar.
fn folder_total_bytes(folder: &Folder) -> u64 {
    (0..folder.row_count())
        .filter_map(|row| folder.entry_at(row))
        .filter_map(|entry| entry.display_len())
        .sum()
}

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
