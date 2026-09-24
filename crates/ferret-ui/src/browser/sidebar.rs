//! The sidebar: favourites, places and volumes, the rows that select
//! them, and the two drags it hosts — its own width, and reordering
//! favourites.

use super::*;
use ferret_remote::Connection;

/// Sidebar width bounds; mirrors `ferret-settings`' clamps so a drag can't
/// produce a width the settings file would refuse.
const MIN_SIDEBAR_WIDTH: f32 = ferret_settings::config::MIN_SIDEBAR_WIDTH;
const MAX_SIDEBAR_WIDTH: f32 = ferret_settings::config::MAX_SIDEBAR_WIDTH;

/// The one sidebar entry that describes where you are.
///
/// Every ancestor of the browsed directory matches some row — in
/// `~/Desktop/Coding`, Home, Desktop and a Coding favourite are all prefixes
/// of it — so lighting each prefix lights three rows at once and none of them
/// tells you anything. The deepest match is the only one that names the place
/// you are actually in, so it is the only one drawn as selected. Volumes and
/// drives still stay lit while you descend into them, because nothing deeper
/// claims the row.
fn current_place<'a>(
    current: &VfsPath,
    candidates: impl Iterator<Item = &'a Path>,
) -> Option<PathBuf> {
    let cur = current.as_local()?;
    candidates
        .filter(|place| cur.starts_with(place))
        .max_by_key(|place| place.components().count())
        .map(Path::to_path_buf)
}

/// Decelerating curve for the slide: fastest at the start, easing into place.
///
/// A linear slide of this length reads as mechanical; front-loading the motion
/// is what makes a 170 ms panel feel like it was already on its way when you
/// pressed the key.
fn ease_out_cubic(t: f32) -> f32 {
    let inverse = 1. - t.clamp(0., 1.);
    1. - inverse * inverse * inverse
}

impl Browser {
    /// How wide the sidebar column is right now, rather than how wide it is
    /// heading for. Zero once it is fully shut.
    ///
    /// The title bar lines its nav group up against this, so the toolbar
    /// moves with the panel instead of snapping to its final place on the
    /// first frame of the slide.
    pub(super) fn sidebar_reveal_width(&self) -> f32 {
        self.sidebar_width * self.sidebar_reveal
    }

