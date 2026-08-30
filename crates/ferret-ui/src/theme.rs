//! Ferret-specific theme tokens.
//!
//! General interface colours come from `gpui-component`'s theme — reach for
//! `cx.theme()` rather than adding a token here. This module holds only what
//! `gpui-component` has no concept of: the sixteen ANSI slots the terminal panel
//! needs, and the colours that encode file and git state in the browser.

use gpui::{App, Hsla, Rgba, rgb};

/// Ferret's interface palette, in `gpui-component`'s theme format.
///
/// The library ships a stock dark theme built on `neutral-950` (`#0a0a0a`),
/// which is near enough to pure black that every surface in the window reads
/// as one flat sheet — no separation between sidebar, chrome and listing.
/// These are the values `docs/ui-base.html` §7 specifies instead.
const THEMES: &str = include_str!("../themes/ferret.json");

/// The two Ferret ships with, opaque.
const LIGHT: &str = "Ferret Light";
const DARK: &str = "Ferret Dark";
/// The translucent pair, used only when the window is drawing blurred (D-19
/// keeps blur optional, so these are never the fallback).
const LIGHT_GLASS: &str = "Ferret Light Glass";
const DARK_GLASS: &str = "Ferret Dark Glass";

/// Load Ferret's palette and make it the active theme.
///
/// Must run after `gpui_component::init` — that is what creates the registry
/// this writes into — and before the first window opens, or the first frame
/// paints in the library's colours and then swaps.
///
/// `glass` selects the translucent variants, which only make sense behind a
/// blurred window: on an opaque one their alpha composites against whatever
/// GPUI clears to rather than against the desktop.
///
/// `font_size` is `settings.appearance.ui_font_size`, already clamped to a
/// usable range by `ferret-settings`. It goes on the theme rather than
/// straight onto the window because `gpui_component::Root` re-applies
/// `set_rem_size(cx.theme().font_size)` on every frame — setting the window
/// directly would be overwritten before it was ever seen.
pub fn register(glass: bool, font_size: f32, cx: &mut App) {
    use gpui_component::{Theme, ThemeMode, ThemeRegistry};

    if let Err(err) = ThemeRegistry::global_mut(cx).load_themes_from_str(THEMES) {
        // A broken palette is not worth failing to start over: the library's
        // own theme still renders a usable window, so say what happened and
        // keep going.
        eprintln!("theme: can't load Ferret's palette, using the default: {err}");
        return;
    }

    let (light, dark) = if glass {
        (LIGHT_GLASS, DARK_GLASS)
    } else {
        (LIGHT, DARK)
    };
    let registry = ThemeRegistry::global(cx);
    let light = registry.themes().get(light).cloned();
    let dark = registry.themes().get(dark).cloned();

    // `Theme::change` re-applies whichever of the two the current mode names,
    // so both are swapped before it runs.
    if !cx.has_global::<Theme>() {
        Theme::change(ThemeMode::default(), None, cx);
    }
    let theme = Theme::global_mut(cx);
    let mode = theme.mode;
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
    Theme::change(mode, None, cx);
    // After `change`, which re-applies a whole theme config and would put the
    // library's own size back.
    Theme::global_mut(cx).font_size = gpui::px(font_size);
}

/// The sixteen ANSI colours plus foreground, background and cursor, as a terminal
/// emulator needs them. Loaded from the JSON files in `themes/`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalPalette {
    pub foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,

    pub black: Hsla,
    pub red: Hsla,
    pub green: Hsla,
    pub yellow: Hsla,
    pub blue: Hsla,
    pub magenta: Hsla,
    pub cyan: Hsla,
    pub white: Hsla,

    pub bright_black: Hsla,
    pub bright_red: Hsla,
    pub bright_green: Hsla,
    pub bright_yellow: Hsla,
    pub bright_blue: Hsla,
    pub bright_magenta: Hsla,
    pub bright_cyan: Hsla,
    pub bright_white: Hsla,
}

impl TerminalPalette {
    /// Groknight — the bundled dark palette, mirrored in `themes/groknight.json`.
    pub fn groknight() -> Self {
        Self {
            foreground: hex(0xe1e1e1),
            background: hex(0x141414),
            cursor: hex(0xc8c8c8),

            black: hex(0x0a0a0a),
            red: hex(0xf7768e),
            green: hex(0x9ece6a),
            yellow: hex(0xe0af68),
            blue: hex(0x7aa2f7),
            magenta: hex(0xbb9af7),
            cyan: hex(0x7dcfff),
            white: hex(0xc8c8c8),

            bright_black: hex(0x787878),
            bright_red: hex(0xff8fa3),
            bright_green: hex(0xb9f27c),
            bright_yellow: hex(0xffc777),
            bright_blue: hex(0x89b4fa),
            bright_magenta: hex(0xcba6f7),
            bright_cyan: hex(0x89ddff),
            bright_white: hex(0xf3f3f3),
        }
    }
}

impl Default for TerminalPalette {
    fn default() -> Self {
        Self::groknight()
    }
}

fn hex(value: u32) -> Hsla {
    let Rgba { r, g, b, a } = rgb(value);
    Rgba { r, g, b, a }.into()
}

