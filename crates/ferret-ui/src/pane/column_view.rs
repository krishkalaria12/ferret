//! Column view (§5): one listing per ancestor of the browsed directory,
//! with a metadata column at the right-hand end.
//!
//! The ancestor listings carry no selection of their own — they exist to
//! be read as a path, so each one draws the child that lies on it.

use super::*;

/// One ancestor listing in column view.
///
/// These are display-and-navigation only: they carry no selection of their
/// own, just the child that lies on the path to the browsed directory, drawn
/// selected so the chain reads as a path.
pub(super) struct AncestorColumn {
    folder: gpui::Entity<Folder>,
    on_path: VfsPath,
    scroll: gpui::UniformListScrollHandle,
    /// Lives here so clearing the chain also detaches its listing tasks.
    _subscription: Subscription,
}

/// Width of one listing column (§5). Wide enough for a typical file name at
/// 13 px without the chain becoming unscrollable on a 760 px window.
const COLUMN_WIDTH: f32 = 216.;
/// The rightmost column: metadata for the selection, never a file preview —
/// the preview pane is an addon.
const METADATA_WIDTH: f32 = 208.;
/// How many strict ancestors get their own column.
///
/// Each column is a real directory walk, so an unbounded chain would list
/// every ancestor of a deep path on arrival. Three is also what fits beside
/// the browsed directory and its metadata at the default window width, which
/// keeps the column you are actually in on screen without a sideways scroll.
const MAX_ANCESTOR_COLUMNS: usize = 3;

impl FilePane {
    /// Walk the ancestor chain into one [`Folder`] per column.
    ///
    /// Each ancestor gets the browsed folder's sort and hidden-file settings
    /// so the chain reads consistently, and keeps the child that lies on the
    /// path so it can be drawn selected.
    pub(super) fn build_ancestor_columns(&mut self, cx: &mut Context<Self>) {
        self.ancestors.clear();
        self.anchor_chain_right = true;

        let (dir, sort_key, sort_order, show_hidden) = self.folder.read_with(cx, |f, _| {
            (f.dir.clone(), f.sort_key(), f.sort_order(), f.show_hidden())
        });

        for (parent, on_path) in ancestor_chain(&dir, MAX_ANCESTOR_COLUMNS) {
            let folder = cx.new(|_| Folder::new(parent, sort_key, sort_order, show_hidden));
            folder.update(cx, |f, cx| f.start_loading(cx));
            let scroll = gpui::UniformListScrollHandle::new();
            // Repaint as each ancestor's rows land, and bring the on-path
            // child into view once they have: a chain scrolled to the top of
            // a 300-entry directory does not read as a path.
            let subscription = cx.subscribe(&folder, {
                let on_path = on_path.clone();
                let scroll = scroll.clone();
                move |_, folder, event: &FolderEvent, cx| {
                    if *event == FolderEvent::LoadFinished
                        && let Some(row) = folder.read(cx).row_of(&on_path)
                    {
                        scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
                    }
                    cx.notify();
                }
            });
            self.ancestors.push(AncestorColumn {
                folder,
                on_path,
                scroll,
                _subscription: subscription,
            });
        }
        cx.notify();
    }

    pub(super) fn render_column_view(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;

        // The chain is wider than the pane on any real path, so it scrolls
        // sideways; after a rebuild it lands on the right-hand end, which is
        // where the browsed directory and its metadata are.
        if self.anchor_chain_right {
            // The metadata column is the last child; scrolling it into view
            // brings the browsed directory with it.
            let last = self.ancestors.len() + 1;
            self.column_chain_scroll.scroll_to_item(last);
            // A scroll request needs child bounds, which only exist after the
            // chain has been laid out once — so ask again next frame until it
            // can take effect, then stop and leave the scroll to the user.
            if self.column_chain_scroll.bounds_for_item(last).is_some() {
                self.anchor_chain_right = false;
            } else {
                cx.notify();
            }
        }

        let mut columns = h_flex()
            .id("column-view")
            .size_full()
            .items_stretch()
            .overflow_x_scroll()
            .track_scroll(&self.column_chain_scroll);

        for ix in 0..self.ancestors.len() {
            columns = columns.child(self.render_ancestor_column(ix, cx));
        }
        columns
            .child(self.render_current_column(cx))
            .child(self.render_metadata_column(cx))
            .bg(background)
    }

