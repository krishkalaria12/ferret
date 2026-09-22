//! PDF page navigation (F-34): page one plus Previous / Next.
///
/// The pipeline renders page one through the bundled backend; turning a page
/// re-renders from the retained bytes, never re-reads the file. Page requests
/// carry the pane generation (a new selection drops the turn) and a page
/// serial (a newer turn drops an older one). Only the current page is
/// retained — continuous scrolling is the documented follow-up, not this.
use super::*;
use gpui::{IntoElement, ParentElement as _, SharedString, Styled as _, Window, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};

impl PreviewPanel {
    /// Step the current page by `delta` (±1 from the footer and the menu).
    pub(crate) fn pdf_step(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let current = match &self.state {
            PaneState::Pdf { pages, .. } => pages.current,
            _ => return,
        };
        self.request_pdf_page(current.saturating_add_signed(delta), window, cx);
    }

    /// Render a 1-based page and swap it in when it lands.
    ///
    /// Out-of-range pages, the current page, and a second turn while one is
    /// in flight are all no-ops: the footer buttons disable themselves for
    /// the same states, so this guard is the backstop, not the UI.
    pub(crate) fn request_pdf_page(
        &mut self,
        page: u32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (pdf, pages) = match &self.state {
            PaneState::Pdf {
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
        if let PaneState::Pdf {
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
            PaneState::Pdf { pdf_gen, .. } => *pdf_gen,
            _ => return,
        };
        let generation = self.generation;
        let window_handle = self.window;
        cx.notify();
        // The render borrows through the share: turning a page costs no
        // file-sized copy, just one `Arc` clone into the blocking task.
        self.tasks.push(cx.spawn(async move |pane, cx| {
            let rendered = pdfium_page(pdf, page - 1).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = pane.update(cx, |pane, cx| {
                    if pane.generation != generation {
                        return;
                    }
                    pane.apply_pdf_page(pdf_gen, page, rendered, window, cx);
                });
            });
        }));
    }

    /// Swap a settled page turn into the pane.
    ///
    /// A generation mismatch means the selection moved on: the new state owns
    /// the pane and the late bitmap is dropped. A serial mismatch means a
    /// newer turn superseded this one. Anything else settles the loading
    /// flag — a failed turn keeps the old page with a note, never a blank.
    fn apply_pdf_page(
        &mut self,
        pdf_gen: u64,
        page: u32,
        rendered: Result<RenderedPage, PdfUnavailable>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A newer turn superseded this one, or the selection moved on and a
        // new state owns the pane: either way the late bitmap is dropped.
        if !matches!(&self.state, PaneState::Pdf { pdf_gen: serial, .. } if *serial == pdf_gen) {
            return;
        }
        match rendered {
            Ok(page_render) => match render_bitmap(&page_render.bitmap) {
                Some(render_image) => {
                    if let PaneState::Pdf {
                        render_image: slot,
                        bitmap_size,
                        pages,
                        loading_page,
                        page_error,
                        zoom,
                        ..
                    } = &mut self.state
                    {
                        *slot = render_image;
                        *bitmap_size = (page_render.bitmap.width, page_render.bitmap.height);
                        pages.current = page;
                        *loading_page = false;
                        *page_error = None;
                        // A new page is a new picture: fit it before judging.
                        *zoom = 0.0;
                    }
                }
                None => {
                    if let PaneState::Pdf {
                        loading_page,
                        page_error,
                        ..
                    } = &mut self.state
                    {
                        *loading_page = false;
                        *page_error = Some(format!("Page {page} didn't fit its frame."));
                    }
                }
            },
            Err(_) => {
                if let PaneState::Pdf {
                    loading_page,
                    page_error,
                    ..
                } = &mut self.state
                {
                    *loading_page = false;
                    *page_error = Some(format!("Couldn't render page {page}."));
                }
            }
        }
        cx.notify();
    }

    /// The PDF footer: page position, loading state, and the last failure.
    ///
    /// The footer only exists when it has something to say, and "Page 1 of
    /// 9" always qualifies — it is the one-line proof the document is paged.
    pub(super) fn render_pdf_bar(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (current, total, loading, error) = match &self.state {
            PaneState::Pdf {
                pages,
                loading_page,
                page_error,
                ..
            } => (
                pages.current,
                pages.total,
                *loading_page,
                page_error.clone(),
            ),
            _ => return None,
        };
        let mut label = format!("Page {current} of {total}.");
        if loading {
            label.push_str(" Loading…");
        }
        if let Some(note) = error {
            label.push(' ');
            label.push_str(&note);
        }
        Some(
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap_2()
                .px(px(12.))
                .py(px(4.))
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    Button::new("pdf-prev")
                        .small()
                        .ghost()
                        .label("‹ Prev")
                        .disabled(current <= 1 || loading)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.request_pdf_page(current.saturating_sub(1), window, cx);
                        })),
                )
                .child(
                    gpui::div()
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(label)),
                )
                .child(
                    Button::new("pdf-next")
                        .small()
                        .ghost()
                        .label("Next ›")
                        .disabled(current >= total || loading)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.request_pdf_page(current.saturating_add(1), window, cx);
                        })),
                ),
        )
    }
}
