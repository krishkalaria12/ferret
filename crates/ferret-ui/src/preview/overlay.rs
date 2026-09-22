//! Space-to-preview (F-75): the same providers on a full-window overlay.
//!
//! The docked pane and the overlay share providers, never pixels: the overlay
//! opens instantly with no read at all when the pane already shows the
//! selected file, and runs the same pipeline on demand when it does not —
//! including with the pane closed. Either way the content is the computed
//! value, so the two surfaces cannot disagree, and the overlay works
//! identically on Windows where there is no Quick Look panel to defer to.
//!
//! The overlay holds no text fields, so the `space` binding that opens it
//! closes it too without ever stealing a keystroke from an input. `esc`
//! closes through the dialog's own cancel path.

use super::render::handle_markdown_link;
use super::*;
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::actions::context;

/// Preferred overlay size: a real reading surface at the default window,
/// clamped into smaller viewports by [`overlay_geometry`].
const OVERLAY_WIDTH: f32 = 1020.;
const OVERLAY_HEIGHT: f32 = 640.;
const MIN_MARGIN_VERTICAL: f32 = 24.;
const MIN_MARGIN_HORIZONTAL: f32 = 16.;

/// What the pane hands the overlay when it already shows the file.
pub(crate) struct OverlaySnapshot {
    pub path: VfsPath,
    pub title: String,
    pub detail: String,
    pub body: SnapshotBody,
}

/// The pane's computed value in overlay-ready shape: shared bitmaps cross
/// by `Arc`, text by one bounded clone per opening — never a re-read.
pub(crate) enum SnapshotBody {
    Text {
        text: String,
        language: SmolStr,
        wrap: bool,
        footer: Option<String>,
    },
    Markdown {
        sanitized: String,
        base: Option<PathBuf>,
        remote: bool,
        footer: Option<String>,
    },
    Image {
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        footer: Option<String>,
    },
    Table {
        table: std::sync::Arc<ParsedTable>,
        footer: Option<String>,
    },
    Pdf {
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        pdf: std::sync::Arc<Vec<u8>>,
        pages: Pages,
    },
}

pub(crate) struct PreviewOverlay {
    browser: gpui::WeakEntity<Browser>,
    focus: FocusHandle,
    window: gpui::AnyWindowHandle,
    editor: Entity<EditorState>,
    docview: Entity<TextViewState>,
    grid: Option<CsvGrid>,
    state: OverlayState,
    generation: u64,
    tasks: Vec<Task<()>>,
}

enum OverlayState {
    Loading {
        name: String,
    },
    Text {
        title: String,
        detail: String,
        footer: Option<String>,
    },
    Markdown {
        title: String,
        detail: String,
        base: Option<PathBuf>,
        remote: bool,
        footer: Option<String>,
    },
    Image {
        title: String,
        detail: String,
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        footer: Option<String>,
    },
    Table {
        title: String,
        detail: String,
        footer: Option<String>,
    },
    Pdf {
        title: String,
        detail: String,
        render_image: std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        pdf: std::sync::Arc<Vec<u8>>,
        pages: Pages,
        pdf_gen: u64,
        loading_page: bool,
        page_error: Option<String>,
    },
    Message {
        title: String,
        message: String,
    },
}