    /// An ancestor listing: read-only, with the on-path child highlighted.
    /// Clicking any row moves the browser to that row's directory, which
    /// re-derives the whole chain.
    fn render_ancestor_column(&mut self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(column) = self.ancestors.get(ix) else {
            return div().into_any_element();
        };
        let folder = column.folder.clone();
        let on_path = column.on_path.clone();
        let scroll = column.scroll.clone();
        let count = folder.read(cx).row_count();

        column_frame(cx)
            .child(
                gpui::uniform_list(
                    ("ancestor-column", ix),
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|row| {
                                let Some(entry) = folder.read(cx).entry_at(row).cloned() else {
                                    return div().into_any_element();
                                };
                                let selected = entry.path == on_path;
                                let target = entry.path.clone();
                                let is_dir = entry.is_dir_like();
                                this.column_row(("ancestor-row", row), &entry, selected, cx)
                                    .on_click(cx.listener(move |_, _: &gpui::ClickEvent, _, cx| {
                                        // A file's own column is its
                                        // parent, which is where clicking
                                        // it lands you.
                                        let dir = if is_dir {
                                            target.clone()
                                        } else {
                                            match target.parent() {
                                                Some(parent) => parent,
                                                None => return,
                                            }
                                        };
                                        cx.emit(PaneEvent::EnterDirectory(dir));
                                    }))
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&scroll),
            )
            .into_any_element()
    }

    /// The browsed directory's own column: the real selection lives here, so
    /// clicks use the same modifier semantics as the list and icon views.
    fn render_current_column(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let count = self.folder.read(cx).row_count();
        let scroll = self.column_scroll.clone();

        column_frame(cx)
            .child(
                gpui::uniform_list(
                    "current-column",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|row| {
                                let Some(entry) = this.folder.read(cx).entry_at(row).cloned()
                                else {
                                    return div().into_any_element();
                                };
                                let selected = this.folder.read(cx).is_row_selected(row);
                                this.column_row(("current-row", row), &entry, selected, cx)
                                    .on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, _, cx| {
                                            this.column_row_clicked(row, event, cx);
                                        },
                                    ))
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(
                                            move |this,
                                                  event: &gpui::MouseDownEvent,
                                                  window,
                                                  cx| {
                                                let path = this
                                                    .folder
                                                    .read(cx)
                                                    .entry_at(row)
                                                    .map(|e| e.path.clone());
                                                this.folder.update(cx, |f, cx| {
                                                    f.prepare_context_menu(row);
                                                    cx.emit(FolderEvent::SelectionChanged);
                                                });
                                                this.open_menu_at(path, event, window, cx);
                                            },
                                        ),
                                    )
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .size_full()
                .track_scroll(&scroll),
            )
            .into_any_element()
    }

