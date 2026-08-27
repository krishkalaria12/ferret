//! The pane's root element: the per-frame hand-offs its listeners queue
//! up, the actions it binds, and which of the three views it mounts.

use super::*;

impl Render for FilePane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The slow-click timer has no window to focus an editor with, so it
        // leaves the request here and the next frame starts the rename.
        if let Some(path) = self.pending_rename.take() {
            self.begin_rename(path, window, cx);
        }
        // Drops arrive from row listeners that cannot reach the browser; this
        // is where they become an event it can act on.
        if let Some(row) = self.toggles.lock().unwrap().take() {
            self.folder.update(cx, |f, cx| f.toggle_expanded(row, cx));
        }
        let sizing: Vec<FileColumn> = std::mem::take(&mut self.sizing_requests.lock().unwrap());
        for column in sizing {
            self.size_column_to_content(column, window, cx);
        }
        if let Some(drop) = self.drops.lock().unwrap().take() {
            cx.emit(PaneEvent::DropPaths {
                sources: drop.sources,
                dest: drop.dest,
                force_copy: drop.force_copy,
            });
        }

        let rename_overlay = self.render_rename_overlay(cx);

        let width_probe = self.pane_width.clone();
        let width_probe_owner = cx.weak_entity();
        // `size_full` is load-bearing: a GPUI div is `display: block`, so
        // without it this wrapper shrinks to its content, the table gets no
        // definite height, and a virtualised list with no height paints no
        // rows at all — silently, and only at runtime.
        let table = div()
            .size_full()
            .on_children_prepainted(move |children: Vec<gpui::Bounds<gpui::Pixels>>, _, cx| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let w = f32::from(bounds.size.width) as usize;
                if width_probe.swap(w, Ordering::Relaxed) != w
                    && let Some(pane) = width_probe_owner.upgrade()
                {
                    pane.update(cx, |pane, cx| pane.table.update(cx, |_, cx| cx.notify()));
                }
            })
            .child(
                DataTable::new(&self.table)
                    .bordered(false)
                    .with_size(px(ROW_HEIGHT)),
            );

        // Menu placement and marquee hit-testing work in pane-local space,
        // but mouse events arrive in window space; this is the offset between
        // them, and it is only knowable after layout.
        let origin = self.pane_origin.clone();

        // Empty space in the pane is the browsed directory itself, so items
        // dragged in from another window or tab land here.
        let pane_dir = self.folder.read(cx).dir.clone();
        let pane_drops = self.drops.clone();

        div()
            .id("file-pane")
            .relative()
            .flex_1()
            .min_h_0()
            .key_context(actions::context::FILE_PANE)
            .track_focus(&self.focus)
            .on_drop(move |dragged: &DraggedPaths, window, _| {
                queue_drop(&pane_drops, dragged, pane_dir.clone(), window);
            })
            .on_prepaint(move |bounds, _, _| {
                *origin.lock().unwrap() = bounds.origin;
            })
            .on_key_down(cx.listener(Self::handle_typeahead))
            .on_action(cx.listener(|this, _: &actions::Open, _, cx| {
                this.open_selection(cx);
            }))
            .on_action(cx.listener(|this, _: &actions::Rename, window, cx| {
                let target = this.folder.read(cx).cursor_path();
                if let Some(path) = target {
                    this.begin_rename(path, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &actions::CancelEdit, _, cx| {
                this.cancel_rename(cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SelectAll, _, cx| {
                this.folder.update(cx, |f, cx| {
                    f.select_all();
                    cx.emit(FolderEvent::SelectionChanged);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &actions::SortByName, _, cx| {
                this.apply_sort(SortKey::Name, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortByModified, _, cx| {
                this.apply_sort(SortKey::Modified, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortBySize, _, cx| {
                this.apply_sort(SortKey::Size, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortAscending, _, cx| {
                this.apply_order(SortOrder::Ascending, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortDescending, _, cx| {
                this.apply_order(SortOrder::Descending, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorUp, _, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorDown, _, cx| {
                this.move_cursor(1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::ToggleColumnModified, _, cx| {
                    this.toggle_column(FileColumn::Modified, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ToggleColumnSize, _, cx| {
                this.toggle_column(FileColumn::Size, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ExtendUp, _, cx| {
                this.extend_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ExtendDown, _, cx| {
                this.extend_cursor(1, cx);
            }))
            // Cmd↓ / Alt↓ descends into the cursor row when it is a folder,
            // the mirror of Cmd↑ going to the parent (§8).
            .on_action(cx.listener(|this, _: &actions::EnterDirectory, _, cx| {
                let folder = this.folder.read(cx);
                if let Some(path) = folder.cursor_path()
                    && folder.is_dir_like(&path) == Some(true)
                {
                    cx.emit(PaneEvent::EnterDirectory(path));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::CursorLeft, _, cx| {
                this.move_cursor_horizontal(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::CursorRight, _, cx| {
                this.move_cursor_horizontal(1, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_space_menu(event, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    // A left click between right-clicks clears the table's
                    // stale row target, so an empty-area right-click can't
                    // replay an old row menu.
                    this.table
                        .update(cx, |t, cx| t.set_right_clicked_row(None, cx));
                    if this.space_menu.take().is_some() {
                        cx.notify();
                    }
                    if this.view_mode == ViewMode::List {
                        this.start_list_marquee(event, cx);
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.view_mode == ViewMode::List {
                    this.update_marquee(event, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.marquee.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .when(self.view_mode == ViewMode::List, |el| {
                el.child(table)
                    .children(self.render_loading_bar(cx))
                    .child(self.marquee_overlay())
            })
            .when(self.view_mode == ViewMode::Icons, |el| {
                el.child(self.render_icon_view(cx))
            })
            .when(self.view_mode == ViewMode::Columns, |el| {
                el.child(self.render_column_view(cx))
            })
            .when_some(rename_overlay, |el, overlay| el.child(overlay))
            .when_some(
                self.space_menu_overlay(cx).into_any_element().into(),
                |el, overlay| el.child(overlay),
            )
            .when(self.view_mode == ViewMode::Icons, |el| {
                let overlay = self.marquee_overlay();
                el.child(overlay)
            })
    }
}
