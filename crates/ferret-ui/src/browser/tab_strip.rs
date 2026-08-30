//! The tab strip and its drag-to-reorder.
//!
//! Reordering is live: the dragged tab takes the slot under the pointer
//! on every move, so the strip follows the cursor rather than jumping
//! into place on release.

use super::*;

impl Browser {
    pub(super) fn render_tab_strip(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();

        // Each tab reports its left edge from inside its own prefix slot,
        // which is the only element in a `Tab` this crate owns.
        self.tab_lefts.lock().resize(self.tabs.len(), px(0.));

        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let title = tab.folder.read(cx).dir.file_name().unwrap_or("/");
                let dragging = self.tab_drag == Some(ix);
                let lefts = self.tab_lefts.clone();
                let mut t = Tab::new()
                    // `Tab` pads its label but not its prefix, so a bare icon
                    // sits flush against the tab's rounded left edge and gets
                    // clipped by it. The padding is ours to add, and the
                    // folder takes the same tint it has in the path bar
                    // rather than reading as chrome.
                    .prefix(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .pl(px(8.))
                            .on_prepaint(move |bounds, _, _| {
                                if let Some(slot) = lefts.lock().get_mut(ix) {
                                    *slot = bounds.origin.x;
                                }
                            })
                            .child(
                                Icon::default()
                                    .path("icons/file/folder-base.svg")
                                    .size_4()
                                    .text_color(BrowserTint::folder(cx)),
                            ),
                    )
                    .label(title.to_string())
                    .when(dragging, |t| t.opacity(0.6));
                if self.tabs.len() > 1 {
                    let this = cx.weak_entity();
                    t = t.suffix(
                        Button::new(SharedString::from(format!("close-tab-{ix}")))
                            .ghost()
                            .xsmall()
                            .icon(gpui_component::IconName::Close)
                            .on_click(move |_, _, cx| {
                                if let Some(this) = this.upgrade() {
                                    this.update(cx, |this, cx| this.close_tab(ix, cx));
                                }
                            }),
                    );
                }
                t
            })
            .collect::<Vec<_>>();

        let this = cx.weak_entity();
        let active_dir = self.active().folder.read(cx).dir.clone();
        let plus = Button::new("new-tab")
            .ghost()
            .xsmall()
            .icon(gpui_component::IconName::Plus)
            .tooltip("New Tab")
            .on_click(move |_, window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        let dir = this.active().folder.read(cx).dir.clone();
                        let _ = &active_dir;
                        this.push_tab(dir, window, cx);
                    });
                }
            });

        h_flex()
            .id("tab-strip")
            .items_center()
            .h(px(32.))
            .px(px(6.))
            .gap(px(4.))
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
            // The whole drag lives on mouse-move: `Tab` stops propagation of
            // its own mouse-down, so a press is never observable here — but a
            // move with the button held is, and the first one still lands on
            // the tab that was pressed.
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if event.pressed_button != Some(gpui::MouseButton::Left) {
                    this.end_tab_drag(cx);
                    return;
                }
                let Some(ix) = this.tab_at(event.position.x) else {
                    return;
                };
                match this.tab_drag {
                    None => {
                        this.tab_drag = Some(ix);
                        cx.notify();
                    }
                    Some(_) => this.move_dragged_tab_to(ix, cx),
                }
            }))
            .child(
                TabBar::new("browser-tabs")
                    .max_width(px(super::TAB_MAX_WIDTH))
                    .selected_index(self.active)
                    .text_size(px(12.5))
                    .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                        this.activate_tab(*ix, cx);
                    }))
                    .children(tabs),
            )
            .child(plus)
    }

    /// Which tab sits under window-space `x`: the last one whose left edge is
    /// at or before the pointer. `None` before the first paint, or left of the
    /// first tab.
    fn tab_at(&self, x: gpui::Pixels) -> Option<usize> {
        let lefts = self.tab_lefts.lock();
        lefts
            .iter()
            .rposition(|&left| left <= x)
            .filter(|_| lefts.iter().any(|&left| left > px(0.)))
    }

    /// Live tab reordering: the dragged tab takes `to`'s slot and everything
    /// between shifts, which is what makes the strip follow the pointer.
    fn move_dragged_tab_to(&mut self, to: usize, cx: &mut Context<Self>) {
        let Some(from) = self.tab_drag else {
            return;
        };
        if from == to || to >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.tab_drag = Some(to);
        self.activate_tab(to, cx);
    }

    pub(super) fn end_tab_drag(&mut self, cx: &mut Context<Self>) {
        if self.tab_drag.take().is_some() {
            cx.notify();
        }
    }
}
