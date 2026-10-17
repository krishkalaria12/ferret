//! Icon view (§4) and the rubber-band selection both it and the list use.
//!
//! Tiles sit on a fixed grid, which is what makes hit-testing a marquee
//! band a matter of arithmetic rather than a per-tile bounds check.

use super::*;

pub(super) struct Marquee {
    start: gpui::Point<gpui::Pixels>,
    current: gpui::Point<gpui::Pixels>,
}

struct IconGeometry {
    width: gpui::Pixels,
    height: gpui::Pixels,
    glyph: gpui::Pixels,
    gap: gpui::Pixels,
    padding: gpui::Pixels,
}

impl FilePane {
    /// Icon-view geometry (§4): 92 px tiles, 48 px glyph, 6 px gaps, 12 px
    /// padding.
    const TILE_W: f32 = 92.;
    const TILE_GLYPH: f32 = 48.;
    const TILE_H: f32 = 96.;
    const GAP: f32 = 6.;
    const PAD: f32 = 12.;

    pub(super) fn tiles_per_row(&self, cx: &App) -> usize {
        let geometry = Self::icon_geometry(cx);
        let width = self.pane_width.load(Ordering::Relaxed) as f32;
        (((width - f32::from(geometry.padding) * 2. + f32::from(geometry.gap))
            / (f32::from(geometry.width) + f32::from(geometry.gap)))
        .floor() as usize)
            .max(1)
    }

    fn icon_geometry(cx: &App) -> IconGeometry {
        let base = cx.theme().font_size;
        IconGeometry {
            width: rem_from_ui_px(Self::TILE_W).to_pixels(base),
            height: rem_from_ui_px(Self::TILE_H).to_pixels(base),
            glyph: rem_from_ui_px(Self::TILE_GLYPH).to_pixels(base),
            gap: rem_from_ui_px(Self::GAP).to_pixels(base),
            padding: rem_from_ui_px(Self::PAD).to_pixels(base),
        }
    }
}

impl FilePane {
    pub(super) fn render_icon_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let geometry = Self::icon_geometry(cx);
        let n = self.folder.read(cx).row_count();
        let rows = n.div_ceil(self.tiles_per_row(cx));
        self.maybe_warm_thumbs(n, rows, cx);

        let height_probe = self.pane_height.clone();
        div()
            .size_full()
            .on_children_prepainted(move |children: Vec<gpui::Bounds<gpui::Pixels>>, _, _| {
                let Some(bounds) = children.first() else {
                    return;
                };
                height_probe.store(f32::from(bounds.size.height) as usize, Ordering::Relaxed);
            })
            .id("icon-area")
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
                            let per_row = this.tiles_per_row(cx);
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
                                        .px(geometry.padding)
                                        .gap(geometry.gap)
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
        let geometry = Self::icon_geometry(cx);
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

