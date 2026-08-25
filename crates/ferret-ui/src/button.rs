use crate::theme::Theme;
use gpui::{
    ClickEvent, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

/// Every variant, in the order the starter view renders them.
pub const BUTTON_VARIANTS: &[ButtonVariant] = &[
    ButtonVariant::Primary,
    ButtonVariant::Secondary,
    ButtonVariant::Ghost,
    ButtonVariant::Danger,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

impl ButtonVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            ButtonVariant::Primary => "Primary",
            ButtonVariant::Secondary => "Secondary",
            ButtonVariant::Ghost => "Ghost",
            ButtonVariant::Danger => "Danger",
        }
    }

    fn colors(self, theme: Theme) -> ButtonColors {
        match self {
            ButtonVariant::Primary => ButtonColors {
                background: theme.accent,
                foreground: theme.black,
                border: theme.accent,
            },
            ButtonVariant::Secondary => ButtonColors {
                background: theme.surface_raised,
                foreground: theme.foreground,
                border: theme.border,
            },
            ButtonVariant::Ghost => ButtonColors {
                background: theme.window_fill(),
                foreground: theme.muted_strong,
                border: theme.window_fill(),
            },
            ButtonVariant::Danger => ButtonColors {
                background: theme.red,
                foreground: theme.black,
                border: theme.red,
            },
        }
    }
}

#[derive(Clone, Copy)]
struct ButtonColors {
    background: Hsla,
    foreground: Hsla,
    border: Hsla,
}

#[derive(IntoElement)]
pub struct Button {
    id: SharedString,
    label: SharedString,
    variant: ButtonVariant,
    theme: Theme,
    disabled: bool,
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static>>,
}

impl Button {
    pub fn new(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::Secondary,
            theme: Theme::default(),
            disabled: false,
            on_click: None,
        }
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let colors = self.variant.colors(self.theme);
        let theme = self.theme;

        let mut button = div()
            .id(self.id)
            .flex()
            .items_center()
            .justify_center()
            .h(px(30.))
            .px(px(14.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.background)
            .text_size(px(13.))
            .text_color(colors.foreground)
            .child(self.label);

        if self.disabled {
            return button.opacity(0.4);
        }

        button = button
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover).border_color(theme.accent))
            .active(|style| style.bg(theme.selected));

        if let Some(handler) = self.on_click {
            button = button.on_click(move |event, window, cx| handler(event, window, cx));
        }

        button
    }
}
