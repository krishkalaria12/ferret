//! The preview header: identical for every file type.
//!
//! Left: the file name (truncated in the middle, never wrapped), then a
//! muted second line — kind · size · modified, where the kind is the
//! resolved language or format name, not the extension. Right: the overflow
//! menu (Open with…, Reveal, Copy path, Word wrap, Line numbers). Keeping
//! the header type-independent is what makes arrow-keying feel like one
//! surface rather than seven.
//!
//! No *Edit* button yet: F-36 needs `Vfs::write_atomic` with its conflict
//! contract first (PV-R01), and the button appears only for complete,
//! lossless text on a backend with the atomic-save capability.

use super::*;
use gpui::{IntoElement, ParentElement as _, SharedString, Styled as _, px};
use gpui_component::{
    ActiveTheme as _, Icon,
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
        let is_image = matches!(self.state, PaneState::Image { .. });

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
                        menu.menu("Word wrap", Box::new(actions::PreviewToggleWrap))
                            .menu("Line numbers", Box::new(actions::PreviewToggleLineNumbers))
                    }),
            )
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
