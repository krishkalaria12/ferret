//! Everything that changes something: filesystem operations, the
//! clipboard, and the view toggles that are persisted to settings.
//!
//! The browser owns these rather than the pane because it owns the error
//! reporting they need — a failed batch has to become one toast, not one
//! per path.

use super::*;

impl Browser {
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

    /// Assign each source a name free in `dest`, before anything is spawned.
    ///
    /// Naming has to happen on the window thread, because only there is the
    /// folder model readable — but it costs one index lookup per source
    /// (A-23), and there are as many sources as the user dragged rather than
    /// as many as the directory holds. `claimed` covers collisions *within*
    /// this batch, which the folder cannot know about yet.
    ///
    /// Names are only checked against the listing when `dest` is the folder on
    /// screen; anywhere else Ferret has nothing loaded to check against, and
    /// the copy fails rather than silently overwriting.
    fn plan_names(
        &self,
        sources: Vec<VfsPath>,
        dest: &VfsPath,
        cx: &Context<Self>,
    ) -> Vec<(VfsPath, String)> {
        let folder = self.active().folder.read(cx);
        let check_listing = *dest == folder.dir;
        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut planned = Vec::with_capacity(sources.len());

        for src in sources {
            let Some(name) = src.file_name() else {
                continue;
            };
            let target = unique_name(
                |candidate| {
                    claimed.contains(candidate)
                        || (check_listing && folder.contains_name(candidate))
                },
                name,
            );
            claimed.insert(target.clone());
            planned.push((src, target));
        }
        planned
    }

    /// A name nothing in the browsed directory is using.
    fn free_name(&self, wanted: &str, cx: &Context<Self>) -> String {
        let folder = self.active().folder.read(cx);
        unique_name(|candidate| folder.contains_name(candidate), wanted)
    }

    pub(super) fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = self.free_name("untitled folder", cx);
        let path = dir.join(&name);

        self.create_then_rename(
            path,
            move |fs, path| async move { fs.create_dir(&path).await },
            window,
            cx,
        );
    }

    pub(super) fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.active().folder.read(cx).dir.clone();
        let name = self.free_name("untitled.txt", cx);
        let path = dir.join(&name);

        self.create_then_rename(
            path,
            move |fs, path| async move { fs.create_file(&path).await },
            window,
            cx,
        );
    }

    /// Run a creation operation away from the window thread, then start the
    /// inline rename only after the watcher can observe a real path.
    fn create_then_rename<F, Fut>(
        &mut self,
        path: VfsPath,
        create: F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: FnOnce(ferret_vfs::LocalFs, VfsPath) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ferret_vfs::Result<()>> + Send + 'static,
    {
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();
        // Capture the initiating pane. If the user changes tabs while a slow
        // volume creates the item, the rename must not land in whichever tab
        // happens to be active when the operation completes.
        let pane = self.active().pane.clone();
        cx.spawn(async move |_, cx| match create(fs, path.clone()).await {
            Ok(()) => {
                let _ = window_handle.update(cx, |_, _window, cx| {
                    pane.update(cx, |pane, cx| pane.request_rename_when_visible(path, cx));
                });
            }
            Err(err) => {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(err.to_string()), cx);
                });
            }
        })
        .detach();
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
        // Names are settled here, so a copy beside its original gets "file 2"
        // rather than failing or overwriting.
        let planned = self.plan_names(sources, &dest, cx);

        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();

            for (src, target) in planned {
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
        // A cut item pasted into its own folder is a no-op, and dropping it
        // before naming keeps it from claiming a "file 2" nothing will use.
        let sources: Vec<VfsPath> = sources
            .into_iter()
            .filter(|src| !(cut && src.parent().as_ref() == Some(&dest_dir)))
            .collect();
        let planned = self.plan_names(sources, &dest_dir, cx);
        let fs = cx.global::<FerretState>().fs;
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();
            for (src, target) in planned {
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
            state.touch_settings();
            state.settings.browser.show_hidden
        };
        crate::state::arm_config_save(cx);
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
        state.touch_settings();
        crate::state::arm_config_save(cx);
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
            state.touch_settings();
        }
        crate::state::arm_config_save(cx);
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
            state.touch_settings();
            crate::state::arm_config_save(cx);
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

    /// The last volume list the background probe produced.
    ///
    /// Never a filesystem call: [`Self::start_volume_refresh`] owns the
    /// probing, so the sidebar draws the last known answer even while a stale
    /// mount is taking seconds to reply (A-25).
    pub(super) fn volumes(&self) -> Vec<Place> {
        self.volumes.clone()
    }

    /// Keep [`Self::volumes`] fresh from a background timer.
    ///
    /// One probe every [`VOLUMES_TTL`], on the background executor, for the
    /// life of the browser. The first pass runs immediately so the sidebar is
    /// not empty for fifteen seconds on launch.
    pub(super) fn start_volume_refresh(&mut self, cx: &mut Context<Self>) {
        self.volume_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let platform = cx.update(|cx| cx.global::<FerretState>().platform.clone());
                let probed = cx
                    .background_executor()
                    .spawn(async move { platform.volumes() })
                    .await;

                let alive = this.update(cx, |this, cx| {
                    // Repaint only on a real change: mounting nothing must not
                    // wake the window every fifteen seconds.
                    if this.volumes != probed {
                        this.volumes = probed;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                cx.background_executor().timer(VOLUMES_TTL).await;
            }
        }));
    }
}
