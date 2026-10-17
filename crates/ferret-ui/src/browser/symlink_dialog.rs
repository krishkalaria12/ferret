use super::*;

use ferret_vfs::{SymlinkKind, SymlinkTarget};
use gpui_component::dialog::{Cancel, Confirm};

const DIALOG_WIDTH: f32 = 560.;

pub(super) struct SymlinkDialog {
    pub(super) folder: Entity<Folder>,
    parent: VfsPath,
    name: Entity<InputState>,
    target: Entity<InputState>,
    kind: SymlinkKind,
    creating: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl SymlinkDialog {
    fn new(
        folder: Entity<Folder>,
        parent: VfsPath,
        target: Option<VfsPath>,
        target_kind: Option<SymlinkTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let default_name = default_name(target.as_ref());
        let target_text = target
            .as_ref()
            .map(|path| path.to_string())
            .unwrap_or_default();
        let name = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("Link name");
            input.set_value(&default_name, window, cx);
            input
        });
        let target = cx.new(|cx| {
            let mut input =
                InputState::new(window, cx).placeholder("Absolute or relative target path");
            input.set_value(&target_text, window, cx);
            input
        });
        let subscriptions = vec![
            cx.subscribe(&name, |dialog, _, _: &InputEvent, cx| {
                dialog.error = None;
                cx.notify();
            }),
            cx.subscribe(&target, |dialog, _, _: &InputEvent, cx| {
                dialog.error = None;
                cx.notify();
            }),
        ];
        Self {
            folder,
            parent,
            name,
            target,
            kind: match target_kind {
                Some(SymlinkTarget::Dir) => SymlinkKind::Directory,
                _ => SymlinkKind::File,
            },
            creating: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn value(input: &Entity<InputState>, cx: &App) -> String {
        input.read(cx).value().trim().to_string()
    }

    fn record(
        &self,
        cx: &App,
    ) -> std::result::Result<(VfsPath, String, SymlinkKind), SharedString> {
        let name = Self::value(&self.name, cx);
        let target = Self::value(&self.target, cx);
        let taken = self.folder.read(cx).contains_name(&name);
        validate_link(&name, &target, taken)?;
        Ok((self.parent.join(&name), target, self.kind))
    }
}

/// The name a link to `target` starts out under, so "New Symlink to
/// Selected…" opens with something to read rather than an empty field.
fn default_name(target: Option<&VfsPath>) -> String {
    target
        .and_then(|path| path.file_name())
        .map(|name| format!("{name} link"))
        .unwrap_or_else(|| "untitled link".to_string())
}

/// Everything a link must satisfy before a backend is asked for it.
///
/// Kept off the dialog so each rule is testable without a window, and so the
/// order is the order the person sees: the name first, because that is what
/// the row will be called, then the target, because nothing can be created
/// without one.
fn validate_link(
    name: &str,
    target: &str,
    name_taken: bool,
) -> std::result::Result<(), SharedString> {
    ferret_vfs::validate_portable_file_name(name)
        .map_err(|reason| SharedString::from(format!("Link name {reason}.")))?;
    if name_taken {
        return Err(SharedString::from("A link with that name already exists."));
    }
    // Trimmed here rather than trusted from the caller: a target of nothing
    // but spaces is as unusable as an empty one, and the backend would only
    // find that out after the dialog closed.
    if target.trim().is_empty() {
        return Err(SharedString::from(
            "Enter the path the link should point to.",
        ));
    }
    if target.as_bytes().contains(&0) {
        return Err(SharedString::from("The target path cannot contain NUL."));
    }
    Ok(())
}

impl Render for SymlinkDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let creating = self.creating;
        let error = self.error.clone();
        let file_kind = self.kind == SymlinkKind::File;
        let directory_kind = self.kind == SymlinkKind::Directory;

