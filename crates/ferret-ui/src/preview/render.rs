//! The preview body: one arm per state, plus the footer that only exists
//! when it has something to say.
//!
//! `Render` matches on the [`PaneState`] and builds elements. It does not
//! clone the text, re-derive a language, re-scan for line endings or rebuild
//! anything proportional to the file — the allocation-budget gate applies to
//! this pane exactly as it does to the file pane.

use super::*;
use gpui::{
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::actions::context;

impl PreviewPanel {
    /// The whole pane: header, body, and a footer only when it has something
    /// to say. An always-present status strip becomes furniture nobody reads.
    pub(super) fn render_pane(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let bg = cx.theme().background;
        let mut footer = match &self.state {
            PaneState::Ready { footer, .. }
            | PaneState::ReadyMd { footer, .. }
            | PaneState::Image { footer, .. }
            | PaneState::Table { footer, .. } => footer.clone(),
            _ => None,
        };
        // A refused save is the footer with something to say: the conflict
        // names what happened and what to do, beside the buffer it kept.
        // (Conflict is a rare state, not steady-state frames, so formatting
        // here does not touch the allocations budget.)
        if let Some(conflict) = self.edit_conflict() {
            footer = Some(match footer {
                Some(existing) => SharedString::from(format!("{existing} {conflict}")),
                None => SharedString::from(conflict),
            });
        }
        v_flex()
            .id("preview-panel")
            .key_context(context::PREVIEW)
            .track_focus(&self.focus)
            .size_full()
            .bg(bg)
            .border_l_1()
            .border_color(cx.theme().border)
            .child(self.render_header(cx))
            .child(self.render_body(cx))
            // The PDF bar carries page position in the footer's slot: one
            // muted line, and the only interactive footer in the pane.
            .children(self.render_pdf_bar(cx))
            .when_some(footer, |el, footer| {
                el.child(self.render_footer(footer, cx))
            })
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            PaneState::Empty { rows, bytes, .. } => self.render_centred(
                match bytes {
                    Some(bytes) => format!("{rows} items · {}", format_size(*bytes)),
                    None => format!("{rows} items"),
                },
                cx,
            ),
            PaneState::Multiple {
                count,
                bytes,
                breakdown,
            } => {
                let mut text = format!("{count} items · {}", format_size(*bytes));
                if let Some(breakdown) = breakdown {
                    text.push_str(" · ");
                    text.push_str(breakdown);
                }
                self.render_centred(text, cx)
            }
            PaneState::Directory { count, .. } => self.render_centred(
                match count {
                    Some(count) => format!("{count} items"),
                    None => "Counting…".to_string(),
                },
                cx,
            ),
            PaneState::Loading {
                skeleton, remote, ..
            } => {
                if *skeleton {
                    self.render_skeleton(*remote, cx)
                } else {
                    // Under the loading threshold: blank, not flashing.
                    div().flex_1().into_any_element()
                }
            }
            PaneState::Ready { .. } => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .px(px(4.))
                .child(self.editor_element())
                .into_any_element(),
            PaneState::ReadyMd {
                show_source,
                base,
                remote,
                ..
            } => {
                if *show_source {
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .px(px(4.))
                        .child(self.editor_element())
                        .into_any_element()
                } else {
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .px(px(12.))
                        .py(px(8.))
                        .child(self.document_element(base.clone(), *remote, cx))
                        .into_any_element()
                }
            }
            PaneState::Image {
                render_image,
                bitmap_size,
                zoom,
                ..
            }
            | PaneState::Pdf {
                render_image,
                bitmap_size,
                zoom,
                ..
            } => self.render_image(render_image, *bitmap_size, *zoom, cx),
            // The sample grid: one virtualised `DataTable` whose delegate
            // holds the rows as shared strings. Sorting happens in the
            // delegate, header clicks only — never a full-file read.
            PaneState::Table { grid, .. } => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(grid.element(cx))
                .into_any_element(),
            PaneState::Unsupported { entry, message, .. } => {
                self.render_fallback(entry, message, cx)
            }
            PaneState::Error { message, .. } => self.render_error(message, cx),
        }
    }

    /// Fit-to-pane by default at any window size and pixel density; fixed
    /// scales scroll. Transparent pixels composite over the secondary
    /// surface — a neutral backdrop rather than a true checkerboard, which
    /// is the follow-up, not a silent omission.
    fn render_image(
        &self,
        render_image: &std::sync::Arc<gpui::RenderImage>,
        bitmap_size: (u32, u32),
        zoom: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Multi-frame images animate through the element's own frame
        // advance: no timer task, no per-frame allocation in the pane.
        let image = gpui::img(gpui::ImageSource::from(render_image.clone()));
        if zoom <= 0.0 {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .bg(cx.theme().secondary)
                .p(px(12.))
                .child(image.w_full().max_h_full())
                .into_any_element()
        } else {
            div()
                .id("preview-image-scroll")
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .overflow_x_scroll()
                .overflow_y_scroll()
                .bg(cx.theme().secondary)
                .p(px(12.))
                .child(
                    image
                        .w(px(bitmap_size.0 as f32 * zoom))
                        .h(px(bitmap_size.1 as f32 * zoom)),
                )
                .into_any_element()
        }
    }

    /// Muted centred line for the states that read nothing.
    fn render_centred(&self, text: String, cx: &mut Context<Self>) -> gpui::AnyElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.))
            .child(
                div()
                    .max_w(px(320.))
                    .text_center()
                    .text_size(rem_from_ui_px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(text)),
            )
            .into_any_element()
    }

    /// Skeleton in the body only; the header is already populated from the
    /// `Entry` the list holds.
    fn render_skeleton(&self, remote: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let bar = cx.theme().secondary;
        let mut body = v_flex().flex_1().gap(px(8.)).p(px(16.));
        for (index, width) in [0.9, 0.7, 0.8, 0.55, 0.75].iter().enumerate() {
            body = body.child(
                div()
                    .h(px(10.))
                    .w(gpui::relative(*width))
                    .rounded(px(5.))
                    .bg(bar)
                    .id(("preview-skeleton", index)),
            );
        }
        if remote {
            body = body.child(
                div()
                    .text_size(rem_from_ui_px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Reading over the connection…"),
            );
        }
        body.into_any_element()
    }

    /// Type icon at large size, the reason, and *Open with…*. A finished
    /// state, not a failure state. The message is a stored handle, cloned
    /// per frame as a refcount bump — never reformatted.
    fn render_fallback(
        &self,
        entry: &Entry,
        message: &SharedString,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let icon = cx.global::<FerretState>().icons.asset_path(entry);
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .p(px(24.))
            .child(
                Icon::default()
                    .path(icon)
                    .with_size(px(48.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .max_w(px(320.))
                    .text_center()
                    .text_size(rem_from_ui_px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(message.clone()),
            )
            .when(entry.path.is_local(), |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("preview-open")
                                .small()
                                .label("Open with…")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_current(window, cx);
                                })),
                        )
                        .child(
                            Button::new("preview-reveal")
                                .small()
                                .ghost()
                                .label("Reveal")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.reveal_current(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_error(&self, message: &SharedString, cx: &mut Context<Self>) -> gpui::AnyElement {
        h_flex()
            .flex_1()
            .items_start()
            .gap_2()
            .p(px(16.))
            .child(
                Icon::new(gpui_component::IconName::TriangleAlert)
                    .size_4()
                    .text_color(cx.theme().warning),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(rem_from_ui_px(13.))
                    .text_color(cx.theme().foreground)
                    .child(message.clone()),
            )
            .into_any_element()
    }

    /// The rendered document: selectable, with the link policy attached.
    ///
    /// Copy/select works in text; every click resolves through
    /// `handle_markdown_link`, never through the component's default
    /// open-URL path.
    fn document_element(
        &self,
        base: Option<PathBuf>,
        remote: bool,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let browser = self.browser.clone();
        gpui::image_cache(self.image_cache.clone()).child(
            TextView::new(&self.docview)
                .scrollable(true)
                .selectable(true)
                .on_link_click(move |url, _, window, cx| {
                    handle_markdown_link(&browser, base.clone(), remote, url, window, cx);
                }),
        )
    }

    fn render_footer(&self, footer: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_shrink_0()
            .px(px(12.))
            .py(px(6.))
            .border_t_1()
            .border_color(cx.theme().border)
            .text_size(rem_from_ui_px(11.5))
            .text_color(cx.theme().muted_foreground)
            .child(footer)
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_pane(window, cx)
    }
}

/// Resolve one Markdown link click (`docs/preview-plan.html` link policy).
///
/// * Same-document anchors (`#frag`) are inert: no anchor scrolling exists
///   yet, and the default would hand the fragment to the OS browser.
/// * `https`/`http`/`mailto` never open directly: the destination lands on
///   the clipboard with a visible toast. A confirmation dialog offering Open
///   is the follow-up; until then no document-chosen URL is fetched or
///   opened — the strictest honest reading of PV-D10.
/// * Anything else with a scheme is rejected with its scheme named.
/// * Relative file links navigate through the browser's normal navigation
///   (missing targets land on the existing gone handling). No filesystem
///   calls happen here: the join is lexical, `..` is refused outright, and
///   remote documents get no file links at all.
pub(super) fn handle_markdown_link(
    browser: &gpui::WeakEntity<crate::Browser>,
    base: Option<PathBuf>,
    remote: bool,
    url: &SharedString,
    window: &mut Window,
    cx: &mut App,
) {
    use ferret_preview::markdown::{has_traversal, is_absolute_path, url_scheme};
    use gpui_component::notification::Notification;

    let url = url.to_string();
    if url.starts_with('#') {
        return;
    }
    if url.starts_with("//") {
        copy_link_address(&url, window, cx);
        return;
    }
    if let Some(scheme) = url_scheme(&url) {
        if matches!(scheme.as_str(), "http" | "https" | "mailto") {
            copy_link_address(&url, window, cx);
        } else {
            window.push_notification(
                Notification::error(format!("Ferret blocked a {scheme} link.")),
                cx,
            );
        }
        return;
    }
    let path_text = url.split(['?', '#']).next().unwrap_or_default();
    if remote || base.is_none() {
        window.push_notification(
            Notification::error("File links are unavailable for this document."),
            cx,
        );
        return;
    }
    if path_text.is_empty() || is_absolute_path(path_text) || has_traversal(path_text) {
        window.push_notification(
            Notification::error("Ferret blocked a link leaving the document."),
            cx,
        );
        return;
    }
    let joined = base.unwrap_or_default().join(path_text);
    let _ = browser.update_in(cx, |browser, window, cx| {
        browser.navigate_preview_link(VfsPath::local(joined), window, cx);
    });
}

/// The destination lands on the clipboard with a visible toast: explicit,
/// inspectable, and never a fetch.
fn copy_link_address(url: &str, window: &mut Window, cx: &mut App) {
    use gpui_component::notification::Notification;

    cx.write_to_clipboard(gpui::ClipboardItem {
        entries: vec![gpui::ClipboardEntry::String(gpui::ClipboardString::new(
            url.to_string(),
        ))],
    });
    window.push_notification(
        Notification::info(format!("Link address copied: {url}")),
        cx,
    );
}
