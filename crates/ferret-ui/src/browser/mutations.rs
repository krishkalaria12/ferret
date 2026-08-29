//! Everything that changes something: filesystem operations, the
//! clipboard, and the view toggles that are persisted to settings.
//!
//! The browser owns these rather than the pane because it owns the error
//! reporting they need — a failed batch has to become one toast, not one
//! per path.

use super::*;

impl Browser {
    pub(super) fn open_current_in_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let Some(local) = dir.as_local().map(Path::to_path_buf) else {
            window.push_notification(
                Notification::error("A system terminal can only open local folders."),
                cx,
            );
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.open_in_terminal(&local).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
    }

    pub(super) fn open_with_default(
        &mut self,
        path: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(local) = path.as_local().map(Path::to_path_buf) else {
            return;
        };
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            if let Err(err) = platform.open_with_default(&local).await {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
    }

    fn entry_names(&self, cx: &Context<Self>) -> std::collections::HashSet<String> {
        self.active().folder.read(cx).entry_names()
    }

    pub(super) fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = unique_name(&self.entry_names(cx), "untitled folder");
        let path = dir.join(&name);

        let fs = cx.global::<FerretState>().fs;
        // A single mkdir is sub-millisecond on local disks; blocking buys us
        // a live window to start the inline rename in. Remote backends will
        // route through an operation queue instead.
        if let Err(err) = smol_block_on(fs.create_dir(&path)) {
            window.push_notification(Notification::error(err.to_string()), cx);
            return;
        }
        self.active()
            .pane
            .update(cx, |pane, cx| pane.begin_rename(path, window, cx));
    }

    pub(super) fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = unique_name(&self.entry_names(cx), "untitled.txt");
        let path = dir.join(&name);

