mod theme;

pub use theme::TerminalPalette;

use gpui::{Context, IntoElement, ParentElement as _, Styled as _, Window, div, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, TitleBar,
    button::{Button, ButtonVariants as _},
    v_flex,
};

/// The window's root view.
///
/// Still the scaffold placeholder. It is replaced by the browser shell — sidebar,
/// file pane, preview pane and terminal panel — in Phase 1; see `docs/plan.html`.
pub struct RootView {
    clicks: u32,
}

impl RootView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { clicks: 0 }
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.clicks == 0 {
            "Ferret — scaffolding only. See docs/plan.html".to_string()
        } else {
            format!("Clicked {n}×", n = self.clicks)
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // The window uses a transparent system titlebar, so the app draws its own.
            // Phase 1 replaces this with the real toolbar; see `docs/plan.html` §3.
            .child(TitleBar::new().child(div().text_sm().child("Ferret")))
            .child(
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_4()
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    )
                    .child(Button::new("primary").primary().label("Let's go").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.clicks += 1;
                            cx.notify();
                        }),
                    )),
            )
    }
}