    /// §5: a directory selection becomes the next listing; a file selection
    /// just selects, and the metadata column describes it.
    fn column_row_clicked(&mut self, row: usize, event: &gpui::ClickEvent, cx: &mut Context<Self>) {
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.folder.update(cx, |f, cx| {
                f.extend_select(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else if modifiers.platform || modifiers.control {
            self.folder.update(cx, |f, cx| {
                f.toggle_select(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        } else {
            self.folder.update(cx, |f, cx| {
                f.select_only(row);
                cx.emit(FolderEvent::SelectionChanged);
            });
        }
        self.sync_table_cursor(cx);
        cx.notify();

        let Some(entry) = self.folder.read(cx).entry_at(row).cloned() else {
            return;
        };
        if entry.is_dir_like() && !modifiers.shift && !modifiers.platform && !modifiers.control {
            cx.emit(PaneEvent::EnterDirectory(entry.path));
        } else if event.click_count() >= 2 && !entry.is_dir_like() {
            cx.emit(PaneEvent::OpenFile(entry.path));
        }
    }

    /// One 26 px row: icon, name, and a chevron for directories so the chain
    /// reads as something you can descend.
    fn column_row(
        &self,
        id: impl Into<gpui::ElementId>,
        entry: &ferret_vfs::Entry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let palette = BrowserPalette::resolve(cx);
        let focused = self.pane_focused.load(Ordering::Relaxed);
        let broken = entry.kind.is_broken_symlink();

        let fill = match (selected, focused) {
            (true, true) => palette.selected,
            (true, false) => palette.selected_idle,
            (false, _) => gpui::transparent_black(),
        };
        let text = if selected && focused {
            crate::theme::text_on(palette.selected, theme.foreground)
        } else {
            theme.foreground
        };
        let icon_color = if broken {
            theme.warning
        } else if entry.is_dir_like() {
            palette.folder
        } else {
            theme.muted_foreground
        };

        h_flex()
            .id(id)
            .w_full()
            .h(px(ROW_HEIGHT))
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .px(px(8.))
            .bg(fill)
            .when(!selected, |row| row.hover(|row| row.bg(theme.secondary)))
            .child(entry_icon(entry, 15., icon_color, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(ROW_FONT_SIZE))
                    .text_color(text)
                    .when(broken, |label| label.line_through())
                    .child(entry.name.clone()),
            )
            .when(entry.is_dir_like(), |row| {
                row.child(
                    gpui_component::Icon::new(gpui_component::IconName::ChevronRight)
                        .with_size(px(12.))
                        .text_color(if selected && focused {
                            crate::theme::text_on(palette.selected, theme.foreground)
                        } else {
                            theme.muted_foreground.opacity(0.7)
                        }),
                )
            })
    }

    /// The rightmost column. Metadata for the selection, not a preview.
    fn render_metadata_column(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (count, size, selected_entry) = self.folder.read_with(cx, |folder, _| {
            let count = folder.selection_count();
            let entry = folder.selected_entry().cloned();
            (count, folder.selection_size(), entry)
        });

        let frame = v_flex()
            .w(px(METADATA_WIDTH))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .p(px(14.))
            .text_color(theme.muted_foreground);

        let Some(entry) = selected_entry else {
            let label = match count {
                0 => "No selection".to_string(),
                n => format!("{n} items selected — {}", format_size(size)),
            };
            return frame
                .justify_center()
                .child(div().text_size(px(12.)).text_center().child(label))
                .into_any_element();
        };

        let icon_path = cx.global::<FerretState>().icons.asset_path(&entry);
        let icon_color = if entry.is_dir_like() {
            BrowserPalette::resolve(cx).folder
        } else {
            theme.muted_foreground
        };
        let detail = metadata_line(&entry);

        frame
            .child(
                gpui_component::Icon::default()
                    .path(icon_path)
                    .text_color(icon_color)
                    .with_size(px(48.)),
            )
            .child(
                div()
                    .w_full()
                    .text_size(px(12.5))
                    .text_center()
                    .text_color(theme.foreground)
                    .child(entry.name.clone()),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .font_features(crate::theme::tabular_numerals())
                    .text_center()
                    .child(detail),
            )
            .into_any_element()
    }
}

/// `(column directory, the child on the path)` for the nearest `max` strict
/// ancestors of `dir`, root-most first.
///
/// The child comes back with its parent because that is what each ancestor
/// column draws selected — without it the chain is a stack of listings rather
/// than a path.
fn ancestor_chain(dir: &VfsPath, max: usize) -> Vec<(VfsPath, VfsPath)> {
    let mut chain = Vec::new();
    let mut child = dir.clone();
    while chain.len() < max {
        let Some(parent) = child.parent() else {
            break;
        };
        chain.push((parent.clone(), child));
        child = parent;
    }
    // `parent()` walks upward; the columns read downward.
    chain.reverse();
    chain
}

/// The shared frame every listing column sits in: fixed width, hairline on
/// the right, its own scroll.
fn column_frame(cx: &App) -> gpui::Div {
    div()
        .w(px(COLUMN_WIDTH))
        .h_full()
        .flex_shrink_0()
        .border_r_1()
        .border_color(cx.theme().border)
}

/// "174 KB · 25 Aug 2026" — size and date, with directories showing neither a
/// size they do not have nor a lie in its place.
fn metadata_line(entry: &ferret_vfs::Entry) -> String {
    let size = entry.display_len().map(format_size);
    let date = entry.metadata.and_then(|m| m.modified).map(format_date);
    match (size, date) {
        (Some(size), Some(date)) => format!("{size} · {date}"),
        (Some(size), None) => size,
        (None, Some(date)) => date,
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{Entry, EntryKind, Metadata};

    fn entry(name: &str, kind: EntryKind, len: u64) -> Entry {
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind,
            metadata: Some(Metadata {
                len,
                modified: Some(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1_756_000_000),
                ),
                ..Default::default()
            }),
            link_target: None,
        }
    }

    #[test]
    fn the_chain_reads_downward_and_pairs_each_column_with_its_child() {
        let dir = VfsPath::local("/a/b/c/d");
        let chain = ancestor_chain(&dir, 4);

        let columns: Vec<String> = chain.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(columns, ["/", "/a", "/a/b", "/a/b/c"], "root-most first");

        // Each column's highlighted child is the next column's directory, and
        // the last one is the browsed directory itself.
        assert_eq!(chain[0].1.to_string(), "/a");
        assert_eq!(chain[3].1.to_string(), "/a/b/c/d");
    }

    #[test]
    fn the_chain_is_capped_at_the_nearest_ancestors() {
        let dir = VfsPath::local("/a/b/c/d/e/f/g");
        let chain = ancestor_chain(&dir, 2);
        assert_eq!(chain.len(), 2);
        // Capping keeps the *nearest* ancestors, not the root-most ones.
        assert_eq!(chain[0].0.to_string(), "/a/b/c/d/e");
        assert_eq!(chain[1].0.to_string(), "/a/b/c/d/e/f");
    }

    #[test]
    fn a_root_directory_has_no_ancestor_columns() {
        assert!(ancestor_chain(&VfsPath::local("/"), 4).is_empty());
    }

    #[test]
    fn metadata_line_omits_a_size_directories_do_not_have() {
        let file = metadata_line(&entry("Cargo.lock", EntryKind::File, 178_000));
        assert!(file.starts_with("174 KB · "), "{file}");

        // A directory has no meaningful size, so the line is the date alone
        // rather than "0 bytes".
        let dir = metadata_line(&entry("crates", EntryKind::Dir, 0));
        assert!(!dir.contains('·'), "{dir}");
        assert!(!dir.is_empty());
    }
}
