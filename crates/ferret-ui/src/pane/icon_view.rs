//! Icon view (§4) and the rubber-band selection both it and the list use.
//!
//! Tiles sit on a fixed grid, which is what makes hit-testing a marquee
//! band a matter of arithmetic rather than a per-tile bounds check.

use super::*;

pub(super) struct Marquee {
    start: gpui::Point<gpui::Pixels>,
    current: gpui::Point<gpui::Pixels>,
}

impl FilePane {
    /// Icon-view geometry (§4): 92 px tiles, 48 px glyph, 6 px gaps, 12 px
    /// padding.
    const TILE_W: f32 = 92.;
    const TILE_GLYPH: f32 = 48.;
    const TILE_H: f32 = 96.;
    const GAP: f32 = 6.;
    const PAD: f32 = 12.;

    pub(super) fn tiles_per_row(&self) -> usize {
        let width = self.pane_width.load(Ordering::Relaxed) as f32;
        (((width - Self::PAD * 2. + Self::GAP) / (Self::TILE_W + Self::GAP)).floor() as usize)
            .max(1)
    }
}

impl FilePane {
    pub(super) fn render_icon_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.folder.read(cx).row_count();
        let rows = n.div_ceil(self.tiles_per_row());

        div()
            .id("icon-area")
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.start_marquee(event, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                this.update_marquee(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.marquee.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                gpui::uniform_list(
                    "icon-rows",
                    rows,
                    cx.processor(
                        move |this, row_range: std::ops::Range<usize>, _window, cx| {
                            let n = this.folder.read(cx).row_count();
                            let per_row = this.tiles_per_row();
                            row_range
                                .map(|row| {
                                    let start = row * per_row;
                                    let end = (start + per_row).min(n);
                                    let mut tiles = Vec::with_capacity(end - start);
                                    for ix in start..end {
                                        tiles.push(this.render_tile(ix, cx));
                                    }
                                    h_flex()
                                        .w_full()
                                        .px(px(Self::PAD))
                                        .gap(px(Self::GAP))
                                        .children(tiles)
                                        .into_any_element()
                                })
                                .collect()
                        },
                    ),
                )
                .size_full()
                .track_scroll(&self.icon_scroll),
            )
    }

