use super::*;

use ferret_settings::{Appearance, Terminal};
use gpui_component::{
    dialog::DialogTitle,
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
};

const SETTINGS_WIDTH: f32 = 780.0;
const SETTINGS_HEIGHT: f32 = 520.0;
const SETTINGS_SIDEBAR_WIDTH: f32 = 210.0;

fn font_size(value: f64, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(
            ferret_settings::config::MIN_FONT_SIZE as f64,
            ferret_settings::config::MAX_FONT_SIZE as f64,
        ) as f32
    } else {
        default
    }
}

fn set_interface_font_size(value: f64, cx: &mut App) {
    let value = font_size(value, Appearance::default().ui_font_size);
    let changed = {
        let state = cx.global_mut::<FerretState>();
        let current = state.settings.appearance.ui_font_size;
        if current != value {
            state.settings.appearance.ui_font_size = value;
            state.touch_settings();
        }
        current != value
    };
    if !changed {
        return;
    }
    crate::state::arm_config_save(cx);
    crate::theme::set_interface_font_size(value, cx);
}

fn set_terminal_font_size(value: f64, cx: &mut App) {
    let value = font_size(value, Terminal::default().font_size);
    let changed = {
        let state = cx.global_mut::<FerretState>();
        let current = state.settings.terminal.font_size;
        if current != value {
            state.settings.terminal.font_size = value;
            state.touch_settings();
        }
        current != value
    };
    if !changed {
        return;
    }
    crate::state::arm_config_save(cx);
    cx.refresh_windows();
}

fn appearance_page() -> SettingPage {
    let options = NumberFieldOptions {
        min: ferret_settings::config::MIN_FONT_SIZE as f64,
        max: ferret_settings::config::MAX_FONT_SIZE as f64,
        step: 1.0,
    };
    SettingPage::new("Appearance")
        .description("Adjust Ferret's interface and terminal text independently.")
        .resettable(true)
        .default_open(true)
        .group(
            SettingGroup::new().title("Text size").items([
                SettingItem::new(
                    "Interface text size (px)",
                    SettingField::number_input(
                        options.clone(),
                        |cx| {
                            f64::from(cx.global::<FerretState>().settings.appearance.ui_font_size)
                        },
                        set_interface_font_size,
                    )
                    .default_value(f64::from(Appearance::default().ui_font_size)),
                )
                .description("Scales labels, menus, controls, and preview text throughout Ferret."),
                SettingItem::new(
                    "Terminal text size (px)",
                    SettingField::number_input(
                        options,
                        |cx| f64::from(cx.global::<FerretState>().settings.terminal.font_size),
                        set_terminal_font_size,
                    )
                    .default_value(f64::from(Terminal::default().font_size)),
                )
                .description("Scales the terminal grid without changing the interface or restarting the shell."),
            ]),
        )
}

impl Browser {
    pub(super) fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let base = window.rem_size();
        let viewport = window.viewport_size();
        let settings_width = rem_from_ui_px(SETTINGS_WIDTH)
            .to_pixels(base)
            .min(viewport.width * 0.94);
        let settings_height = rem_from_ui_px(SETTINGS_HEIGHT)
            .to_pixels(base)
            .min(viewport.height * 0.9);
        let sidebar_width = rem_from_ui_px(SETTINGS_SIDEBAR_WIDTH)
            .to_pixels(base)
            .min(settings_width * 0.45);
        window.open_dialog(cx, move |modal, _, _| {
            modal
                .w(settings_width)
                .h(settings_height)
                .title(DialogTitle::new().child("Settings"))
                .child(
                    Settings::new("ferret-settings")
                        .sidebar_width(sidebar_width)
                        .page(appearance_page()),
                )
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_input_uses_the_shared_bounds() {
        let default = Appearance::default().ui_font_size;
        assert_eq!(
            font_size(f64::MIN, default),
            ferret_settings::config::MIN_FONT_SIZE
        );
        assert_eq!(
            font_size(f64::MAX, default),
            ferret_settings::config::MAX_FONT_SIZE
        );
        assert_eq!(font_size(f64::NAN, default), default);
        assert_eq!(
            font_size(f64::NAN, Terminal::default().font_size),
            Terminal::default().font_size
        );
    }
}