        let fs = cx.global::<FerretState>().fs;
        if let Err(err) = smol_block_on(fs.create_file(&path)) {
            window.push_notification(Notification::error(err.to_string()), cx);
            return;
        }
        self.active()
            .pane
            .update(cx, |pane, cx| pane.begin_rename(path, window, cx));
    }

    pub(super) fn trash_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();
            for path in paths {
                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                    continue;
                };
                if let Err(err) = platform.move_to_trash(&local).await {
                    failures.push(err.to_string());
                }
            }
            if let Some(message) = summarize_failures("move to the trash", &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
        // Watchers refresh the listing as entries vanish.
    }

    pub(super) fn delete_selection_permanently(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let this = cx.weak_entity();
        let window_handle = window.window_handle();
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .title("Delete Permanently?")
                .description(format!(
                    "{count} item(s) will be deleted permanently. This can't be undone."
                ))
                .confirm()
                .on_ok({
                    let paths = paths.clone();
                    let this = this.clone();
                    move |_, _, cx| {
                        if this.upgrade().is_none() {
                            return true;
                        }
                        let platform = cx.global::<FerretState>().platform.clone();
                        let paths = paths.clone();
                        cx.spawn(async move |cx| {
                            let mut failures = 0usize;
                            let mut first_error = None;
                            for path in paths {
                                let Some(local) = path.as_local().map(Path::to_path_buf) else {
                                    continue;
                                };
                                if let Err(err) = platform.remove_permanently(&local).await {
                                    failures += 1;
                                    first_error.get_or_insert(err.to_string());
                                }
                            }
                            if failures > 0 {
                                let message = match first_error {
                                    Some(err) if failures == 1 => err,
                                    Some(err) => {
                                        format!("{failures} items could not be deleted: {err}")
                                    }
                                    None => String::new(),
                                };
                                let _ = window_handle.update(cx, |_, window, cx| {
                                    window.push_notification(Notification::error(message), cx);
                                });
                            }
                        })
                        .detach();
                        true
                    }
                })
        });
    }

    /// Drop a set of paths into `dest` (§8).
    ///
    /// Move within a volume, copy across one, and copy either way when Alt
    /// was held. The cross-volume case is not guessed at from the paths —
    /// `rename` reports `CrossVolume` and the copy path takes over, which is
    /// the only answer that stays right for mount points and network shares.
    pub(super) fn transfer(
        &mut self,
        sources: Vec<VfsPath>,
        dest: VfsPath,
        force_copy: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();
        // Names already in the destination, so a copy beside its original
        // gets "file 2" rather than failing or overwriting.
        let taken = if dest == self.active().folder.read(cx).dir {
            self.entry_names(cx)
        } else {
            std::collections::HashSet::new()
        };

        cx.spawn(async move |_, cx| {
            let mut taken = taken;
            let mut failures = Vec::new();

            for src in sources {
                let Some(name) = src.file_name().map(str::to_owned) else {
                    continue;
                };
                let target = unique_name(&taken, &name);
                taken.insert(target.clone());
                let into = dest.join(&target);

                let result = if force_copy {
                    fs.copy(&src, &into).await
                } else {
                    match fs.rename(&src, &into).await {
                        // Across volumes there is no rename to do: copy, then
                        // remove the original, which is what a move means.
                        Err(ferret_vfs::VfsError::CrossVolume { .. }) => {
                            match fs.copy(&src, &into).await {
                                Ok(()) => fs.remove(&src, ferret_vfs::RemoveMode::Permanent).await,
                                Err(err) => Err(err),
                            }
                        }
                        other => other,
                    }
                };
                if let Err(err) = result {
                    failures.push(err.to_string());
                }
            }

            let verb = if force_copy { "copy" } else { "move" };
            if let Some(message) = summarize_failures(verb, &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
        // Both directories are watched, so the rows follow on their own.
    }

    pub(super) fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if !paths.is_empty() {
            self.clipboard = Some((paths, cut));
        }
    }

    pub(super) fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((sources, cut)) = self.clipboard.take() else {
            return;
        };
        let restore_on_copy = (!cut).then(|| sources.clone());
        let dest_dir = self.active().folder.read(cx).dir.clone();
        let taken = self.entry_names(cx);
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            let mut taken = taken;
            let mut failures = Vec::new();
            for src in sources {
                let Some(name) = src.file_name() else {
                    continue;
                };
                let dest_parent = src.parent();
                if cut && dest_parent.as_ref() == Some(&dest_dir) {
                    continue; // Pasting a cut item into its own folder.
                }
                let target = unique_name(&taken, name);
                taken.insert(target.clone());
                let dest = dest_dir.join(&target);

                let result = match fs.copy(&src, &dest).await {
                    Ok(()) => {
                        if cut {
                            fs.remove(&src, ferret_vfs::RemoveMode::Permanent).await
                        } else {
                            Ok(())
                        }
                    }
                    Err(err) => Err(err),
                };
                if let Err(err) = result {
                    failures.push(err.to_string());
                }
            }
            if let Some(message) = summarize_failures("paste", &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();

        // Copies stay on the clipboard: paste twice, paste elsewhere.
        self.clipboard = restore_on_copy.map(|sources| (sources, false));
    }

    pub(super) fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        let show = {
            let state = cx.global_mut::<FerretState>();
            state.settings.browser.show_hidden = !state.settings.browser.show_hidden;
            state.save_settings();
            state.settings.browser.show_hidden
        };
        self.active().folder.update(cx, |f, cx| {
            f.set_show_hidden(show);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.active()
            .folder
            .update(cx, |f, cx| f.reload_preserving_state(cx));
    }

    pub(super) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        self.animate_sidebar(cx);
        let state = cx.global_mut::<FerretState>();
        state.settings.sidebar.visible = self.sidebar_visible;
        state.save_settings();
        cx.notify();
    }

    pub(super) fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::{Theme, ThemeMode};
        let next = if Theme::global(cx).mode.is_dark() {
            ThemeMode::Light
        } else {
            ThemeMode::Dark
        };
        {
            let state = cx.global_mut::<FerretState>();
            state.settings.appearance.theme = if next.is_dark() {
                ThemeChoice::Dark
            } else {
                ThemeChoice::Light
            };
            state.save_settings();
        }
        Theme::change(next, Some(window), cx);
        cx.notify();
    }

    pub(super) fn set_view_mode(
        &mut self,
        mode: ViewMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = window;
        self.active_mut().view_mode = mode;
        self.active()
            .pane
            .update(cx, |pane, cx| pane.set_view_mode(mode, cx));

        // Remember per folder (F-06): the mode rides the same FolderView
        // record as sort.
        self.record_current_view(cx);
        cx.notify();
    }

    pub(super) fn add_current_to_favorites(&mut self, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let Some(local) = dir.as_local() else {
            return;
        };
        let state = cx.global_mut::<FerretState>();
        if !state.settings.sidebar.favorites.iter().any(|p| p == local) {
            state.settings.sidebar.favorites.push(local.to_path_buf());
            state.save_settings();
        }
        cx.notify();
    }

    pub(super) fn favorites(&self, cx: &Context<Self>) -> Vec<PathBuf> {
        cx.global::<FerretState>()
            .settings
            .sidebar
            .favorites
            .clone()
    }

    pub(super) fn places(&self, cx: &Context<Self>) -> Vec<Place> {
        cx.global::<FerretState>().platform.places()
    }

    pub(super) fn volumes(&mut self, cx: &Context<Self>) -> Vec<Place> {
        if let Some((at, volumes)) = &self.volumes_cache
            && at.elapsed() < VOLUMES_TTL
        {
            return volumes.clone();
        }
        let volumes = cx.global::<FerretState>().platform.volumes();
        self.volumes_cache = Some((Instant::now(), volumes.clone()));
        volumes
    }
}
