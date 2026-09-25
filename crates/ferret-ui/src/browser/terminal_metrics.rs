use super::*;

pub(super) fn terminal_metrics(cx: &App, window: &mut Window) -> (SharedString, f32) {
    let settings = &cx.global::<FerretState>().settings.terminal;
    let family = settings
        .font_family
        .clone()
        .unwrap_or_else(|| default_terminal_font(cx));
    let run = gpui::TextRun {
        len: 1,
        font: gpui::font(SharedString::from(family.clone())),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let width = window
        .text_system()
        .layout_line("M", px(settings.font_size), &[run], None)
        .width;
    (SharedString::from(family), f32::from(width).max(1.0))
}

fn default_terminal_font(cx: &App) -> String {
    let installed = cx.text_system().all_font_names();
    let first_installed = |candidates: &[&str]| {
        candidates
            .iter()
            .find(|candidate| installed.iter().any(|font| font == **candidate))
            .map(|candidate| (*candidate).to_owned())
    };
    #[cfg(target_os = "macos")]
    {
        first_installed(&["JetBrainsMono Nerd Font Mono", "MesloLGS NF", "Menlo"])
            .unwrap_or_else(|| "monospace".into())
    }
    #[cfg(target_os = "windows")]
    {
        first_installed(&["CaskaydiaMono Nerd Font", "Cascadia Mono", "Consolas"])
            .unwrap_or_else(|| "monospace".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "monospace".into()
    }
}