        v_flex()
            .id("symlink-dialog-body")
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        Icon::default()
                            .path("icons/file/url.svg")
                            .with_size(px(24.))
                            .text_color(theme.primary),
                    )
                    .child(
                        v_flex()
                            .child(
                                div()
                                    .text_size(rem_from_ui_px(15.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("New Symbolic Link"),
                            )
                            .child(
                                div()
                                    .text_size(rem_from_ui_px(12.))
                                    .text_color(theme.muted_foreground)
                                    .child("The target is stored exactly as entered."),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(82.))
                            .text_color(theme.muted_foreground)
                            .child("Name"),
                    )
                    .child(Input::new(&self.name).flex_1().disabled(creating)),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(82.))
                            .text_color(theme.muted_foreground)
                            .child("Target"),
                    )
                    .child(Input::new(&self.target).flex_1().disabled(creating)),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(82.))
                            .text_color(theme.muted_foreground)
                            .child("Type"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("symlink-kind-file")
                                    .compact()
                                    .label("File")
                                    .when(file_kind, |button| button.primary())
                                    .when(!file_kind, |button| button.outline())
                                    .disabled(creating)
                                    .on_click(cx.listener(|dialog, _, _, cx| {
                                        dialog.kind = SymlinkKind::File;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("symlink-kind-directory")
                                    .compact()
                                    .label("Folder")
                                    .when(directory_kind, |button| button.primary())
                                    .when(!directory_kind, |button| button.outline())
                                    .disabled(creating)
                                    .on_click(cx.listener(|dialog, _, _, cx| {
                                        dialog.kind = SymlinkKind::Directory;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(error, |dialog, error| {
                dialog.child(
                    div()
                        .text_size(rem_from_ui_px(12.))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("symlink-cancel")
                            .outline()
                            .label("Cancel")
                            .disabled(creating)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("symlink-create")
                            .primary()
                            .label(if creating { "Creating…" } else { "Create" })
                            .disabled(creating)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                            }),
                    ),
            )
    }
}

impl Browser {
    pub(super) fn open_symlink_dialog(
        &mut self,
        target: Option<VfsPath>,
        target_kind: Option<SymlinkTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let folder = self.active().folder.clone();
        let parent = folder.read(cx).dir.clone();
        let supports_creation = cx
            .global::<FerretState>()
            .fs
            .capabilities_for(&parent)
            .create_symlinks;
        if !supports_creation {
            window.push_notification(
                Notification::error("This location cannot create symbolic links."),
                cx,
            );
            return;
        }

        let dialog =
            cx.new(|cx| SymlinkDialog::new(folder, parent, target, target_kind, window, cx));
        self.symlink_dialog = Some(dialog.clone());
        let focus = gpui::Focusable::focus_handle(dialog.read(cx).name.read(cx), cx);
        let browser = cx.weak_entity();

        window.open_dialog(cx, move |modal, window, cx| {
            window.focus(&focus, cx);
            let creating = dialog.read(cx).creating;
            let dialog = dialog.clone();
            let browser = browser.clone();
            modal
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .keyboard(!creating)
                .overlay_closable(!creating)
                .child(dialog.clone())
                .on_ok(move |_, window, cx| {
                    let Some(browser) = browser.upgrade() else {
                        return true;
                    };
                    // A second Confirm while one is in flight would create the
                    // same link twice; the first one already owns the name.
                    if dialog.read(cx).creating {
                        return false;
                    }
                    let values = dialog.read_with(cx, |dialog, cx| dialog.record(cx));
                    match values {
                        Ok((link, target, kind)) => {
                            let folder = dialog.read(cx).folder.clone();
                            dialog.update(cx, |dialog, cx| {
                                dialog.creating = true;
                                dialog.error = None;
                                cx.notify();
                            });
                            browser.update(cx, |browser, cx| {
                                browser.create_symlink(folder, link, target, kind, window, cx)
                            });
                        }
                        Err(error) => {
                            dialog.update(cx, |dialog, cx| {
                                dialog.error = Some(error);
                                cx.notify();
                            });
                        }
                    }
                    false
                })
        });
        cx.notify();
    }

    pub(super) fn create_symlink(
        &mut self,
        folder: Entity<Folder>,
        link: VfsPath,
        target: String,
        kind: SymlinkKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = window.window_handle();
        let browser = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result = fs.create_symlink(&link, &target, kind).await;
            let _ = window_handle.update(cx, |_, window, cx| {
                if let Some(browser) = browser.upgrade() {
                    browser.update(cx, |browser, cx| {
                        browser.finish_symlink(result, folder, link, window, cx);
                    });
                }
            });
        })
        .detach();
    }

    fn finish_symlink(
        &mut self,
        result: ferret_vfs::Result<()>,
        folder: Entity<Folder>,
        link: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.symlink_dialog = None;
                window.close_dialog(cx);
                folder.update(cx, |folder, cx| {
                    folder.reload_with_mapped_selection(
                        std::collections::HashSet::from([link.clone()]),
                        Some(link),
                        &[],
                        cx,
                    );
                });
            }
            Err(error) => {
                if let Some(dialog) = &self.symlink_dialog {
                    dialog.update(cx, |dialog, cx| {
                        dialog.creating = false;
                        dialog.error = Some(SharedString::from(error.to_string()));
                        cx.notify();
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{default_name, validate_link};
    use ferret_vfs::VfsPath;

    #[test]
    fn a_relative_target_is_accepted_verbatim() {
        // The point of a link is that the target is stored exactly as typed, so
        // `..` and a bare name are both valid and neither is resolved here.
        assert!(validate_link("notes link", "../elsewhere/notes.txt", false).is_ok());
    }

    #[test]
    fn a_name_a_directory_cannot_hold_is_named_as_such() {
        let error = validate_link("a/b", "notes.txt", false).unwrap_err();
        assert_eq!(error, "Link name contains a path separator.");
    }

    #[test]
    fn a_taken_name_is_reported_before_the_target_is_read() {
        // Both fields are wrong here; the name wins, because that is the one
        // the row will be called.
        let error = validate_link("notes.txt", "", true).unwrap_err();
        assert_eq!(error, "A link with that name already exists.");
    }

    #[test]
    fn an_empty_target_says_what_is_missing() {
        let error = validate_link("link", "   ", false).unwrap_err();
        assert_eq!(error, "Enter the path the link should point to.");
    }

    #[test]
    fn a_nul_in_the_target_is_refused() {
        let error = validate_link("link", "no\0pe", false).unwrap_err();
        assert_eq!(error, "The target path cannot contain NUL.");
    }

    #[test]
    fn a_link_to_a_selection_starts_out_named_after_it() {
        assert_eq!(
            default_name(Some(&VfsPath::local("/docs/notes.txt"))),
            "notes.txt link"
        );
        assert_eq!(default_name(None), "untitled link");
    }
}
