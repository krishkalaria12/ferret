use ferret_settings::config::DEFAULT_UI_FONT_SIZE;

pub(crate) const fn rem_from_ui_px(value: f32) -> gpui::Rems {
    gpui::rems(value / DEFAULT_UI_FONT_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_sizes_keep_their_default_pixel_values() {
        assert_eq!(rem_from_ui_px(DEFAULT_UI_FONT_SIZE), gpui::rems(1.0));
        assert_eq!(
            rem_from_ui_px(11.5).to_pixels(gpui::px(13.0)),
            gpui::px(11.5)
        );
    }
}
