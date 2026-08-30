//! Dragging rows out of the pane, and dropping paths onto it.
//!
//! A drag carries the whole selection rather than the row under the
//! cursor. Drops arrive on listeners that have no route to the browser,
//! so they are recorded here and drained by the pane on its next frame.

use super::*;

/// What a row drag carries.
///
/// The whole selection, not the row under the cursor: dragging one of five
/// selected files and getting only that one is the behaviour every file
/// manager avoids.
#[derive(Clone, Debug)]
pub struct DraggedPaths {
    pub paths: Arc<[VfsPath]>,
}

/// The little card that follows the cursor during a drag.
pub(super) struct DragPreview {
    pub(super) label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px(px(8.))
            .py(px(4.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_md()
            .text_size(px(12.))
            .text_color(cx.theme().foreground)
            .child(self.label.clone())
    }
}

/// A drop the pane has not turned into an event yet.
///
/// The delegate's drop listener has no route to the browser, so it leaves the
/// request here and the pane's next frame emits it — the same hand-off the
/// slow-click rename uses.
pub(super) type DropQueue = Arc<Mutex<Option<PendingDrop>>>;

/// A disclosure triangle click waiting to be applied.
///
/// Same hand-off as drops: the delegate renders the row but only the pane can
/// mutate the folder.
pub(super) type ToggleQueue = Arc<Mutex<Option<usize>>>;

pub(super) struct PendingDrop {
    pub(super) sources: Vec<VfsPath>,
    pub(super) dest: VfsPath,
    /// Alt was held: copy even within one volume.
    pub(super) force_copy: bool,
}

/// Record a drop, rejecting the ones that cannot mean anything.
pub(super) fn queue_drop(
    queue: &DropQueue,
    dragged: &DraggedPaths,
    dest: VfsPath,
    window: &Window,
) {
    let sources = droppable(&dragged.paths, &dest);
    if sources.is_empty() {
        return;
    }
    *queue.lock() = Some(PendingDrop {
        sources,
        dest,
        force_copy: window.modifiers().alt,
    });
}

/// Which of `paths` may actually be dropped into `dest`.
///
/// Two rules, both of which would otherwise destroy data or do nothing: a
/// directory cannot be moved into itself or into anything it contains, and an
/// item already in `dest` has nowhere to go.
pub fn droppable(paths: &[VfsPath], dest: &VfsPath) -> Vec<VfsPath> {
    paths
        .iter()
        .filter(|src| *src != dest && !dest.starts_with(src))
        .filter(|src| src.parent().as_ref() != Some(dest))
        .cloned()
        .collect()
}

/// The same paths as a native file drag, for when the pointer leaves the
/// window (§8: "drop targets: … other applications").
///
/// Whether each path is a directory is passed in rather than looked up: the
/// resolver runs as the drag crosses the window edge, and a `stat` there
/// would be a syscall in the middle of a gesture.
pub(super) fn external_payload(
    dragged: &DraggedPaths,
    folder: &Folder,
) -> Option<gpui::ExternalDragPayload> {
    let entries: Vec<(std::path::PathBuf, bool)> = dragged
        .paths
        .iter()
        .filter_map(|p| {
            let local = p.as_local()?.to_path_buf();
            Some((local, folder.is_dir_like(p).unwrap_or(false)))
        })
        .collect();
    if entries.is_empty() {
        // A remote path has nothing the platform could receive.
        return None;
    }
    Some(gpui::ExternalDragPayload::Files(gpui::FileDragPaths::new(
        entries,
    )))
}

/// What the little card that follows the cursor says.
///
/// Derived inside the `on_drag` constructor rather than alongside the payload
/// (A-02a). The payload itself has to exist before a drag starts, but it is
/// now an `Arc` clone; the label is a fresh allocation, and building one per
/// row per frame paid for a card that is drawn at most once and usually never.
pub(super) fn drag_label(paths: &[VfsPath]) -> SharedString {
    match paths {
        [only] => SharedString::new(only.file_name().unwrap_or("item")),
        other => SharedString::new(format!("{} items", other.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> VfsPath {
        VfsPath::local(s)
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_itself() {
        assert!(droppable(&[p("/a/docs")], &p("/a/docs")).is_empty());
    }

    #[test]
    fn a_folder_cannot_be_dropped_into_its_own_child() {
        // The case that would otherwise move a directory inside itself and
        // orphan everything under it.
        assert!(droppable(&[p("/a/docs")], &p("/a/docs/notes")).is_empty());
        assert!(droppable(&[p("/a")], &p("/a/b/c/d")).is_empty());
    }

    #[test]
    fn a_sibling_with_a_shared_prefix_is_still_droppable() {
        // "/a/docs" is not inside "/a/doc" despite the shared text.
        let ok = droppable(&[p("/a/doc")], &p("/a/docs"));
        assert_eq!(ok, vec![p("/a/doc")]);
    }

    #[test]
    fn dropping_where_it_already_is_does_nothing() {
        assert!(droppable(&[p("/a/file.txt")], &p("/a")).is_empty());
    }

    #[test]
    fn a_mixed_drag_keeps_only_the_valid_paths() {
        // Into /a/sub: one from elsewhere, one from the parent directory —
        // both real moves — plus the destination itself, which is not.
        let sources = [p("/other/keep.txt"), p("/a/also.txt"), p("/a/sub")];
        assert_eq!(
            droppable(&sources, &p("/a/sub")),
            vec![p("/other/keep.txt"), p("/a/also.txt")]
        );

        // Into /a: the one already in /a drops out, the rest proceed.
        let sources = [p("/other/keep.txt"), p("/a/already.txt")];
        assert_eq!(droppable(&sources, &p("/a")), vec![p("/other/keep.txt")]);
    }
}
