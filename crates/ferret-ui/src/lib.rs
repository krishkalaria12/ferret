//! Ferret's UI crate: views, panels, components and Ferret-specific theme
//! tokens.
//!
//! Layout follows the repository conventions (`AGENTS.md`): public surface is
//! re-exported here, modules stay private, general interface colour comes
//! from `cx.theme()`, and only what `gpui-component` has no concept of lives
//! in [`theme`].

pub mod actions;
mod assets;
mod browser;
mod folder;
mod format;
mod icons;
mod pane;
mod state;
mod theme;

pub use assets::Assets;
pub use browser::{Browser, TITLE_BAR_HEIGHT, TRAFFIC_LIGHT_DIAMETER};
pub use format::{format_date, format_size, natural_compare};
pub use state::{FerretState, load_state};
pub use theme::{BrowserPalette, TerminalPalette, register as register_theme};

use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, Window,
};
use gpui_component::v_flex;

/// The window's root view: the browser shell.
pub struct RootView {
    browser: gpui::Entity<Browser>,
}

impl RootView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let browser = cx.new(|cx| Browser::new(window, cx));
        Self { browser }
    }

    /// The shell, for tests and embedding.
    pub fn browser(&self) -> &gpui::Entity<Browser> {
        &self.browser
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().size_full().child(self.browser.clone())
    }
}