    fn render_tile(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.folder.read(cx).entry_at(ix).cloned() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let palette = BrowserPalette::resolve(cx);
        let selected = self.folder.read(cx).is_row_selected(ix);
        let focused = self.pane_focused.load(Ordering::Relaxed);
        let broken = entry.kind.is_broken_symlink();

        let icon_color = if broken {
            theme.warning
        } else if entry.is_dir_like() {
            palette.folder
        } else {
            theme.muted_foreground
        };
        let label_color = if selected && focused {
            crate::theme::text_on(palette.selected, theme.foreground)
        } else {
            theme.foreground
        };
        let fill = if selected {
            if focused {
                palette.selected
            } else {
                palette.selected_idle
            }
        } else {
            gpui::transparent_black()
        };

        let tile_icon = entry_icon(&entry, Self::TILE_GLYPH, icon_color, cx);
        let dragged: Arc<[VfsPath]> = if selected {
            self.folder.read(cx).selected_paths_shared()
        } else {
            Arc::from([entry.path.clone()])
        };
        let tile_drop_dir = entry.is_dir_like().then(|| entry.path.clone());
        let drops = self.drops.clone();
        let accent = theme.primary;
        // Taken before the listeners below capture `entry`. Cheap: a name
        // short enough to live inline in a `SmolStr` copies rather than
        // allocating, which is most of them.
        let label = gpui::SharedString::new(entry.name());
        let menu_path = entry.path.clone();

        div()
            .id(("tile", ix))
            .w(px(Self::TILE_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(5.))
            .p(px(8.))
            .pt(px(10.))
            .rounded(px(6.))
            .bg(fill)
            .when(!selected, |tile| {
                tile.hover(|tile| tile.bg(theme.secondary))
            })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                this.tile_clicked(ix, event, cx);
            }))
            // Same as the list: the label is derived when a drag starts, not
            // once per tile per frame (A-02a).
            .on_drag(DraggedPaths { paths: dragged }, move |dragged, _, _, cx| {
                let label = drag_label(&dragged.paths);
                cx.new(|_| DragPreview { label })
            })
            .external_drag_payload({
                let folder = self.folder.clone();
                move |dragged: &DraggedPaths, _, cx| external_payload(dragged, folder.read(cx))
            })
            .when_some(tile_drop_dir, |tile, dest| {
                let drops = drops.clone();
                tile.drag_over::<DraggedPaths>(move |style, _, _, _| {
                    style.border_1().border_color(accent)
                })
                .on_drop(move |dragged: &DraggedPaths, window, _| {
                    queue_drop(&drops, dragged, dest.clone(), window);
                })
            })
            .child(tile_icon)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    // Select under the cursor, then open the target menu.
                    let path = menu_path.clone();
                    this.folder.update(cx, |f, cx| {
                        f.prepare_context_menu(ix);
                        cx.emit(FolderEvent::SelectionChanged);
                    });
                    this.open_menu_at(Some(path), event, window, cx);
                }),
            )
            .child(
                div()
                    .w_full()
                    .line_clamp(2)
                    .text_size(px(11.5))
                    .line_height(px(14.))
                    .text_center()
                    .text_color(label_color)
                    .when(broken, |text| text.line_through())
                    .child(label),
            )
            .into_any_element()
    }

    /// Pointer selection on a tile: plain replaces, Cmd/Ctrl toggles,
    /// Shift extends — identical semantics to the list.
    fn tile_clicked(&mut self, ix: usize, event: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.folder.update(cx, |f, _| f.extend_select(ix));
        } else if modifiers.platform || modifiers.control {
            self.folder.update(cx, |f, cx| {
                f.toggle_select(ix);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else {
            self.folder.update(cx, |f, cx| {
                f.select_only(ix);
                cx.emit(FolderEvent::SelectionChanged);
            });
        }
        if event.click_count() >= 2 {
            let entry = self.folder.read(cx).entry_at(ix).cloned();
            if let Some(entry) = entry {
                let ev = if entry.is_dir_like() {
                    PaneEvent::EnterDirectory(entry.path.clone())
                } else {
                    PaneEvent::OpenFile(entry.path.clone())
                };
                cx.emit(ev);
            }
        }
        self.sync_table_cursor(cx);
        cx.notify();
    }

    // -- marquee ------------------------------------------------------------

    /// Marquee in list view (§8: "expected in list view").
    ///
    /// Starts only below the last row. A drag that begins *on* a row is the
    /// start of a file drag, not a selection band — which is what both
    /// Finder and Explorer do, and what §8's "Drag | Rows" entry describes.
    pub(super) fn start_list_marquee(
        &mut self,
        event: &gpui::MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        if self.list_row_at(event.position, cx).is_some() {
            return;
        }
        self.begin_marquee(event.position, cx);
    }

    /// Which list row sits under a window-space point, if any.
    fn list_row_at(
        &self,
        position: gpui::Point<gpui::Pixels>,
        cx: &Context<Self>,
    ) -> Option<usize> {
        let origin = *self.pane_origin.lock();
        let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
            &self.table.read(cx).vertical_scroll_handle,
        );
        let local_y = position.y - origin.y - px(COLUMN_HEADER_HEIGHT) - scrolled.y;
        if local_y < px(0.) {
            return None; // In the header.
        }
        let row = (f32::from(local_y) / ROW_HEIGHT).floor() as usize;
        (row < self.folder.read(cx).row_count()).then_some(row)
    }

    /// Rows whose bands intersect the marquee, in list view.
    fn list_rows_in_band(
        &self,
        top: gpui::Pixels,
        bottom: gpui::Pixels,
        cx: &Context<Self>,
    ) -> Vec<usize> {
        let n = self.folder.read(cx).row_count();
        if n == 0 {
            return Vec::new();
        }
        let origin = *self.pane_origin.lock();
        let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
            &self.table.read(cx).vertical_scroll_handle,
        );
        let to_row = |y: gpui::Pixels| {
            let local = y - origin.y - px(COLUMN_HEADER_HEIGHT) - scrolled.y;
            (f32::from(local) / ROW_HEIGHT).floor() as isize
        };
        let first = to_row(top).clamp(0, n as isize - 1) as usize;
        let last = to_row(bottom).clamp(0, n as isize - 1) as usize;
        (first..=last).collect()
    }

    fn begin_marquee(&mut self, at: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        self.marquee = Some(Marquee {
            start: at,
            current: at,
        });
        self.folder.update(cx, |f, cx| {
            f.clear_selection();
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    fn start_marquee(&mut self, event: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        // Only start on empty space: a tile under the cursor claims it first
        // (its own handler runs in the inner bubble phase and marks the
        // folder's selection).
        let n = self.folder.read(cx).row_count();
        if self.tile_at(event.position, n).is_some() {
            return;
        }
        self.marquee = Some(Marquee {
            start: event.position,
            current: event.position,
        });
        self.folder.update(cx, |f, cx| {
            f.clear_selection();
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    pub(super) fn update_marquee(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(marquee) = self.marquee.as_ref().map(|m| (m.start, m.current)) else {
            return;
        };
        let (start, current) = marquee;
        let delta = event.position - current;
        if delta.x.abs() < gpui::px(1.) && delta.y.abs() < gpui::px(1.) {
            return;
        }
        self.marquee = Some(Marquee {
            start,
            current: event.position,
        });

        // A list row spans the full width, so only the vertical band matters.
        if self.view_mode == ViewMode::List {
            let top = start.y.min(event.position.y);
            let bottom = start.y.max(event.position.y);
            let hits = self.list_rows_in_band(top, bottom, cx);
            self.folder.update(cx, |f, cx| {
                f.select_rows(hits);
                cx.emit(FolderEvent::SelectionChanged);
            });
            cx.notify();
            return;
        }

        // Live-select every tile intersecting the band (content coordinates).
        let origin = *self.pane_origin.lock();
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll);
        let to_content = |p: gpui::Point<gpui::Pixels>| {
            gpui::point(p.x - origin.x + offset.x, p.y - origin.y + offset.y)
        };
        let a = to_content(start);
        let b = to_content(event.position);
        let left = a.x.min(b.x);
        let right = a.x.max(b.x);
        let top = a.y.min(b.y);
        let bottom = a.y.max(b.y);

        let per_row = self.tiles_per_row();
        let n = self.folder.read(cx).row_count();
        let col_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first = (((lo - px(Self::PAD) + px(Self::GAP)) / px(Self::TILE_W + Self::GAP))
                .floor() as isize)
                .clamp(0, per_row as isize - 1);
            let last = (((hi - px(Self::PAD)) / px(Self::TILE_W + Self::GAP)).floor() as isize)
                .clamp(0, per_row as isize - 1);
            first..=last
        };
        let row_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first =
                (((lo - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor() as isize).max(0);
            let last =
                (((hi - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor() as isize).max(0);
            first..=last
        };

        let mut hits = Vec::new();
        if right >= left && bottom >= top {
            for r in row_range(top, bottom) {
                for c in col_range(left, right) {
                    let ix = (r as usize) * per_row + c as usize;
                    if ix < n {
                        hits.push(ix);
                    }
                }
            }
        }
        self.folder.update(cx, |f, cx| {
            f.select_rows(hits);
            cx.emit(FolderEvent::SelectionChanged);
        });
        cx.notify();
    }

    fn tile_at(&self, position: gpui::Point<gpui::Pixels>, n: usize) -> Option<usize> {
        let origin = *self.pane_origin.lock();
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll);
        let local = gpui::point(
            position.x - origin.x + offset.x,
            position.y - origin.y + offset.y,
        );
        let per_row = self.tiles_per_row();
        let col = (((local.x - px(Self::PAD) + px(Self::GAP)) / px(Self::TILE_W + Self::GAP))
            .floor()) as isize;
        let row = (((local.y - px(Self::PAD)) / px(Self::TILE_H + Self::GAP)).floor()) as isize;
        if col < 0 || row < 0 || col as usize >= per_row {
            return None;
        }
        let ix = row as usize * per_row + col as usize;
        (ix < n).then_some(ix)
    }

    pub(super) fn marquee_overlay(&self) -> gpui::AnyElement {
        let Some(marquee) = &self.marquee else {
            return div().into_any_element();
        };
        let origin = *self.pane_origin.lock();
        let left = marquee.start.x.min(marquee.current.x) - origin.x;
        let top = marquee.start.y.min(marquee.current.y) - origin.y;
        let width = (marquee.start.x - marquee.current.x).abs();
        let height = (marquee.start.y - marquee.current.y).abs();

        div()
            .absolute()
            .left(left)
            .top(top)
            .w(width)
            .h(height)
            .border_1()
            .border_color(gpui::rgb(0x0a6cff))
            .bg(gpui::rgb(0x0a6cff).opacity(0.12))
            .into_any_element()
    }
}
