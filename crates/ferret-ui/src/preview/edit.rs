//! In-place editing (F-36): the smallest feature with the largest downside.
//!
//! R-06 names data loss as the worst thing this product can do, and the save
//! path is the one place the preview pane can do it. The shape, held from the
//! plan:
//!
//! * The Edit button exists only for complete, lossless text on a backend
//!   with the atomic-save capability. General write permission is
//!   insufficient; a generic writable backend stays read-only.
//! * The editor buffer is pinned at edit start (bytes plus identity). Saving
//!   re-encodes fidelity (line endings, BOM, trailing newline) around the
//!   edited text and replaces the file atomically, or refuses on conflict.
//! * Selection changes are blocked while dirty until the user answers Save /
//!   Discard / Cancel. The selection change is blocked, not queued.
//! * Saving and Conflict are explicit UI states. No autosave.
//!
//! The editor's writable flag lives on the *element* (`Editor::readonly`),
//! not on `EditorState`, so entering and leaving edit mode is a re-render —
//! no buffer reset, no scroll or cursor jump.

use super::{PaneState, PreviewPanel};
use crate::state::FerretState;
use ferret_preview::{Bom, LineEndings};
use ferret_vfs::{FileIdentity, Vfs as _, VfsPath};
use gpui::{App, AppContext, Context, Focusable as _, Window};
use gpui_component::WindowExt as _;

/// The loaded bytes behind a saveable text preview, carried from the
/// background load so the Edit button never re-reads the file.
///
/// The identity is of *these* bytes — what the user sees — not of whatever
/// the file holds when Edit is pressed. An external change between preview
/// and Edit therefore conflicts at save instead of being silently kept or
/// silently overwritten.
#[derive(Clone, Debug)]
pub(crate) struct EditOriginal {
    /// The complete file bytes (only carried for complete files).
    pub bytes: Vec<u8>,
    /// Length, mtime and content hash of `bytes`.
    pub identity: FileIdentity,
    /// Whether the decode was lossless with uniform endings — the saveable
    /// gate. Threads through so the button needs no second decode.
    pub saveable: bool,
}

/// One pinned edit buffer (PV-Q02): no per-tab unsaved workspace.
pub(crate) struct EditSession {
    /// What is being edited, for the guard's same-path check.
    pub path: VfsPath,
    /// The editor text at edit start, for no-op detection.
    pub original_text: String,
    pub bom: Bom,
    pub endings: LineEndings,
    pub trailing_newline: bool,
    pub identity: FileIdentity,
    pub dirty: bool,
    /// True from Save until the backend answers. The pane stays up for that
    /// whole window, which is what stops a second press queueing a second
    /// save.
    pub saving: bool,
    /// The refusal message, when the last save conflicted. Kept with the
    /// pinned buffer: the user answers reload-or-cancel from here.
    pub conflict: Option<String>,
}

impl PreviewPanel {
    /// Whether the current body offers in-place editing right now.
    ///
    /// O(1): a flag carried from the background load, never a re-decode.
    /// Never true while already editing, while saving, for truncated or
    /// lossy buffers, for symlinks, or off an atomic-save backend.
    pub(crate) fn can_edit(&self) -> bool {
        self.editing.is_none()
            && self
                .state
                .original()
                .is_some_and(|original| original.saveable)
    }

    /// Whether unsaved changes are holding the selection hostage.
    pub(crate) fn is_dirty(&self) -> bool {
        self.editing.as_ref().is_some_and(|session| session.dirty)
    }

    /// Whether a save is in flight (duplicate saves stay disabled).
    pub(crate) fn is_saving(&self) -> bool {
        self.editing.as_ref().is_some_and(|session| session.saving)
    }

    /// The path being edited, if any — for the guard's same-path check.
    pub(crate) fn editing_path(&self) -> Option<VfsPath> {
        self.editing.as_ref().map(|session| session.path.clone())
    }

    /// The conflict message, if the last save was refused.
    pub(crate) fn edit_conflict(&self) -> Option<&str> {
        self.editing
            .as_ref()
            .and_then(|session| session.conflict.as_deref())
    }

    /// The dirty buffer for quit-time recovery: path plus current text.
    ///
    /// `None` when nothing unsaved exists. The binary calls this from
    /// `on_app_quit` — where no dialog can be opened — and preserves a
    /// recovery copy instead of discarding the buffer silently.
    pub(crate) fn dirty_buffer(&self, cx: &App) -> Option<(VfsPath, String)> {
        let session = self.editing.as_ref()?;
        if !session.dirty {
            return None;
        }
        Some((
            session.path.clone(),
            self.editor.read(cx).value().to_string(),
        ))
    }