/// The tokens `gpui-component` has no concept of (`docs/ui-base.html` §7):
/// the two selection fills and the folder tint.
///
/// Every token is defined for both themes — a token defined for only one is a
/// bug. Values mirror `docs/ui-base.html`'s `--app-selected`,
/// `--app-selected-idle` and `--app-folder`.
#[derive(Clone, Copy, Debug)]
pub struct BrowserPalette {
    /// Selected row fill while the pane has focus.
    ///
    /// Not `cx.theme().selection`: that token is the *text* highlight, and
    /// the library clamps it to 30% alpha on the way in — so a row filled
    /// with it comes out a wash of whatever is behind it, and the focused
    /// selection ends up weaker than the idle one it is supposed to outrank.
    pub selected: Hsla,
    /// Selected row fill when the file pane has lost focus.
    pub selected_idle: Hsla,
    /// Folder glyph tint; the one place colour carries meaning in the listing.
    pub folder: Hsla,

    // Git state (`docs/git.html` §9). `gpui-component` has no concept of any
    // of these, which is what makes them this module's business rather than
    // `cx.theme()`'s.
    //
    /// Changed in the work tree and not staged.
    pub git_modified: Hsla,
    /// Staged and ready to commit.
    pub git_staged: Hsla,
    /// Not in the index. Grey on purpose — new is not an alarm.
    pub git_untracked: Hsla,
    /// Unmerged. The only red in the listing.
    pub git_conflicted: Hsla,
    /// The branch chip's glyph and label.
    pub git_branch: Hsla,
}

impl BrowserPalette {
    /// The dot colour for one file status.
    pub fn git(&self, status: ferret_git::FileStatus) -> Hsla {
        use ferret_git::FileStatus::*;
        match status {
            Untracked => self.git_untracked,
            // A staged file that has been edited again draws as staged and
            // wears the modified colour as a ring, so it needs no sixth token.
            Staged | StagedAndModified => self.git_staged,
            Modified => self.git_modified,
            Conflicted => self.git_conflicted,
        }
    }
}

impl BrowserPalette {
    /// Resolve against the active theme's mode, so switching light/dark moves
    /// these with it.
    pub fn resolve(cx: &gpui::App) -> Self {
        use gpui_component::Theme;
        if Theme::global(cx).mode.is_dark() {
            Self {
                selected: hex(0x2f6bd4),
                selected_idle: hex(0x33333c),
                folder: hex(0x5aa6ff),
                // The Groknight yellow and green, deliberately: the terminal
                // panel six pixels below is printing `git status` in exactly
                // these, and two greens for one meaning in one window reads as
                // sloppiness without ever being consciously noticed.
                git_modified: hex(0xe0af68),
                git_staged: hex(0x9ece6a),
                git_untracked: hex(0x8b93a3),
                git_conflicted: hex(0xf7768e),
                git_branch: hex(0xa0aec0),
            }
        } else {
            Self {
                selected: hex(0x0a6cff),
                selected_idle: hex(0xe2e2e9),
                folder: hex(0x4a9eff),
                // Darkened against a white ground: the dark theme's values are
                // tuned for a #141414 backdrop and wash out completely here.
                git_modified: hex(0xb8860b),
                git_staged: hex(0x2e7d32),
                git_untracked: hex(0x6b7280),
                git_conflicted: hex(0xc62828),
                git_branch: hex(0x4a5568),
            }
        }
    }
}

/// Fixed-advance digits, for any column where numbers stack (§7).
///
/// The interface face ships proportional figures by default, so a `1` is
/// narrower than a `0`. Down a column of sizes or dates that makes the text
/// shift left and right as rows scroll past — §7 calls it out as visible
/// jitter in the Size column, which is exactly where it is worst.
///
/// `tnum` is the OpenType tag for tabular figures; a face without it simply
/// ignores the feature, so this is safe to apply unconditionally.
pub fn tabular_numerals() -> gpui::FontFeatures {
    static FEATURES: std::sync::OnceLock<gpui::FontFeatures> = std::sync::OnceLock::new();
    FEATURES
        .get_or_init(|| gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".to_string(), 1)])))
        .clone()
}

/// Text that stays legible on `fill`.
///
/// The selection fill comes from `gpui-component`, and it is a saturated
/// accent in the dark theme but a pale tint in the light one. Hard-coding
/// white text is therefore wrong in exactly one of the two themes — which is
/// the failure the "both themes" gate exists to catch — so the colour is
/// chosen from the fill's luminance instead.
pub fn text_on(fill: Hsla, dark_text: Hsla) -> Hsla {
    if relative_luminance(fill) > 0.5 {
        dark_text
    } else {
        gpui::white()
    }
}

/// Rec. 709 luma, which is close enough to perceived brightness for a
/// two-way choice and far cheaper than a full contrast-ratio computation.
fn relative_luminance(color: Hsla) -> f32 {
    let Rgba { r, g, b, .. } = color.into();
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_flips_with_the_fill_it_sits_on() {
        let dark_text = hex(0x1a1a1a);

        // A saturated accent (the dark theme's selection) takes white text.
        assert_eq!(text_on(hex(0x2563eb), dark_text), gpui::white());
        // A pale tint (the light theme's selection) must not.
        assert_eq!(text_on(hex(0xdbeafe), dark_text), dark_text);
    }

    #[test]
    fn hex_preserves_channels_and_is_opaque() {
        let color: Rgba = hex(0x7aa2f7).into();
        assert_eq!(
            (
                (color.r * 255.).round() as u32,
                (color.g * 255.).round() as u32,
                (color.b * 255.).round() as u32
            ),
            (0x7a, 0xa2, 0xf7)
        );
        assert_eq!(color.a, 1.0);
    }
}