    /// Slide the sidebar towards whatever `sidebar_visible` now says.
    ///
    /// Driven from a task rather than GPUI's `with_animation` because the
    /// title bar has to read the same live width: an element animation hands
    /// its progress to one element's closure and nothing else can see it.
    pub(super) fn animate_sidebar(&mut self, cx: &mut Context<Self>) {
        let target = if self.sidebar_visible { 1. } else { 0. };
        let from = self.sidebar_reveal;
        if (target - from).abs() < f32::EPSILON {
            self.sidebar_animation = None;
            return;
        }
        // A toggle mid-slide starts from where the panel actually is, so the
        // remaining distance is covered in the remaining time rather than
        // replaying the whole sweep.
        let span = SIDEBAR_ANIMATION.mul_f32((target - from).abs());
        let started = Instant::now();

        // Assigning the new task drops the old one, which cancels it.
        self.sidebar_animation = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SIDEBAR_ANIMATION_TICK).await;
                let done = this
                    .update(cx, |this, cx| {
                        let t = (started.elapsed().as_secs_f32() / span.as_secs_f32()).min(1.);
                        this.sidebar_reveal = from + (target - from) * ease_out_cubic(t);
                        cx.notify();
                        t >= 1.
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        }));
    }

    /// The hairline on the sidebar's right edge, and the handle that resizes
    /// it.
    ///
    /// The line is what separates the two regions and the grab area is drawn
    /// over it, spilling either side without taking any layout width — so the
    /// pane still begins one pixel after the sidebar ends while the divider
    /// stays something a pointer can actually catch.
    pub(super) fn render_sidebar_divider(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let line = cx.theme().border;
        let grabbed = cx.theme().primary.opacity(0.35);
        div()
            .relative()
            .w(px(SIDEBAR_DIVIDER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(line)
            .child(
                div()
                    .id("sidebar-divider")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(-SIDEBAR_GRAB_SLOP))
                    .right(px(-SIDEBAR_GRAB_SLOP))
                    .cursor(gpui::CursorStyle::ResizeLeftRight)
                    .hover(|el| el.bg(grabbed))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                            this.sidebar_drag = Some(SidebarDrag {
                                start_x: event.position.x,
                                start_width: this.sidebar_width,
                            });
                            cx.notify();
                        }),
                    ),
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
        state.touch_settings();
        crate::state::arm_config_save(cx);
        cx.notify();
    }

    pub(super) fn render_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let dir = self.active().folder.read(cx).dir.clone();

        // `settings.sidebar.show_places` / `show_volumes`: a group turned off
        // is not gathered at all, so it costs nothing rather than being drawn
        // and hidden. Favourites have no toggle — they are only there because
        // the user put them there.
        let (show_places, show_volumes) = {
            let sidebar = &cx.global::<FerretState>().settings.sidebar;
            (sidebar.show_places, sidebar.show_volumes)
        };

        // Gathered up front so the selected row can be picked against every
        // candidate at once rather than section by section.
        let places = if show_places {
            self.places()
        } else {
            Vec::new()
        };
        let favorites = self.favorites(cx);
        let volumes = if show_volumes {
            self.volumes()
        } else {
            Vec::new()
        };
        // Connection records are small and bounded by what a person saved.
        // Clone them before building rows so the render does not hold a global
        // borrow while installing event handlers.
        let connections = cx
            .global::<FerretState>()
            .connections
            .connections()
            .to_vec();
        let current = current_place(
            &dir,
            places
                .iter()
                .map(|p| p.path.as_path())
                .chain(favorites.iter().map(PathBuf::as_path))
                .chain(volumes.iter().map(|v| v.path.as_path())),
        );
        let is_current = |path: &Path| current.as_deref() == Some(path);

        let theme = cx.theme();
        let focused = self.sidebar_focus.is_focused(window);
        let mut side = v_flex()
            .id("sidebar")
            .key_context(actions::context::SIDEBAR)
            .track_focus(&self.sidebar_focus)
            .overflow_y_scroll()
            .w_full()
            .h_full()
            .bg(theme.sidebar)
            // The edge against the pane belongs to the divider, not here —
            // drawing it on both would double the line.
            .when(focused, |el| {
                // Focus still has to be visible (§8), so it borrows the
                // accent rather than a border that is always there.
                el.border_r_2().border_color(theme.primary)
            })
            .pt(px(4.))
            .pb(px(10.));

        if !places.is_empty() {
            side = side.child(section_label("Places", cx));
        }
        for (i, place) in places.into_iter().enumerate() {
            let active = is_current(&place.path);
            side = side.child(self.sidebar_place_row(
                ("sidebar-place", i),
                &place.name,
                place.kind.icon_name(),
                place.path.clone(),
                active,
                None,
                cx,
            ));
        }

        // A heading over nothing advertises an empty room. Favourites and
        // Volumes appear only once they have something in them; Places is
        // always populated, so it always shows.
        if !favorites.is_empty() {
            side = side.child(section_label("Favorites", cx));
        }
        for (i, fav) in favorites.into_iter().enumerate() {
            let name = fav
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| fav.to_string_lossy().into_owned());
            let active = is_current(&fav);
            side = side.child(self.sidebar_place_row(
                ("sidebar-favorite", i),
                &name,
                "icons/file/folder-base.svg",
                fav.clone(),
                active,
                Some((i, fav)),
                cx,
            ));
        }

        // Unlike the other optional groups, Connections keeps its heading
        // while empty: the trailing + is the discovery and creation affordance
        // shown by the product reference (F-20).
        side = side.child(connection_section_label(cx.weak_entity(), cx));
        for (i, connection) in connections.into_iter().enumerate() {
            let active =
                matches!(&dir, VfsPath::Remote { mount, .. } if *mount == connection.mount);
            side = side.child(self.sidebar_connection_row(
                ("sidebar-connection", i),
                connection,
                active,
                cx,
            ));
        }

        if !volumes.is_empty() {
            side = side.child(section_label("Volumes", cx));
        }
        for (i, volume) in volumes.into_iter().enumerate() {
            let active = is_current(&volume.path);
            side = side.child(self.sidebar_place_row(
                ("sidebar-volume", i),
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

    fn sidebar_connection_row(
        &self,
        id: impl Into<gpui::ElementId>,
        connection: Connection,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        use crate::mounts::MountStatus;

        let selected = crate::theme::BrowserPalette::resolve(cx).selected;
        let theme = cx.theme();
        let status = self.mount_status(&connection.id, cx);
        let foreground = if active {
            crate::theme::text_on(selected, theme.sidebar_foreground)
        } else {
            theme.sidebar_foreground
        };
        // State is carried by the icon rather than by a badge: the row is 22
        // logical pixels and a second element would crowd the name out.
        let icon_color = match &status {
            MountStatus::Connected => theme.primary,
            MountStatus::Failed(_) => theme.danger,
            _ => foreground.opacity(0.9),
        };
        // The subtitle is the one place a failure reason fits. `endpoint()`
        // returns when the row recovers, so nothing has to clear it.
        let (subtitle, subtitle_color) = match &status {
            MountStatus::Connecting => ("Connecting…".to_string(), foreground.opacity(0.65)),
            MountStatus::Failed(reason) => (reason.to_string(), theme.danger),
            _ => (connection.endpoint(), foreground.opacity(0.65)),
        };

        let this = cx.weak_entity();
        let clicked = connection.clone();
        let menu_record = connection.clone();
        let dropped_on = connection.clone();
        let connected = status.is_connected();
        let drop_tint = selected.opacity(0.5);
        let row = h_flex()
            .id(id.into())
            .w_full()
            .min_h(px(SIDEBAR_ROW_HEIGHT + 10.))
            .mx(px(8.))
            .rounded(px(6.))
            .px(px(10.))
            .gap(px(10.))
            .items_center()
            .when(active, |el| el.bg(selected))
            .when(!active, |el| el.hover(|el| el.bg(theme.secondary)))
            .when(status.is_busy(), |el| el.opacity(0.7))
            // "Drag to transfer": the same gesture that moves files between
            // local folders sends them to a server, with the destination
            // already answered by which row was hit.
            .drag_over::<crate::pane::DraggedPaths>(move |style, _, _, _| style.bg(drop_tint))
            .on_drop({
                let this = this.clone();
                move |dragged: &crate::pane::DraggedPaths, window, cx| {
                    let Some(browser) = this.upgrade() else {
                        return;
                    };
                    let sources = dragged.paths(cx);
                    let record = dropped_on.clone();
                    browser.update(cx, |browser, cx| {
                        browser.send_to_connection(record, sources, window, cx);
                    });
                }
            })
            .child(
                Icon::new(gpui_component::IconName::Globe)
                    .with_size(px(super::SIDEBAR_ICON_SIZE))
                    .text_color(icon_color),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(foreground)
                            .child(connection.display_name()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(subtitle_color)
                            .child(subtitle),
                    ),
            )
            // One click connects, and a second navigates without re-dialling —
            // `open_connection` decides which, because only it knows whether
            // the mount is live.
            .on_click({
                let this = this.clone();
                move |_: &ClickEvent, window, cx| {
                    if let Some(this) = this.upgrade() {
                        let record = clicked.clone();
                        this.update(cx, |this, cx| this.open_connection(record, window, cx));
                    }
                }
            });

        row.context_menu(move |menu, _, _| {
            let this = this.clone();
            let record = menu_record.clone();
            let menu = if connected {
                let disconnect = this.clone();
                let disconnect_record = record.clone();
                menu.item(PopupMenuItem::new("Disconnect").on_click(move |_, _, cx| {
                    if let Some(browser) = disconnect.upgrade() {
                        let record = disconnect_record.clone();
                        browser
                            .update(cx, |browser, cx| browser.disconnect_connection(&record, cx));
                    }
                }))
            } else {
                let connect = this.clone();
                let connect_record = record.clone();
                let root = this.clone();
                let root_record = record.clone();
                menu.item(
                    PopupMenuItem::new("Connect").on_click(move |_, window, cx| {
                        if let Some(browser) = connect.upgrade() {
                            let record = connect_record.clone();
                            browser.update(cx, |browser, cx| {
                                browser.open_connection(record, window, cx)
                            });
                        }
                    }),
                )
                // Ellipsis because it asks first, and what it asks for has to
                // be typed (F-23).
                .item(
                    PopupMenuItem::new("Connect as Root…").on_click(move |_, window, cx| {
                        if let Some(browser) = root.upgrade() {
                            let record = root_record.clone();
                            browser.update(cx, |browser, cx| {
                                browser.connect_as_root(record, window, cx)
                            });
                        }
                    }),
                )
            };
            let edit = this.clone();
            let edit_record = record.clone();
            let copy_command = record.ssh_command();
            let remove_record = record.clone();
            menu.separator()
                .item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                    if let Some(browser) = edit.upgrade() {
                        let record = edit_record.clone();
                        browser.update(cx, |browser, cx| {
                            browser.open_connection_dialog(Some(record), window, cx)
                        });
                    }
                }))
                .item(
                    PopupMenuItem::new("Copy SSH Command").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                            copy_command.clone(),
                        ));
                    }),
                )
                .separator()
                .item(PopupMenuItem::new("Remove Connection").on_click({
                    let this = this.clone();
                    move |_, _, cx| {
                        if let Some(browser) = this.upgrade() {
                            let record = remove_record.clone();
                            browser
                                .update(cx, |browser, cx| browser.remove_connection(&record, cx));
                        }
                    }
                }))
        })
    }

    /// One h22 pill row. `favorite` carries the row's place in the favourites
    /// list, which is what makes it removable and re-orderable.
    #[allow(clippy::too_many_arguments)]
    fn sidebar_place_row(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &str,
        icon: &'static str,
        target: PathBuf,
        active: bool,
        favorite: Option<(usize, PathBuf)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = crate::theme::BrowserPalette::resolve(cx).selected;
        let theme = cx.theme();
        let fg = if active {
            crate::theme::text_on(selected, theme.sidebar_foreground)
        } else {
            theme.sidebar_foreground
        };

        let drop_tint = selected.opacity(0.5);
        let this = cx.weak_entity();
        let dragging = favorite
            .as_ref()
            .is_some_and(|(ix, _)| self.favorite_drag == Some(*ix));
        let row = h_flex()
            .id(id.into())
            .w_full()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .mx(px(8.))
            .rounded(px(6.))
            .px(px(10.))
            .gap(px(10.))
            .items_center()
            .flex_shrink_0()
            .when(active, |el| el.bg(selected))
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
                    let (sources, dest) = (dragged.paths(cx), dest.clone());
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
                                state.touch_settings();
                            });
                            crate::state::arm_config_save(cx);
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
        cx.global_mut::<FerretState>().touch_settings();
        crate::state::arm_config_save(cx);
        cx.notify();
    }
}