impl PreviewOverlay {
    pub(crate) fn new(
        browser: gpui::WeakEntity<Browser>,
        snapshot: Option<OverlaySnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("plain")
                .line_number(false)
                .soft_wrap(true)
        });
        let docview = cx.new(|cx| TextViewState::markdown("", cx));
        let mut overlay = Self {
            browser,
            focus: cx.focus_handle(),
            window: window.window_handle(),
            editor,
            docview,
            grid: None,
            state: OverlayState::Loading {
                name: String::new(),
            },
            generation: 0,
            tasks: Vec::new(),
        };
        // The constructor holds the window, so the instant path fills the
        // views here: by the first frame the content is already standing.
        if let Some(snapshot) = snapshot {
            overlay.apply_snapshot(snapshot, window, cx);
        }
        overlay
    }

    /// Fill the overlay from the pane's computed value: no read at all.
    pub(crate) fn apply_snapshot(
        &mut self,
        snapshot: OverlaySnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let OverlaySnapshot {
            title,
            detail,
            body,
            ..
        } = snapshot;
        match body {
            SnapshotBody::Text {
                text,
                language,
                wrap,
                footer,
            } => {
                self.editor.update(cx, |editor, cx| {
                    editor.set_highlighter(gpui::SharedString::from(registry_name(&language)), cx);
                    editor.set_soft_wrap(wrap, window, cx);
                    editor.set_value(text, window, cx);
                });
                self.state = OverlayState::Text {
                    title,
                    detail,
                    footer,
                };
            }
            SnapshotBody::Markdown {
                sanitized,
                base,
                remote,
                footer,
            } => {
                self.docview.update(cx, |docview, cx| {
                    docview.set_text(&sanitized, cx);
                });
                self.state = OverlayState::Markdown {
                    title,
                    detail,
                    base,
                    remote,
                    footer,
                };
            }
            SnapshotBody::Image {
                render_image,
                bitmap_size,
                footer,
            } => {
                self.state = OverlayState::Image {
                    title,
                    detail,
                    render_image,
                    bitmap_size,
                    footer,
                };
            }
            SnapshotBody::Table { table, footer } => {
                // Widths remeasured from the retained sample: O(the sample)
                // once per opening, never per frame. The grid holds the rows
                // from here; the sample itself stays with the pane.
                let widths = grid::column_widths(&table);
                self.grid = Some(CsvGrid::new(&table, widths, window, cx));
                self.state = OverlayState::Table {
                    title,
                    detail,
                    footer,
                };
            }
            SnapshotBody::Pdf {
                render_image,
                bitmap_size,
                pdf,
                pages,
            } => {
                self.state = OverlayState::Pdf {
                    title,
                    detail,
                    render_image,
                    bitmap_size,
                    pdf,
                    pages,
                    pdf_gen: 0,
                    loading_page: false,
                    page_error: None,
                };
            }
        }
        cx.notify();
    }

    /// Run the same pipeline on demand: the pane had nothing fresh to reuse
    /// — closed, loading, or showing another file.
    pub(crate) fn load_on_demand(&mut self, entry: Entry, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.tasks.clear();
        self.state = OverlayState::Loading {
            name: entry.name().to_string(),
        };
        cx.notify();

        let fs = cx.global::<FerretState>().fs;
        let thumbs = cx.global::<FerretState>().thumb_cache.clone();
        let remote = !entry.path.is_local();
        let window_handle = self.window;
        self.tasks.push(cx.spawn(async move |overlay, cx| {
            let outcome = load_single(&fs, &thumbs, &entry, remote).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = overlay.update(cx, |overlay, cx| {
                    if overlay.generation != generation {
                        return;
                    }
                    overlay.apply_outcome(outcome, window, cx);
                });
            });
        }));
    }

    /// Swap a loaded value into the overlay's own views.
    fn apply_outcome(&mut self, outcome: LoadOutcome, window: &mut Window, cx: &mut Context<Self>) {
        match outcome {
            LoadOutcome::Text {
                entry,
                detail,
                text,
                language,
                wrap,
                footer,
                ..
            } => {
                self.editor.update(cx, |editor, cx| {
                    editor.set_highlighter(gpui::SharedString::from(registry_name(&language)), cx);
                    editor.set_soft_wrap(wrap, window, cx);
                    editor.set_value(text, window, cx);
                });
                self.state = OverlayState::Text {
                    title: PreviewPane::display_name(entry.name()),
                    detail,
                    footer,
                };
            }
            LoadOutcome::Markdown {
                entry,
                detail,
                sanitized,
                footer,
                base,
                remote,
                ..
            } => {
                self.docview.update(cx, |docview, cx| {
                    docview.set_text(&sanitized, cx);
                });
                self.state = OverlayState::Markdown {
                    title: PreviewPane::display_name(entry.name()),
                    detail,
                    base,
                    remote,
                    footer,
                };
            }
            LoadOutcome::Table {
                entry,
                detail,
                table,
                widths,
                footer,
            } => {
                let table = std::sync::Arc::new(table);
                self.grid = Some(CsvGrid::new(&table, widths, window, cx));
                self.state = OverlayState::Table {
                    title: PreviewPane::display_name(entry.name()),
                    detail,
                    footer,
                };
            }
            LoadOutcome::Pdf {
                entry,
                detail,
                pdf,
                page,
            } => {
                // Page one arrives rendered from the shared pipeline; the
                // overlay keeps the bytes so its own page turns re-render
                // from memory. No blocking call on the window thread.
                match render_bitmap(&page.bitmap) {
                    Some(render_image) => {
                        self.state = OverlayState::Pdf {
                            title: PreviewPane::display_name(entry.name()),
                            detail,
                            bitmap_size: (page.bitmap.width, page.bitmap.height),
                            render_image,
                            pdf,
                            pages: Pages {
                                current: 1,
                                total: page.pages_total,
                            },
                            pdf_gen: 0,
                            loading_page: false,
                            page_error: None,
                        };
                    }
                    None => {
                        self.state = OverlayState::Message {
                            title: PreviewPane::display_name(entry.name()),
                            message: "Ferret couldn't render this PDF here.".to_string(),
                        };
                    }
                }
            }
            LoadOutcome::Image {
                entry,
                detail,
                image,
                footer,
            } => match render_image(&image) {
                Some(render_image) => {
                    let first = &image.frames[0];
                    self.state = OverlayState::Image {
                        title: PreviewPane::display_name(entry.name()),
                        detail,
                        bitmap_size: (first.bitmap.width, first.bitmap.height),
                        render_image,
                        footer,
                    };
                }
                None => {
                    self.state = OverlayState::Message {
                        title: PreviewPane::display_name(entry.name()),
                        message: "Ferret couldn't draw these pixels.".to_string(),
                    };
                }
            },
            LoadOutcome::Unsupported {
                entry,
                detail,
                message,
            } => {
                self.state = OverlayState::Message {
                    title: format!("{} · {detail}", PreviewPane::display_name(entry.name())),
                    message,
                };
            }
            LoadOutcome::Error { name, message } => {
                self.state = OverlayState::Message {
                    title: PreviewPane::display_name(&name),
                    message,
                };
            }
        }
        cx.notify();
    }

    /// Turn one overlay PDF page, from the retained bytes.
    fn request_page(&mut self, page: u32, cx: &mut Context<Self>) {
        let (pdf, pages) = match &self.state {
            OverlayState::Pdf {
                pdf,
                pages,
                loading_page,
                ..
            } if !loading_page => (pdf.clone(), *pages),
            _ => return,
        };
        if page < 1 || page > pages.total || page == pages.current {
            return;
        }
        if let OverlayState::Pdf {
            pdf_gen,
            loading_page,
            page_error,
            ..
        } = &mut self.state
        {
            *pdf_gen += 1;
            *loading_page = true;
            *page_error = None;
        }
        let pdf_gen = match &self.state {
            OverlayState::Pdf { pdf_gen, .. } => *pdf_gen,
            _ => return,
        };
        let generation = self.generation;
        let window_handle = self.window;
        cx.notify();
        self.tasks.push(cx.spawn(async move |overlay, cx| {
            let rendered = pdfium_page(pdf, page - 1).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = overlay.update(cx, |overlay, cx| {
                    if overlay.generation != generation {
                        return;
                    }
                    if !matches!(&overlay.state, OverlayState::Pdf { pdf_gen: serial, .. } if *serial == pdf_gen)
                    {
                        return;
                    }
                    match rendered {
                        Ok(page_render) => match render_bitmap(&page_render.bitmap) {
                            Some(render_image) => {
                                if let OverlayState::Pdf {
                                    render_image: slot,
                                    bitmap_size,
                                    pages,
                                    loading_page,
                                    page_error,
                                    ..
                                } = &mut overlay.state
                                {
                                    *slot = render_image;
                                    *bitmap_size =
                                        (page_render.bitmap.width, page_render.bitmap.height);
                                    pages.current = page;
                                    *loading_page = false;
                                    *page_error = None;
                                }
                            }
                            None => {
                                if let OverlayState::Pdf {
                                    loading_page,
                                    page_error,
                                    ..
                                } = &mut overlay.state
                                {
                                    *loading_page = false;
                                    *page_error =
                                        Some(format!("Page {page} didn't fit its frame."));
                                }
                            }
                        },
                        Err(_) => {
                            if let OverlayState::Pdf {
                                loading_page,
                                page_error,
                                ..
                            } = &mut overlay.state
                            {
                                *loading_page = false;
                                *page_error = Some(format!("Couldn't render page {page}."));
                            }
                        }
                    }
                    let _ = window;
                    cx.notify();
                });
            });
        }));
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, detail) = match &self.state {
            OverlayState::Loading { name } => {
                (PreviewPane::display_name(name), "Loading…".to_string())
            }
            OverlayState::Text { title, detail, .. }
            | OverlayState::Markdown { title, detail, .. }
            | OverlayState::Image { title, detail, .. }
            | OverlayState::Table { title, detail, .. }
            | OverlayState::Pdf { title, detail, .. } => (title.clone(), detail.clone()),
            OverlayState::Message { title, .. } => (title.clone(), "Preview".to_string()),
        };
        let browser = self.browser.clone();
        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(detail)),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from("space · esc to close")),
            )
            .child(
                Button::new("overlay-close")
                    .ghost()
                    .icon(Icon::new(gpui_component::IconName::Close))
                    .tooltip("Close preview")
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let _ = browser.update(cx, |browser, cx| {
                            browser.overlay_closed(cx);
                        });
                    }),
            )
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.state {
            OverlayState::Loading { .. } => self.render_centred("Loading…".to_string(), cx),
            OverlayState::Text { .. } => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .px(px(8.))
                .child(Editor::new(&self.editor).readonly(true).bordered(false))
                .into_any_element(),
            OverlayState::Markdown { base, remote, .. } => {
                let browser = self.browser.clone();
                let base = base.clone();
                let remote = *remote;
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px(px(20.))
                    .py(px(12.))
                    .child(TextView::new(&self.docview).selectable(true).on_link_click(
                        move |url, _, window, cx| {
                            handle_markdown_link(&browser, base.clone(), remote, url, window, cx);
                        },
                    ))
                    .into_any_element()
            }
            OverlayState::Image {
                render_image,
                bitmap_size,
                ..
            } => self.render_image_fit(render_image, *bitmap_size, cx),
            OverlayState::Table { .. } => match &self.grid {
                Some(grid) => div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(grid.element())
                    .into_any_element(),
                None => self.render_centred("No rows to show.".to_string(), cx),
            },
            OverlayState::Pdf {
                render_image,
                bitmap_size,
                ..
            } => self.render_image_fit(render_image, *bitmap_size, cx),
            OverlayState::Message { message, .. } => self.render_centred(message.clone(), cx),
        }
    }

    /// Fit-to-overlay at any window size: the overlay is already the large
    /// surface, so fixed zoom lives in the docked pane, not here.
    fn render_image_fit(
        &self,
        render_image: &std::sync::Arc<gpui::RenderImage>,
        _bitmap_size: (u32, u32),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let image = gpui::img(gpui::ImageSource::from(render_image.clone()));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .bg(cx.theme().secondary)
            .p(px(16.))
            .child(image.w_full().max_h_full())
            .into_any_element()
    }

    fn render_centred(&self, text: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.))
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_size(px(13.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(text)),
            )
            .into_any_element()
    }

    fn render_bar(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        match &self.state {
            OverlayState::Text { footer, .. }
            | OverlayState::Markdown { footer, .. }
            | OverlayState::Image { footer, .. }
            | OverlayState::Table { footer, .. } => footer.clone().map(|footer| {
                div()
                    .flex_shrink_0()
                    .px(px(16.))
                    .py(px(6.))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(footer))
                    .into_any_element()
            }),
            OverlayState::Pdf {
                pages,
                loading_page,
                page_error,
                ..
            } => {
                let current = pages.current;
                let total = pages.total;
                let loading = *loading_page;
                let mut label = format!("Page {current} of {total}.");
                if loading {
                    label.push_str(" Loading…");
                }
                if let Some(note) = page_error.clone() {
                    label.push(' ');
                    label.push_str(&note);
                }
                Some(
                    h_flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap_2()
                        .px(px(16.))
                        .py(px(4.))
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            Button::new("overlay-pdf-prev")
                                .small()
                                .ghost()
                                .label("‹ Prev")
                                .disabled(current <= 1 || loading)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.request_page(current.saturating_sub(1), cx);
                                })),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(label)),
                        )
                        .child(
                            Button::new("overlay-pdf-next")
                                .small()
                                .ghost()
                                .label("Next ›")
                                .disabled(current >= total || loading)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.request_page(current.saturating_add(1), cx);
                                })),
                        )
                        .into_any_element(),
                )
            }
            OverlayState::Loading { .. } | OverlayState::Message { .. } => None,
        }
    }
}

