mod button;
mod theme;

pub use button::{BUTTON_VARIANTS, Button, ButtonVariant};
pub use theme::Theme;

use gpui::{Context, SharedString, Window, div, prelude::*, px};

/// The window's root view.
///
/// This is still the starter placeholder. It gets replaced by the browser shell
/// (sidebar / file pane / preview / terminal) in Phase 1 — see `docs/plan.html`.
pub struct RootView {
    theme: Theme,
    clicks: u32,
}

impl RootView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            theme: Theme::groknight(),
            clicks: 0,
        }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let status: SharedString = if self.clicks == 0 {
            "Ferret — scaffolding only. See docs/plan.html".into()
        } else {
            format!("Clicked {n}×", n = self.clicks).into()
        };

        div()
            .flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(theme.window_fill())
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .bg(theme.main_bg)
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(theme.muted_strong)
                            .child(status),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .px(px(24.))
                            .children(BUTTON_VARIANTS.iter().copied().map(|variant| {
                                let id: SharedString =
                                    format!("btn-{}", variant.as_str().to_lowercase()).into();
                                Button::new(id, variant.as_str())
                                    .variant(variant)
                                    .theme(theme)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.clicks += 1;
                                        cx.notify();
                                    }))
                            })),
                    ),
            )
    }
}