    /// Fired once per editor change — never per frame.
    ///
    /// Value-compared, not flag-suppressed: loads happen with no edit
    /// session standing (so this ignores them), and revert restores the
    /// exact original text (so the comparison stays clean). A temporal
    /// `suppress_dirty` flag would assume `observe` fires synchronously
    /// inside `update`, which GPUI does not promise — a deferred observer
    /// would re-dirty a just-reverted buffer.
    pub(crate) fn on_editor_changed(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.editing.as_mut() else {
            return;
        };
        let current = self.editor.read(cx).value().to_string();
        let dirty = current != session.original_text;
        if dirty == session.dirty {
            return;
        }
        session.dirty = dirty;
        if dirty {
            // A save in flight pins its bytes; late keystrokes start the
            // next dirty cycle rather than joining the write.
            // A new edit supersedes a shown conflict: the buffer moved
            // on, and the old refusal describes bytes nobody holds.
            session.conflict = None;
        }
        cx.notify();
    }

    /// Enter edit mode: pin the buffer, focus the writable editor.
    ///
    /// The element re-renders writable around the buffer already on screen,
    /// so no `set_value` — and no scroll or cursor jump — starts an edit.
    pub(crate) fn begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(original) = self.state.original().cloned() else {
            return;
        };
        if !original.saveable || self.editing.is_some() {
            return;
        }
        let Some(entry) = self.state.entry().cloned() else {
            return;
        };
        let Some(decoded) = ferret_preview::text::decode(&original.bytes) else {
            return;
        };
        // Markdown edits its source: the rendered view cannot take a caret.
        if matches!(self.state, PaneState::ReadyMd { .. }) {
            self.toggle_source(cx);
        }
        self.editing = Some(EditSession {
            path: entry.path.clone(),
            original_text: decoded.text,
            bom: decoded.bom,
            endings: decoded.endings,
            trailing_newline: decoded.trailing_newline,
            identity: original.identity,
            dirty: false,
            saving: false,
            conflict: None,
        });
        window.focus(&self.editor.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Save the buffer, atomically or not at all.
    ///
    /// An unmodified Save is a no-op that simply leaves edit mode. Anything
    /// else re-encodes fidelity around the edited text and replaces the file
    /// through `Vfs::write_atomic`, which refuses on conflict rather than
    /// resolving. The browser continues a guarded navigation behind the
    /// outcome.
    pub(crate) fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|session| session.saving) {
            return;
        }
        let current = self.editor.read(cx).value().to_string();
        let no_op = self
            .editing
            .as_ref()
            .is_some_and(|session| current == session.original_text);
        if no_op {
            // Opened and saved without being edited: leave quietly. The
            // byte-identical round trip is a VFS test, not a write to make.
            self.finish_edit(window, cx);
            self.continue_edit_save(true, cx);
            return;
        }
        let Some(session) = self.editing.as_mut() else {
            return;
        };
        session.saving = true;
        session.conflict = None;
        cx.notify();