impl Focusable for PreviewOverlay {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PreviewOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The dialog owns the surface — this fills it, the way the power
        // search dialog's content does. No second border, no second radius.
        v_flex()
            .id("preview-overlay")
            .key_context(context::OVERLAY)
            .size_full()
            .overflow_hidden()
            .child(self.render_header(cx))
            .child(self.render_body(cx))
            .children(self.render_bar(cx))
    }
}

/// The overlay's viewport geometry: nearly the window, with room to see it
/// is an overlay — the backdrop dimming does half that work already.
pub(crate) struct OverlayGeometry {
    pub body_width: f32,
    pub body_height: f32,
    pub margin_top: f32,
}

pub(crate) fn overlay_geometry(viewport_width: f32, viewport_height: f32) -> OverlayGeometry {
    let body_width = OVERLAY_WIDTH.min((viewport_width - MIN_MARGIN_HORIZONTAL * 2.).max(0.));
    let body_height = OVERLAY_HEIGHT.min((viewport_height - MIN_MARGIN_VERTICAL * 2.).max(0.));
    let margin_top = ((viewport_height - body_height) / 2.).max(MIN_MARGIN_VERTICAL);
    OverlayGeometry {
        body_width,
        body_height,
        margin_top,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_geometry_fills_the_default_window() {
        let geometry = overlay_geometry(1180., 760.);
        assert_eq!(geometry.body_width, OVERLAY_WIDTH);
        assert_eq!(geometry.body_height, OVERLAY_HEIGHT);
        assert_eq!(geometry.margin_top, (760. - OVERLAY_HEIGHT) / 2.);
    }

    #[test]
    fn overlay_geometry_clamps_into_the_minimum_window() {
        let geometry = overlay_geometry(760., 480.);
        assert_eq!(geometry.body_width, 760. - MIN_MARGIN_HORIZONTAL * 2.);
        assert_eq!(geometry.body_height, 480. - MIN_MARGIN_VERTICAL * 2.);
        assert_eq!(geometry.margin_top, MIN_MARGIN_VERTICAL);
        assert!(geometry.body_width > 0.);
        assert!(geometry.body_height > 0.);
    }
}
