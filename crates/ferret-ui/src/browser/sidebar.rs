//! The sidebar: favourites, places and volumes, the rows that select
//! them, and the two drags it hosts — its own width, and reordering
//! favourites.

use super::*;

/// Sidebar width bounds; mirrors `ferret-settings`' clamps so a drag can't
/// produce a width the settings file would refuse.
const MIN_SIDEBAR_WIDTH: f32 = ferret_settings::config::MIN_SIDEBAR_WIDTH;
const MAX_SIDEBAR_WIDTH: f32 = ferret_settings::config::MAX_SIDEBAR_WIDTH;

/// Whether a sidebar place is the directory being browsed (or an ancestor of
/// it for volumes and drives, which stay lit while you descend into them).
fn place_is_current(current: &VfsPath, place: &Path) -> bool {
    let Some(cur) = current.as_local() else {
        return false;
    };
    if cur == place {
        return true;
    }
    cur.starts_with(place) && place.parent().is_some()
}

impl Browser {
    /// The drag handle on the sidebar's right edge.
    pub(super) fn render_sidebar_divider(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-divider")
            .w(px(5.))
            .h_full()
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .hover(|el| el.bg(cx.theme().primary.opacity(0.35)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.sidebar_drag = Some(SidebarDrag {
                        start_x: event.position.x,
                        start_width: this.sidebar_width,
                    });
                    cx.notify();
                }),
            )
    }

    /// Follow the pointer while the sidebar divider is held. Width is clamped
    /// here rather than on release so the sidebar never paints outside the
    /// bounds `ferret-settings` would accept.
    pub(super) fn update_sidebar_drag(&mut self, x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = &self.sidebar_drag else {
            return;
        };
        let delta = f32::from(x - drag.start_x);
        let width = (drag.start_width + delta).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// Persist on release, not on every move: a drag would otherwise rewrite
    /// the settings file once per frame.
    pub(super) fn end_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if self.sidebar_drag.take().is_none() {
            return;
        }
        let width = self.sidebar_width;
        let state = cx.global_mut::<FerretState>();
        state.settings.sidebar.width = width;
        state.save_settings();
        cx.notify();
    }

    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let dir = self.active().folder.read(cx).dir.clone();

        let focused = self.sidebar_focus.is_focused(window);
        let mut side = v_flex()
            .id("sidebar")
            .key_context(actions::context::SIDEBAR)
            .track_focus(&self.sidebar_focus)
            .overflow_y_scroll()
            .w_full()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            // Which region has focus has to be visible, or Tab moves
            // something the user cannot see.
            .when(focused, |el| el.border_r_2().border_color(theme.primary))
            .pt(px(6.))
            .pb(px(8.));

        side = side.child(section_label("Places", cx));
        for (i, place) in self.places(cx).into_iter().enumerate() {
            let active = place_is_current(&dir, &place.path);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("place-{i}")),
                &place.name,
                place.kind.icon_name(),
                place.path.clone(),
                active,
                None,
                cx,
            ));
        }

        side = side.child(section_label("Favorites", cx));
        for (i, fav) in self.favorites(cx).into_iter().enumerate() {
            let name = fav
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| fav.to_string_lossy().into_owned());
            let active = place_is_current(&dir, &fav);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("favorite-{i}")),
                &name,
                "icons/file/folder-base.svg",
                fav.clone(),
                active,
                Some((i, fav)),
                cx,
            ));
        }

        side = side.child(section_label("Volumes", cx));
        for (i, volume) in self.volumes(cx).into_iter().enumerate() {
            let active = place_is_current(&dir, &volume.path);
            side = side.child(self.sidebar_place_row(
                SharedString::from(format!("volume-{i}")),
                &volume.name,
                volume.kind.icon_name(),
                volume.path.clone(),
                active,
                None,
                cx,
            ));
        }

        side
    }

    /// One h22 pill row. `favorite` carries the row's place in the favourites
    /// list, which is what makes it removable and re-orderable.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_place_row(
        &self,
        id: SharedString,
        label: &str,
        icon: &'static str,
        target: PathBuf,
        active: bool,
        favorite: Option<(usize, PathBuf)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = if active {
            crate::theme::text_on(theme.selection, theme.sidebar_foreground)
        } else {
            theme.sidebar_foreground
        };

        let drop_tint = theme.selection.opacity(0.5);
        let this = cx.weak_entity();
        let dragging = favorite
            .as_ref()
            .is_some_and(|(ix, _)| self.favorite_drag == Some(*ix));
        let row = h_flex()
            .id(id)
            .w_full()
            .h(px(22.))
            .mx(px(6.))
            .rounded(px(5.))
            .px(px(8.))
            .gap(px(8.))
            .items_center()
            .flex_shrink_0()
            .when(active, |el| el.bg(theme.selection))
            .when(!active, |el| el.hover(|el| el.bg(theme.secondary)))
            .when(dragging, |el| el.opacity(0.6))
            // Every sidebar row is a folder, so every one is a drop target
            // (§8). Dropping here moves into that place without navigating.
            .drag_over::<crate::pane::DraggedPaths>(move |style, _, _, _| style.bg(drop_tint))
            .on_drop({
                let this = this.clone();
                let dest = VfsPath::local(target.clone());
                move |dragged: &crate::pane::DraggedPaths, window, cx| {
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let (sources, dest) = (dragged.paths.clone(), dest.clone());
                    let force_copy = window.modifiers().alt;
                    this.update(cx, |this, cx| {
                        // Same guard the pane applies.
                        let sources = crate::pane::droppable(&sources, &dest);
                        if !sources.is_empty() {
                            this.transfer(sources, dest, force_copy, window, cx);
                        }
                    });
                }
            })
            // Favourites reorder by drag (§8). Driven by mouse-move with the
            // button held, so the row's own click handler still navigates.
            .when_some(favorite.as_ref().map(|(ix, _)| *ix), |el, ix| {
                el.on_mouse_move(
                    cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(gpui::MouseButton::Left) {
                            return;
                        }
                        match this.favorite_drag {
                            None => {
                                this.favorite_drag = Some(ix);
                                cx.notify();
                            }
                            Some(_) => this.move_dragged_favorite_to(ix, cx),
                        }
                    }),
                )
            })
            .child(
                Icon::default()
                    .path(icon)
                    .with_size(px(super::SIDEBAR_ICON_SIZE))
                    .text_color(fg.opacity(0.9)),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(13.))
                    .text_color(fg)
                    .child(label.to_string()),
            )
            .on_click(move |_: &ClickEvent, window, cx| {
                let Some(this) = this.upgrade() else {
                    return;
                };
                let target = VfsPath::local(target.clone());
                this.update(cx, |this, cx| {
                    this.navigate(target, window, cx);
                });
            });

        match favorite.map(|(_, path)| path) {
            Some(path) => row
                .context_menu(move |menu, _, _| {
                    menu.item(PopupMenuItem::new("Remove from Favorites").on_click({
                        let path = path.clone();
                        move |_, _, cx| {
                            use gpui::BorrowAppContext as _;
                            cx.update_global(|state: &mut FerretState, _| {
                                state
                                    .settings
                                    .sidebar
                                    .favorites
                                    .retain(|p| p.as_path() != path);
                                state.save_settings();
                            });
                        }
                    }))
                })
                .into_any_element(),
            None => row.into_any_element(),
        }
    }

    /// Reorder favourites live while one is dragged over another.
    fn move_dragged_favorite_to(&mut self, to: usize, cx: &mut Context<Self>) {
        let Some(from) = self.favorite_drag else {
            return;
        };
        if from == to {
            return;
        }
        let state = cx.global_mut::<FerretState>();
        let favorites = &mut state.settings.sidebar.favorites;
        if from >= favorites.len() || to >= favorites.len() {
            return;
        }
        let moved = favorites.remove(from);
        favorites.insert(to, moved);
        self.favorite_drag = Some(to);
        cx.notify();
    }

    /// Written once, on release: a drag would otherwise rewrite the settings
    /// file on every frame it crosses a row.
    pub(super) fn end_favorite_drag(&mut self, cx: &mut Context<Self>) {
        if self.favorite_drag.take().is_none() {
            return;
        }
        cx.global_mut::<FerretState>().save_settings();
        cx.notify();
    }
}

/// A sidebar group heading (§4): h20, 10.5 px, bold, uppercase, in
/// `--app-fg-faint` — muted foreground at 70% (§7).
fn section_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .h(px(20.))
        .mt(px(8.))
        .px(px(14.))
        .text_size(px(10.5))
        .line_height(px(20.))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(cx.theme().muted_foreground.opacity(0.7))
        .child(label.to_uppercase())
}