        // The badge rides on the glyph rather than beside the label: a tile's
        // label is centred and wraps to two lines, so anything appended to it
        // moves with the text instead of staying where the eye expects it.
        // A resolved photo tile takes the glyph's place; the badge rides it
        // the same way.
        let inner = match self.thumb_tile(&entry, cx) {
            Some(tile) => tile,
            None => entry_icon(&entry, f32::from(geometry.glyph), icon_color, cx),
        };
        let tile_icon = match self.git_status(&entry) {
            Some(status) => div()
                .relative()
                .flex_shrink_0()
                .child(inner)
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .right_0()
                        .child(super::git_badge(status, entry.is_dir_like(), &palette)),
                )
                .into_any_element(),
            None => inner,
        };
        let dragged = if selected {
            Some(DraggedPaths::selected(&self.folder, cx))
        } else {
            DraggedPaths::row(&self.folder, ix, cx)
        };
        let tile_drop_dir = entry.is_dir_like().then(|| entry.path.clone());
        let drops = self.drops.clone();
        let accent = theme.primary;
        // Taken before the listeners below capture `entry`. Cheap: a name
        // short enough to live inline in a `SmolStr` copies rather than
        // allocating, which is most of them.
        let label = gpui::SharedString::new(entry.name());
        let menu_target = (entry.path.clone(), super::symlink_target_kind(entry.kind));

        div()
            .id(("tile", ix))
            .w(geometry.width)
            .h(geometry.height)
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .gap(geometry.gap)
            .p(rem_from_ui_px(7.).to_pixels(theme.font_size))
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
            .when_some(dragged, |tile, dragged| {
                tile.on_drag(dragged, move |dragged, _, _, cx| {
                    let label = drag_label(&dragged.paths(cx));
                    cx.new(|_| DragPreview { label })
                })
                .external_drag_payload(move |dragged: &DraggedPaths, _, cx| {
                    external_payload(dragged, cx)
                })
            })
            .when_some(tile_drop_dir, |tile, dest| {
                let drops = drops.clone();
                tile.drag_over::<DraggedPaths>(move |style, _, _, _| {
                    style.border_1().border_color(accent)
                })
                .on_drop(move |dragged: &DraggedPaths, window, cx| {
                    queue_drop(&drops, dragged, dest.clone(), window, cx);
                })
            })
            .child(tile_icon)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    // Select under the cursor, then open the target menu.
                    let path = menu_target.clone();
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
                    .text_size(rem_from_ui_px(11.5))
                    .line_height(rem_from_ui_px(14.))
                    .text_center()
                    .text_color(label_color)
                    .when(broken, |text| text.line_through())
                    .child(label),
            )
            .into_any_element()
    }

    /// A resolved photo tile, fitted inside the glyph box with aspect
    /// preserved — or `None`, in which case the tile draws the kind glyph.
    ///
    /// Memory-cache only: one lock-free lookup and no I/O, which is what
    /// keeps the render path O(visible tiles). Warming happens in
    /// [`Self::maybe_warm_thumbs`], off the path.
    fn thumb_tile(&self, entry: &ferret_vfs::Entry, cx: &gpui::App) -> Option<gpui::AnyElement> {
        if !cx.global::<FerretState>().settings.preview.thumbnails {
            return None;
        }
        let cached = cx.global::<FerretState>().rendered_thumbs.get(entry)?;
        let glyph = rem_from_ui_px(Self::TILE_GLYPH).to_pixels(cx.theme().font_size);
        let scale =
            (f32::from(glyph) / cached.width as f32).min(f32::from(glyph) / cached.height as f32);
        Some(
            gpui::img(gpui::ImageSource::Render(cached.image))
                .w(px(cached.width as f32 * scale))
                .h(px(cached.height as f32 * scale))
                .flex_shrink_0()
                .into_any_element(),
        )
    }

    /// Resolve tiles for the visible range plus one viewport of lookahead.
    ///
    /// Runs once per range change, never per frame: the range is arithmetic
    /// on the scroll offset and the probed height, and a new range replaces
    /// the previous task, which is what cancels the stale one. Each tile is
    /// memory-hit, disk-hit, or one bounded generate; failures keep the
    /// glyph silently — a tile is never worth an error surface.
    fn maybe_warm_thumbs(&mut self, n: usize, rows: usize, cx: &mut Context<Self>) {
        if self.view_mode != ViewMode::Icons
            || !cx.global::<FerretState>().settings.preview.thumbnails
        {
            self.thumb_task = None;
            return;
        }
        let height = self.pane_height.load(Ordering::Relaxed) as f32;
        if height <= 0.0 || n == 0 {
            return;
        }
        let geometry = Self::icon_geometry(cx);
        let per_row = self.tiles_per_row(cx);
        let stride = f32::from(geometry.height) + f32::from(geometry.gap);
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll).y;
        let first_row = (f32::from(offset) / stride).floor().max(0.0) as usize;
        let visible_rows = (height / stride).ceil() as usize;
        // One viewport of lookahead: the next scroll step finds warm tiles.
        let last_row = (first_row + visible_rows * 2).min(rows.saturating_sub(1));
        let range = first_row * per_row..(last_row * per_row + per_row).min(n);
        // Compare before cloning: the common scroll step keeps the directory
        // and only moves the range, so no path clone is owed per frame.
        let same_range = range == self.thumb_range;
        let same_dir = self
            .thumb_dir
            .as_ref()
            .is_some_and(|cached| *cached == self.folder.read(cx).dir);
        if same_range && same_dir {
            return;
        }
        let dir = self.folder.read(cx).dir.clone();
        // A new directory drops the previous folder's tiles: memory holds the
        // visible working set, the disk cache holds the rest.
        if Some(&dir) != self.thumb_dir.as_ref() {
            cx.global::<FerretState>().rendered_thumbs.clear();
        }
        self.thumb_range = range.clone();
        self.thumb_dir = Some(dir);

        let entries: Vec<ferret_vfs::Entry> = range
            .filter_map(|ix| self.folder.read(cx).entry_at(ix).cloned())
            .collect();
        let fs = cx.global::<FerretState>().fs.clone();
        let cache = cx.global::<FerretState>().thumb_cache.clone();
        let rendered = Arc::clone(&cx.global::<FerretState>().rendered_thumbs);
        self.thumb_task = Some(cx.spawn(async move |pane, cx| {
            for entry in entries.into_iter().take(crate::thumbs::THUMB_WARM_CAP) {
                if entry.is_dir_like() {
                    continue;
                }
                // In-process decoders generate; everything the OS covers
                // (HEIC, PDF, video posters) asks the service — but only
                // where a service exists. On Windows the provider declines
                // until the supervised helper lands, so native formats keep
                // their glyphs without paying canonicalize + lookup + hop
                // per tile to learn the answer is `None`.
                let native = match entry.extension() {
                    Some(ext) if ferret_preview::is_tileable_extension(ext) => false,
                    Some(ext)
                        if ferret_preview::thumbs::is_native_tileable_extension(ext)
                            && ferret_platform::thumbnail_available() =>
                    {
                        true
                    }
                    _ => continue,
                };
                let Some(local) = entry.path.as_local() else {
                    continue;
                };
                let Some(meta) = entry.metadata else {
                    continue;
                };
                let Some((len, secs, nanos)) = ferret_preview::fingerprint(meta.len, meta.modified)
                else {
                    continue;
                };
                if rendered.get(&entry).is_some() {
                    continue;
                }
                // Blocking syscalls and the decode stay in this background
                // task; the window thread only sees the insert plus notify.
                let Ok(canonical) = std::fs::canonicalize(local) else {
                    continue;
                };
                let key = ferret_preview::thumb_key(
                    &canonical,
                    len,
                    secs,
                    nanos,
                    ferret_preview::config::THUMB_PX,
                );
                if let Some(bitmap) = cache.lookup(&key)
                    && let Some(thumb) = crate::thumbs::render_thumb(&bitmap, len, secs, nanos)
                {
                    let path = entry.path.clone();
                    let rendered = Arc::clone(&rendered);
                    let _ = pane.update(cx, |_, cx| {
                        rendered.insert(path, thumb);
                        cx.notify();
                    });
                    continue;
                }
                let Some(small) = (if native {
                    native_tile(&entry, &cache, &key, len).await
                } else {
                    generated_tile(&fs, &entry, &cache, &key).await
                }) else {
                    continue;
                };
                if let Some(thumb) = crate::thumbs::render_thumb(&small, len, secs, nanos) {
                    let path = entry.path.clone();
                    let rendered = Arc::clone(&rendered);
                    let _ = pane.update(cx, |_, cx| {
                        rendered.insert(path, thumb);
                        cx.notify();
                    });
                }
            }
        }));
    }

    /// Pointer selection on a tile: plain replaces, Cmd/Ctrl toggles,
    /// Shift extends — identical semantics to the list.
    fn tile_clicked(&mut self, ix: usize, event: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.folder.update(cx, |f, cx| {
                f.extend_select(ix);
                cx.emit(FolderEvent::SelectionChanged);
            });
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
        let row_height = rem_from_ui_px(ROW_HEIGHT).to_pixels(cx.theme().font_size);
        let local_y = position.y - origin.y - row_height - scrolled.y;
        if local_y < px(0.) {
            return None; // In the header.
        }
        let row = (f32::from(local_y) / f32::from(row_height)).floor() as usize;
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
        let row_height = rem_from_ui_px(ROW_HEIGHT).to_pixels(cx.theme().font_size);
        let to_row = |y: gpui::Pixels| {
            let local = y - origin.y - row_height - scrolled.y;
            (f32::from(local) / f32::from(row_height)).floor() as isize
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
        if self.tile_at(event.position, n, cx).is_some() {
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

        let geometry = Self::icon_geometry(cx);
        let per_row = self.tiles_per_row(cx);
        let n = self.folder.read(cx).row_count();
        let col_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first = (((lo - geometry.padding + geometry.gap) / (geometry.width + geometry.gap))
                .floor() as isize)
                .clamp(0, per_row as isize - 1);
            let last = (((hi - geometry.padding) / (geometry.width + geometry.gap)).floor()
                as isize)
                .clamp(0, per_row as isize - 1);
            first..=last
        };
        let row_range = |lo: gpui::Pixels, hi: gpui::Pixels| {
            let first = (((lo - geometry.padding) / (geometry.height + geometry.gap)).floor()
                as isize)
                .max(0);
            let last = (((hi - geometry.padding) / (geometry.height + geometry.gap)).floor()
                as isize)
                .max(0);
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

    fn tile_at(&self, position: gpui::Point<gpui::Pixels>, n: usize, cx: &App) -> Option<usize> {
        let origin = *self.pane_origin.lock();
        let offset = gpui_component::scroll::ScrollbarHandle::offset(&self.icon_scroll);
        let local = gpui::point(
            position.x - origin.x + offset.x,
            position.y - origin.y + offset.y,
        );
        let geometry = Self::icon_geometry(cx);
        let per_row = self.tiles_per_row(cx);
        let col = (((local.x - geometry.padding + geometry.gap) / (geometry.width + geometry.gap))
            .floor()) as isize;
        let row =
            (((local.y - geometry.padding) / (geometry.height + geometry.gap)).floor()) as isize;
        if col < 0 || row < 0 || col as usize >= per_row {
            return None;
        }
        let ix = row as usize * per_row + col as usize;
        (ix < n).then_some(ix)
    }

    pub(super) fn marquee_overlay(&self, cx: &App) -> gpui::AnyElement {
        let Some(marquee) = &self.marquee else {
            return div().into_any_element();
        };
        let origin = *self.pane_origin.lock();
        let accent = cx.theme().primary;
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
            .border_color(accent)
            .bg(accent.opacity(0.12))
            .into_any_element()
    }
}

/// Disk miss for an in-process format: one bounded read, the pane's capped
/// decode, first frame only — then store for the next visit.
///
/// A failed store still returns the bitmap for this session: the disk being
/// unwritable must not cost a re-decode every scroll.
async fn generated_tile(
    fs: &impl ferret_vfs::Vfs,
    entry: &ferret_vfs::Entry,
    cache: &ferret_preview::ThumbCache,
    key: &str,
) -> Option<ferret_preview::Bitmap> {
    let size = match entry.metadata {
        Some(metadata) => metadata.len,
        None => fs.metadata(&entry.path).await.ok()?.len,
    };
    if size > ferret_preview::config::IMAGE_BUDGET as u64 {
        return None;
    }
    let bytes = fs
        .read(&entry.path, ferret_preview::config::IMAGE_BUDGET)
        .await
        .ok()?;
    let decoded = ferret_preview::image::decode(&bytes).ok()?;
    let small = ferret_preview::ThumbCache::make(
        &decoded.frames.first()?.bitmap,
        ferret_preview::config::THUMB_PX,
    )?;
    cache.store(key, &small);
    Some(small)
}

/// Disk miss for an OS-covered format: one best-effort service question.
/// Declines at any stage are `None` and keep the glyph.
async fn native_tile(
    entry: &ferret_vfs::Entry,
    cache: &ferret_preview::ThumbCache,
    key: &str,
    len: u64,
) -> Option<ferret_preview::Bitmap> {
    if !ferret_platform::thumbnail_available() {
        return None;
    }
    let local = entry.path.as_local()?;
    if !ferret_platform::native_thumbnail_path_allowed(local) {
        return None;
    }
    if len > ferret_platform::config::NATIVE_THUMBNAIL_INPUT_LIMIT {
        return None;
    }
    let bitmap = ferret_platform::thumbnail_provider()
        .thumbnail(local, ferret_preview::config::THUMB_PX)
        .await
        .ok()??;
    if !bitmap.is_well_formed() {
        return None;
    }
    let bitmap = ferret_preview::Bitmap {
        width: bitmap.width,
        height: bitmap.height,
        has_transparency: ferret_preview::image::detect_transparency(&bitmap.bgra),
        bgra: bitmap.bgra,
    };
    cache.store(key, &bitmap);
    Some(bitmap)
}
