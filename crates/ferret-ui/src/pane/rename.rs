//! Inline rename (§6): the slow click that starts one, the editor
//! overlay, and committing the new name to the filesystem.

use super::list::NAME_MIN_WIDTH;
use super::*;

/// How long a click on an already-selected name waits before becoming a
/// rename (§8: "click then pause, or slow second click").
///
/// Must clear the platform double-click interval — 500 ms on both targets —
/// or opening a file with two ordinary clicks would leave a rename editor
/// open behind it.
const SLOW_CLICK_RENAME_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

pub(super) struct Renaming {
    pub(super) path: VfsPath,
    input: gpui::Entity<InputState>,
}

impl FilePane {
    /// Start a rename as soon as a background creation appears in the folder
    /// model. Watcher delivery is asynchronous, so the first render may still
    /// have no row; `render` retains this request until a `RowsChanged` event
    /// makes the path available.
    pub fn request_rename_when_visible(&mut self, path: VfsPath, cx: &mut Context<Self>) {
        self.pending_rename = Some(path);
        cx.notify();
    }

    pub fn begin_rename(&mut self, path: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = path.file_name() else {
            return;
        };
        if !self.folder.read(cx).has_entry(&path) || self.renaming.is_some() {
            return;
        }

        let stem_len = stem_length(name);
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value(name);
            // Spec §6: pre-select the stem, not the extension.
            state.set_selected_range(0..stem_len, cx);
            state
        });

        self._subscriptions.push(cx.subscribe_in(
            &input,
            window,
            move |this, input, event, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_rename(input.clone(), window, cx),
                InputEvent::Blur => this.commit_rename(input.clone(), window, cx),
                _ => {}
            },
        ));

        self.renaming = Some(Renaming { path, input });
        cx.notify();
    }

    fn commit_rename(
        &mut self,
        input: gpui::Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let new_name = input.read(cx).value().trim().to_string();
        cx.notify();

        let old_name = renaming.path.file_name().unwrap_or("").to_string();
        if new_name.is_empty() || new_name == old_name {
            return; // Nothing changed: treat as cancel.
        }
        if new_name.contains('/') || new_name.contains('\\') {
            eprintln!("rename: name may not contain separators: {new_name:?}");
            return;
        }

        let parent = renaming
            .path
            .parent()
            .unwrap_or_else(|| renaming.path.clone());
        let dest = parent.join(&new_name);
        let fs = cx.global::<FerretState>().fs;
        let src = renaming.path.clone();
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            if let Err(err) = fs.rename(&src, &dest).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    use gpui_component::{WindowExt as _, notification::Notification};
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
        // The directory watcher repaints the listing once the rename lands.
    }

    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.renaming.take().is_some() {
            cx.notify();
        }
    }

    /// Start the countdown to an inline rename, and cancel any previous one.
    ///
    /// The delay has to clear the double-click interval, or opening a file
    /// with two ordinary clicks would leave a rename editor behind it.
    pub(super) fn arm_slow_click_rename(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(path) = self.folder.read(cx).entry_at(row).map(|e| e.path.clone()) else {
            return;
        };
        self.rename_arm += 1;
        let armed = self.rename_arm;

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SLOW_CLICK_RENAME_DELAY)
                .await;
            let _ = this.update(cx, |this, cx| {
                // Anything at all since the click — a second click, a key, a
                // new selection — has already bumped the generation.
                if this.rename_arm != armed || this.renaming.is_some() {
                    return;
                }
                let still_alone = {
                    let folder = this.folder.read(cx);
                    folder.selection_count() == 1 && folder.has_entry(&path)
                };
                if still_alone {
                    this.pending_rename = Some(path.clone());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn disarm_slow_click_rename(&mut self) {
        self.rename_arm += 1;
        self.pending_rename = None;
    }

    /// The rename editor, sat on the row it renames (§6).
    ///
    /// In list view the row's position is computable — fixed row height, a
    /// known header, and the table's scroll offset — so the editor lands on
    /// the name it is editing. The other two views have no row geometry to
    /// borrow, so there it falls back to a panel at the foot of the pane.
    pub(super) fn render_rename_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let renaming = self.renaming.as_ref()?;
        let theme = cx.theme();
        let editor = Input::new(&renaming.input).small();

        let row_top = (self.view_mode == ViewMode::List)
            .then(|| self.folder.read(cx).row_of(&renaming.path))
            .flatten()
            .map(|row| {
                let scrolled = gpui_component::scroll::ScrollbarHandle::offset(
                    &self.table.read(cx).vertical_scroll_handle,
                );
                px(COLUMN_HEADER_HEIGHT + row as f32 * ROW_HEIGHT) + scrolled.y
            })
            // Only when the row is actually on screen; a rename started and
            // then scrolled away should not paint over the header.
            .filter(|top| *top >= px(COLUMN_HEADER_HEIGHT));

        let frame = h_flex()
            .absolute()
            .p_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.primary)
            .bg(theme.background)
            .shadow_md();

        Some(match row_top {
            Some(top) => frame
                .top(top - px(3.))
                .left(px(28.))
                .w(px(NAME_MIN_WIDTH))
                .child(editor)
                .into_any_element(),
            None => frame
                .left(px(12.))
                .bottom(px(12.))
                .w(px(320.))
                .child(editor)
                .into_any_element(),
        })
    }
}

fn stem_length(name: &str) -> usize {
    match name.rfind('.') {
        Some(ix) if ix > 0 => ix,
        _ => name.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_are_preselected_without_the_extension() {
        assert_eq!(stem_length("notes.txt"), 5);
        assert_eq!(stem_length("archive.tar.gz"), 11);
        // A dotfile is all stem: renaming ".gitignore" must not preselect
        // nothing.
        assert_eq!(stem_length(".gitignore"), 10);
        assert_eq!(stem_length("Makefile"), 8);
    }
}
