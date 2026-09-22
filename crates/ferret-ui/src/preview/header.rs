//! The preview header: identical for every file type.
//!
//! Left: the file name (truncated in the middle, never wrapped), then a
//! muted second line — kind · size · modified, where the kind is the
//! resolved language or format name, not the extension. Right: the Edit
//! button for saveable text (F-36), Save / Revert while editing, and the
//! overflow menu (Open with…, Reveal, Copy path, Word wrap, Line numbers).
//! Keeping the header type-independent is what makes arrow-keying feel like
//! one surface rather than seven.

use super::*;
use gpui::{IntoElement, ParentElement as _, SharedString, Styled as _, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
    v_flex,
};

use crate::actions;

impl PreviewPane {
    /// Truncate a name in the middle, never wrapping.
    ///
    /// The head matters more than the middle for long names, and the
    /// extension matters most of all — so the cut keeps both ends.
    pub(super) fn display_name(name: &str) -> String {
        const KEEP: usize = 28;
        if name.chars().count() <= KEEP * 2 + 1 {
            return name.to_string();
        }
        let head: String = name.chars().take(KEEP).collect();
        let tail: String = name
            .chars()
            .rev()
            .take(KEEP)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{head}…{tail}")
    }

    pub(super) fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (title, detail) = match &self.state {
            PaneState::Empty { name, .. } => (Self::display_name(name), "Preview".to_string()),
            PaneState::Multiple { count, .. } => {
                (format!("{count} items selected"), "Preview".to_string())
            }
            PaneState::Directory { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Loading { name, detail, .. } => (Self::display_name(name), detail.clone()),
            PaneState::Ready { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::ReadyMd { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Image { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Table { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Pdf { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Unsupported { entry, detail, .. } => {
                (Self::display_name(entry.name()), detail.clone())
            }
            PaneState::Error { name, .. } => (Self::display_name(name), "Preview".to_string()),
        };
        let reveal_label: &'static str = if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else {
            "Show in Explorer"
        };
        // Rendered/Source is offered only for Markdown, and labels the
        // switch by the view it leaves, not the one it enters.
        let source_toggle =
            self.showing_markdown().map(
                |show_source| {
                    if show_source { "Rendered" } else { "Source" }
                },
            );
        let is_image = matches!(self.state, PaneState::Image { .. } | PaneState::Pdf { .. });
        // Page navigation lives in the footer bar too; the menu holds the
        // same two actions for discoverability, disabled at the document's
        // ends or while a turn is in flight.
        let pdf_nav = match &self.state {
            PaneState::Pdf {
                pages,
                loading_page,
                ..
            } => Some((
                pages.current > 1 && !loading_page,
                pages.current < pages.total && !loading_page,
            )),
            _ => None,
        };
        // The reversible header inference toggle: offered only when the
        // parser detected a header, and labelled by the state it leaves.
        let csv_header = match &self.state {
            PaneState::Table { grid, .. } => grid.header_state(cx),
            _ => None,
        };
        // F-36: Edit for complete, lossless text on an atomic-save backend;
        // Save / Revert while the buffer is pinned. The Save button labels
        // the dirty state ("Save •") so the header says what would be lost.
        let edit_buttons = if self.editing.is_some() {
            EditButtons::Editing
        } else if self.can_edit() {
            EditButtons::Offer
        } else {
            EditButtons::None
        };

        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px(px(12.))
            .py(px(8.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        gpui::div()
                            .truncate()
                            .text_size(px(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        gpui::div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(detail)),
                    ),
            )
            .children(self.render_edit_buttons(edit_buttons, cx))
            .child(
                Button::new("preview-overflow")
                    .ghost()
                    .icon(Icon::new(gpui_component::IconName::EllipsisVertical))
                    .tooltip("Preview options")
                    .dropdown_menu(move |menu, _, _| {
                        let menu = menu
                            .menu("Open with…", Box::new(actions::PreviewOpen))
                            .menu(reveal_label, Box::new(actions::PreviewReveal))
                            .menu("Copy path", Box::new(actions::PreviewCopyPath))
                            .separator();
                        let menu = match source_toggle {
                            Some(label) => menu
                                .menu(label, Box::new(actions::PreviewToggleSource))
                                .separator(),
                            None => menu,
                        };
                        let menu = if is_image {
                            menu.menu("Zoom in", Box::new(actions::PreviewZoomIn))
                                .menu("Zoom out", Box::new(actions::PreviewZoomOut))
                                .menu("Reset zoom", Box::new(actions::PreviewZoomReset))
                                .separator()
                        } else {
                            menu
                        };
                        let menu = match csv_header {
                            Some(true) => menu
                                .menu(
                                    "Show first row as data",
                                    Box::new(actions::PreviewToggleCsvHeader),
                                )
                                .separator(),
                            Some(false) => menu
                                .menu(
                                    "Use first row as header",
                                    Box::new(actions::PreviewToggleCsvHeader),
                                )
                                .separator(),
                            None => menu,
                        };
                        let menu = match pdf_nav {
                            Some((prev, next)) => menu
                                .menu_with_disabled(
                                    "Previous page",
                                    Box::new(actions::PreviewPdfPrev),
                                    !prev,
                                )
                                .menu_with_disabled(
                                    "Next page",
                                    Box::new(actions::PreviewPdfNext),
                                    !next,
                                )
                                .separator(),
                            None => menu,
                        };
                        menu.menu("Word wrap", Box::new(actions::PreviewToggleWrap))
                            .menu("Line numbers", Box::new(actions::PreviewToggleLineNumbers))
                    }),
            )
    }
}

/// Which edit controls the header shows: the reference puts *Edit*
/// right-aligned in the header, and Save / Revert take its place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditButtons {
    None,
    Offer,
    Editing,
}

impl PreviewPane {
    fn render_edit_buttons(
        &self,
        buttons: EditButtons,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        match buttons {
            EditButtons::None => None,
            EditButtons::Offer => Some(
                Button::new("preview-edit")
                    .ghost()
                    .label("Edit")
                    .tooltip("Edit this file in place")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.begin_edit(window, cx);
                    }))
                    .into_any_element(),
            ),
            EditButtons::Editing => {
                let dirty = self.is_dirty();
                let saving = self.is_saving();
                let save_label = if saving {
                    "Saving…"
                } else if dirty {
                    "Save •"
                } else {
                    "Save"
                };
                Some(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("preview-revert")
                                .ghost()
                                .label("Revert")
                                .tooltip("Throw away the edits")
                                .disabled(!dirty || saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.revert_edit(window, cx);
                                })),
                        )
                        .child(
                            Button::new("preview-save")
                                .primary()
                                .label(save_label)
                                .tooltip("Save atomically (⌘S)")
                                .disabled(!dirty || saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_edit(window, cx);
                                })),
                        )
                        .into_any_element(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PreviewPane;

    #[test]
    fn long_names_keep_both_ends() {
        let name = "a_quite_long_filename_for_a_config_file_with_extra_versioning.yaml";
        let shown = PreviewPane::display_name(name);
        assert!(shown.starts_with("a_quite_long_filename_for_a_"), "{shown}");
        assert!(shown.ends_with("with_extra_versioning.yaml"), "{shown}");
        assert!(shown.contains('…'));
        assert_eq!(PreviewPane::display_name("short.txt"), "short.txt");
    }
}