        let fidelity = ferret_preview::text::DecodedText {
            text: session.original_text.clone(),
            bom: session.bom,
            endings: session.endings,
            trailing_newline: session.trailing_newline,
            lossy: false,
            truncated_lines: false,
        };
        let bytes = ferret_preview::reencode(&fidelity, &current);
        let path = session.path.clone();
        let identity = session.identity;
        let generation = self.generation;
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = self.window;
        cx.spawn(async move |pane, cx| {
            let result = fs.write_atomic(&path, &bytes, identity).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                let _ = pane.update(cx, |pane, cx| {
                    // A save in flight across a tab switch or a new load must
                    // not settle onto the new file: drop the result on a
                    // generation or path mismatch.
                    if pane.generation != generation
                        || pane
                            .editing
                            .as_ref()
                            .is_none_or(|session| session.path != path)
                    {
                        return;
                    }
                    pane.finish_save(result, bytes, window, cx);
                });
            });
        })
        .detach();
    }

    /// Settle an async save back on the window thread.
    fn finish_save(
        &mut self,
        result: ferret_vfs::Result<FileIdentity>,
        written: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(identity) => {
                // The buffer is the new truth: re-pin around the written
                // bytes so a second edit diffs against what is on disk.
                if let Some(original) = self.state.original_mut() {
                    original.bytes = written;
                    original.identity = identity;
                }
                self.finish_edit(window, cx);
                self.continue_edit_save(true, cx);
            }
            Err(ferret_vfs::VfsError::Conflict { .. }) => {
                // The check-to-replace race has a caller-side answer: stash
                // the refused bytes in the recovery dir before telling the
                // user, so even a mistaken Discard cannot lose work. Bounded
                // (one file per conflict), best-effort, never a second error
                // surface — the buffer itself stays put regardless.
                let recovery_note = stash_conflict_recovery(
                    self.editing.as_ref().map(|session| &session.path),
                    &written,
                );
                if let Some(session) = self.editing.as_mut() {
                    session.saving = false;
                    session.conflict = Some(format!(
                        "Ferret can't save — this file changed on disk since you started editing. \
                         Revert to reload it, or copy your text elsewhere first.{recovery_note}"
                    ));
                }
                // Save failure keeps the buffer and focus intact.
                window.focus(&self.editor.read(cx).focus_handle(cx), cx);
                cx.notify();
                self.continue_edit_save(false, cx);
            }
            Err(err) => {
                use gpui_component::notification::Notification;
                if let Some(session) = self.editing.as_mut() {
                    session.saving = false;
                }
                window.push_notification(Notification::error(err.to_string()), cx);
                cx.notify();
                self.continue_edit_save(false, cx);
            }
        }
    }

    /// Leave edit mode with the buffer clean: no session, element readonly.
    fn finish_edit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        cx.notify();
    }

    /// Tell the browser a guarded save settled, so it can continue the
    /// navigation it held — or leave the conflict UI standing.
    fn continue_edit_save(&self, success: bool, cx: &mut impl AppContext) {
        let browser = self.browser.clone();
        let _ = browser.update_in(cx, |browser, window, cx| {
            browser.after_edit_save(success, window, cx);
        });
    }

    /// Throw away the edits and reload the pinned truth into the editor.
    ///
    /// Stays in edit mode: Revert means "that was a wrong turn", not "I am
    /// done editing". Leaving is Esc (when clean) or the selection moving on.
    pub(crate) fn revert_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.editing.as_mut() else {
            return;
        };
        if session.saving {
            return;
        }
        let original = session.original_text.clone();
        session.dirty = false;
        session.conflict = None;
        self.editor.update(cx, |editor, cx| {
            editor.set_value(original, window, cx);
        });
        cx.notify();
    }

    /// Drop the session without touching the buffer's bytes.
    ///
    /// Used when the user answers Discard: the selection is moving on and
    /// the incoming load replaces the buffer wholesale, so restoring the
    /// original text first would be a visible flash of a file nobody asked
    /// to see. The next `set_value` lands with no session standing, which
    /// is what keeps the observer from reading it as a keystroke.
    pub(crate) fn discard_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.is_none() {
            return;
        }
        self.editing = None;
        cx.notify();
    }
}

/// Stash refused save bytes in the recovery dir, returning the sentence the
/// conflict message appends.
///
/// Named by a hash of the full path plus the file name, so two same-named
/// files in different directories never collide and the original directory
/// stays recoverable from the copy's first line (which names the source).
/// Best-effort: a full disk or a missing cache dir costs the stash, never
/// the buffer — the caller keeps that regardless.
fn stash_conflict_recovery(path: Option<&VfsPath>, bytes: &[u8]) -> String {
    let Some(path) = path else {
        return String::new();
    };
    let dir = dirs::cache_dir()
        .map(|dir| dir.join("ferret").join("recovery"))
        .unwrap_or_else(|| std::env::temp_dir().join("ferret-recovery"));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    path.to_string().hash(&mut hasher);
    let digest = hasher.finish();
    let name = path
        .file_name()
        .map(|name| {
            name.chars()
                .map(|ch| {
                    if ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                        ch
                    } else {
                        '_'
                    }
                })
                .take(48)
                .collect::<String>()
        })
        .unwrap_or_else(|| "untitled".to_string());
    let dest = dir.join(format!("{name}.{digest:016x}.conflict.txt"));
    let mut body = format!("Source: {}\n", path);
    body.push_str(&String::from_utf8_lossy(bytes));
    if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&dest, body).is_ok() {
        format!(" A copy is kept at {}.", dest.to_string_lossy())
    } else {
        String::new()
    }
}