/// A sidebar group heading (§4): h20, 10.5 px, bold, uppercase, in
/// `--app-fg-faint` — muted foreground at 70% (§7).
fn section_label(label: &'static str, cx: &App) -> impl IntoElement {
    div()
        .h(px(SIDEBAR_SECTION_HEIGHT))
        // Air above each group, and none below: the label belongs to the
        // rows under it, so it should sit closer to them than to the group
        // it follows.
        .mt(px(14.))
        .mb(px(2.))
        .px(px(14.))
        .text_size(px(11.))
        .line_height(px(SIDEBAR_SECTION_HEIGHT))
        // Sentence case at medium weight. Bold uppercase shouts a heading
        // that is only there to be scanned past.
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground.opacity(0.8))
        .child(label)
}

fn connection_section_label(this: WeakEntity<Browser>, cx: &App) -> impl IntoElement {
    h_flex()
        .h(px(SIDEBAR_SECTION_HEIGHT))
        .mt(px(14.))
        .mb(px(2.))
        .px(px(14.))
        .items_center()
        .justify_between()
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground.opacity(0.8))
        .child("Connections")
        .child(
            Button::new("new-connection")
                .ghost()
                .compact()
                .label("+")
                .on_click(move |_, window, cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |this, cx| this.open_connection_dialog(None, window, cx));
                    }
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn winner(current: &str, candidates: &[&str]) -> Option<String> {
        current_place(&VfsPath::local(current), candidates.iter().map(Path::new))
            .map(|p| p.to_string_lossy().into_owned())
    }

    #[test]
    fn only_the_deepest_matching_row_is_selected() {
        // Home, Desktop and the Coding favourite are all prefixes of the
        // browsed directory; lighting all three is the bug this guards.
        assert_eq!(
            winner(
                "/Users/krish/Desktop/Coding",
                &[
                    "/Users/krish",
                    "/Users/krish/Desktop",
                    "/Users/krish/Desktop/Coding"
                ],
            )
            .as_deref(),
            Some("/Users/krish/Desktop/Coding")
        );
    }

    #[test]
    fn an_ancestor_wins_when_nothing_deeper_matches() {
        // A volume stays lit while you descend into it, because no row
        // describes where you are more precisely.
        assert_eq!(
            winner(
                "/Volumes/Backup/2026/June",
                &["/Users/krish", "/Volumes/Backup"]
            )
            .as_deref(),
            Some("/Volumes/Backup")
        );
    }

    #[test]
    fn nothing_is_selected_outside_every_place() {
        assert_eq!(winner("/etc/ssh", &["/Users/krish"]), None);
    }
}
