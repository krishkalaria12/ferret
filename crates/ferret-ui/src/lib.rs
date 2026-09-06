//! Ferret's UI crate: views, panels, components and Ferret-specific theme
//! tokens.
//!
//! Layout follows the repository conventions (`AGENTS.md`): public surface is
//! re-exported here, modules stay private, general interface colour comes
//! from `cx.theme()`, and only what `gpui-component` has no concept of lives
//! in [`theme`].

// The allocations-per-frame budget's counting allocator (`docs/audit.html`
// §11). Test builds only, and only with `--features dhat-heap`; a release
// binary never sees it.
#[cfg(all(test, feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

pub mod actions;
mod assets;
mod browser;
mod folder;
mod format;
mod icons;
mod mounts;
mod pane;
mod state;
mod theme;

pub use assets::Assets;
pub use browser::{Browser, TITLE_BAR_HEIGHT, TRAFFIC_LIGHT_DIAMETER};
pub use format::{format_date, format_size, natural_compare};
pub use state::{FerretState, load_state};
pub use theme::{BrowserPalette, TerminalPalette, register as register_theme};

/// Write any settings or folder preferences the debounce has not flushed yet.
///
/// Saving is debounced by [`config::SAVE_DEBOUNCE`](ferret_settings::config::SAVE_DEBOUNCE)
/// so a run of sort clicks costs one write rather than one per click — which
/// means quitting inside that window would otherwise drop the last one. The
/// binary hooks this to `App::on_app_quit`; it is the one config write allowed
/// to block, because there is no next frame left to protect.
pub fn flush_pending_saves(cx: &mut gpui::App) {
    if cx.has_global::<FerretState>() {
        cx.global_mut::<FerretState>().flush_pending_saves();
    }
}

/// Close every mounted connection before the process exits.
///
/// Sessions live in the global state precisely so a tab close cannot take one
/// with it, which means quitting is the only thing that ends them. Dropping
/// each one closes its connection; the runtime shutdown that follows in the
/// binary is what guarantees no SSH task outlives the process.
pub fn disconnect_mounts(cx: &mut gpui::App) {
    if cx.has_global::<FerretState>() {
        Browser::disconnect_all(cx);
    }
}

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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use gpui_component::Root;

        // The fifth part of the bootstrap contract, and the one that had been
        // missed. `Root::render` draws the mounted view, the tooltip overlay
        // and the native-menu overlay — and nothing else. The dialog, sheet and
        // notification layers are static builders the *application's* root has
        // to mount, so without these three lines `window.open_dialog` stores a
        // builder that is never called and `push_notification` queues a toast
        // nobody ever sees. Both are silent at compile time and silent at
        // runtime, which is exactly the failure mode `AGENTS.md` warns about.
        //
        // Siblings of the browser rather than children of it: each layer
        // positions itself against the nearest positioned ancestor, which is
        // why this container is `relative`.
        let sheets = Root::render_sheet_layer(window, cx);
        let dialogs = Root::render_dialog_layer(window, cx);
        let notifications = Root::render_notification_layer(window, cx);

        v_flex()
            .relative()
            .size_full()
            .child(self.browser.clone())
            .children(sheets)
            .children(dialogs)
            .children(notifications)
    }
}
