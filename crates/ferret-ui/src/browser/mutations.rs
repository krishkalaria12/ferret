//! Everything that changes something: filesystem operations, the
//! clipboard, and the view toggles that are persisted to settings.
//!
//! The browser owns these rather than the pane because it owns the error
//! reporting they need — a failed batch has to become one toast, not one
//! per path.

use super::*;

/// Why a delete is irreversible, which decides whether it may be skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermanentReason {
    /// The user chose "Delete Permanently". Honours the settings toggle.
    Asked,
    /// The user chose Trash, but the items are on a server, which has none.
    /// Always confirms: agreeing to skip confirmations for a *local*
    /// permanent delete says nothing about this.
    NoRemoteTrash,
}

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
        F: FnOnce(ferret_vfs::Router, VfsPath) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ferret_vfs::Result<()>> + Send + 'static,
    {
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = window.window_handle();
        // A mount has no watcher, so the new row would never appear on its
        // own and the rename request would wait for a row that never comes.
        let needs_reload = !path.is_local();
        // Capture the initiating pane. If the user changes tabs while a slow
        // volume creates the item, the rename must not land in whichever tab
        // happens to be active when the operation completes.
        let pane = self.active().pane.clone();
        cx.spawn(async move |this, cx| match create(fs, path.clone()).await {
            Ok(()) => {
                if needs_reload {
                    let _ = this.update(cx, |this, cx| this.refresh(cx));
                }
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

        // There is no trash on a server. Silently skipping the remote half of
        // a selection would be the worst of the three options — the user
        // pressed Delete and nothing happened — and quietly deleting it
        // instead is exactly what the "trash before delete" gate forbids. So
        // remote paths are split out and routed to the permanent path, which
        // says so before it does it.
        let (local, remote): (Vec<VfsPath>, Vec<VfsPath>) =
            paths.into_iter().partition(VfsPath::is_local);

        if !remote.is_empty() {
            self.confirm_permanent_delete(remote, PermanentReason::NoRemoteTrash, window, cx);
        }
        if local.is_empty() {
            return;
        }

        let platform = cx.global::<FerretState>().platform.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();
            for path in local {
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

        self.confirm_permanent_delete(paths, PermanentReason::Asked, window, cx);
    }

    /// Confirm, then delete irreversibly.
    ///
    /// `reason` decides whether the confirmation can be skipped.
    /// `settings.browser.confirm_permanent_delete` is on by default, and the
    /// schema says why trashing is never confirmed: it is reversible, and a
    /// dialog on every delete trains people to dismiss them. Someone who has
    /// turned it off has said they know what Delete Permanently does — but
    /// they have *not* said anything about a server with no trash, so
    /// [`PermanentReason::NoRemoteTrash`] always asks.
    fn confirm_permanent_delete(
        &mut self,
        paths: Vec<VfsPath>,
        reason: PermanentReason,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let skippable = reason == PermanentReason::Asked;
        if skippable
            && !cx
                .global::<FerretState>()
                .settings
                .browser
                .confirm_permanent_delete
        {
            self.delete_now(paths, window, cx);
            return;
        }

        let count = paths.len();
        let description = match reason {
            PermanentReason::Asked => {
                format!("{count} item(s) will be deleted permanently. This can't be undone.")
            }
            PermanentReason::NoRemoteTrash => format!(
                "{count} item(s) are on a server, which has no trash. \
                 They will be deleted permanently. This can't be undone."
            ),
        };
        let this = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .title("Delete Permanently?")
                .description(description.clone())
                .confirm()
                .on_ok({
                    let paths = paths.clone();
                    let this = this.clone();
                    move |_, window, cx| {
                        if let Some(this) = this.upgrade() {
                            this.update(cx, |this, cx| {
                                this.delete_now(paths.clone(), window, cx);
                            });
                        }
                        true
                    }
                })
        });
    }

    /// Delete without asking. The confirmation, where there is one, has
    /// already happened.
    fn delete_now(&mut self, paths: Vec<VfsPath>, window: &mut Window, cx: &mut Context<Self>) {
        // A mount has no watcher, so nothing else will notice the rows are
        // gone. The reload is scheduled here rather than left to the user.
        let needs_reload = paths.iter().any(|path| !path.is_local());
        // Through the Vfs, not the platform: deleting irreversibly is
        // filesystem mechanics with nothing OS-specific in it, and
        // `RemoveMode::Permanent` is where the parallel tree delete lives
        // (D-34).
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let mut failures = Vec::new();
            for path in paths {
                if let Err(err) = fs.remove(&path, ferret_vfs::RemoveMode::Permanent).await {
                    failures.push(err.to_string());
                }
            }
            if needs_reload {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
            }
            if let Some(message) = summarize_failures("be deleted", &failures) {
                let _ = window_handle.update(cx, |_, window, cx| {
                    window.push_notification(Notification::error(message), cx);
                });
            }
        })
        .detach();
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
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = window.window_handle();
        // Names are settled here, so a copy beside its original gets "file 2"
        // rather than failing or overwriting.
        let planned = self.plan_names(sources, &dest, cx);
        // Either end being remote means no watcher saw the change.
        let needs_reload = !dest.is_local() || planned.iter().any(|(source, _)| !source.is_local());

        cx.spawn(async move |this, cx| {
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

            if needs_reload {
                let _ = this.update(cx, |this, cx| this.refresh(cx));
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

    /// Copy or cut the selection to the **system** pasteboard (D-42).
    ///
    /// Two entries are written. `ExternalPaths` is the one that means "these
    /// are files" to another application; the newline-joined string is what
    /// makes pasting into a terminal, a Save dialog or a text field give you
    /// the paths. See [`read_clipboard_paths`] for which of the two survives
    /// on the pinned GPUI revision.
    ///
    /// Whether this was a cut stays a process-local flag beside the write:
    /// macOS has no pasteboard representation for it, and Finder does the same
    /// thing — a "cut" there is a copy plus a move-on-paste decided by the
    /// receiving side.
    pub(super) fn copy_selection(&mut self, cut: bool, cx: &mut Context<Self>) {
        let paths = self.active().pane.read(cx).selected_paths(cx);
        if paths.is_empty() {
            return;
        }

        let locals: Vec<std::path::PathBuf> = paths
            .iter()
            .filter_map(|p| p.as_local().map(Path::to_path_buf))
            .collect();
        if !locals.is_empty() {
            let text = locals
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join("\n");
            cx.write_to_clipboard(gpui::ClipboardItem {
                entries: vec![
                    gpui::ClipboardEntry::ExternalPaths(gpui::ExternalPaths(
                        locals.into_iter().collect(),
                    )),
                    gpui::ClipboardEntry::String(gpui::ClipboardString::new(text)),
                ],
            });
        }

        // Kept alongside, not instead: the pasteboard cannot carry the cut
        // flag, and a remote path has nothing to put on it at all.
        self.clipboard = Some((paths, cut));
    }

    pub(super) fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The system pasteboard wins when it holds something newer than our
        // own last copy — which is how a copy in Finder, in another Ferret
        // window, or in any application that puts files on the pasteboard
        // becomes a paste here (D-42). Falling back to the local clipboard
        // covers a remote path, which has no pasteboard representation.
        let from_system = read_clipboard_paths(cx);
        let (sources, cut) = match (from_system, self.clipboard.take()) {
            (Some(system), local) => {
                // A cut is only ours to know about, and only if the pasteboard
                // still holds the same paths we put there.
                let cut = local
                    .filter(|(paths, _)| *paths == system)
                    .is_some_and(|(_, cut)| cut);
                (system, cut)
            }
            (None, Some((local, cut))) => (local, cut),
            (None, None) => return,
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
        let fs = cx.global::<FerretState>().fs.clone();
        let window_handle = window.window_handle();

        cx.spawn(async move |_, cx| {
            let mut failures = Vec::new();
            for (src, target) in planned {
                // Checked here rather than on the window thread: a pasteboard
                // full of prose produces lines that look like paths, and this
                // is where a `stat` is allowed to be slow.
                if !fs.exists(&src).await {
                    continue;
                }
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
        // A mount answers from a short-lived cache, so Refresh has to say so
        // first — otherwise ⌘R over a remote folder could return exactly what
        // is already on screen, which is the opposite of what it promises.
        // Local disk implements this as a no-op.
        let dir = self.active().folder.read(cx).dir.clone();
        cx.global::<FerretState>().fs.invalidate(&dir);

        self.active()
            .folder
            .update(cx, |f, cx| f.reload_preserving_state(cx));
        // ⌘R means "show me what is actually there", which includes the git
        // state — the listing's own reload would otherwise refresh the rows and
        // leave last minute's badges on them.
        self.request_git_refresh(cx);
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

    /// The last volume list the background probe produced.
    ///
    /// Never a filesystem call: [`Self::start_place_refresh`] owns the
    /// probing, so the sidebar draws the last known answer even while a stale
    /// mount is taking seconds to reply (A-25).
    pub(super) fn volumes(&self) -> Vec<Place> {
        self.volumes.clone()
    }

    /// The last Places list the background probe produced.
    ///
    /// Same rule and the same reason (A-28). `platform.places()` is not a
    /// lookup — it asks the filesystem whether each well-known folder still
    /// exists, so it was seven `is_dir()` calls and eight allocations *per
    /// frame*, and `~/Documents` on an evicted iCloud Drive or a home
    /// directory on a dropped SMB mount is exactly where `is_dir` blocks.
    pub(super) fn places(&self) -> Vec<Place> {
        self.places.clone()
    }

    /// Keep [`Self::places`] and [`Self::volumes`] fresh from one background
    /// timer.
    ///
    /// One task rather than two: they are the same kind of work — ask the
    /// platform, compare, repaint if it moved — on the same schedule, and two
    /// timers would wake the window twice as often to usually do nothing.
    /// The first pass runs immediately so the sidebar is not empty for fifteen
    /// seconds on launch.
    pub(super) fn start_place_refresh(&mut self, cx: &mut Context<Self>) {
        self.place_refresh = Some(cx.spawn(async move |this, cx| {
            loop {
                let platform = cx.update(|cx| cx.global::<FerretState>().platform.clone());
                let probed = cx
                    .background_executor()
                    .spawn(async move { (platform.places(), platform.volumes()) })
                    .await;

                let alive = this.update(cx, |this, cx| {
                    // Repaint only on a real change: mounting nothing must not
                    // wake the window every fifteen seconds.
                    let (places, volumes) = probed;
                    if this.places != places || this.volumes != volumes {
                        this.places = places;
                        this.volumes = volumes;
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

/// Whatever files the system pasteboard is holding.
///
/// Two shapes are accepted because only one of them round-trips today:
///
/// * `ExternalPaths` is what Finder and every other well-behaved application
///   writes, and GPUI reads it correctly — so **copying in Finder and pasting
///   in Ferret works**.
/// * The newline-joined string is the fallback, and it is what makes
///   **Ferret → Ferret** work. The pinned GPUI revision's macOS pasteboard
///   *write* for `ExternalPaths` is an empty match arm
///   (`gpui_macos/src/pasteboard.rs`), so what Ferret puts on the pasteboard
///   today is only the string half. Ferret → Finder therefore does not yet
///   move files; the entry is written anyway, so it starts working the day
///   that arm is implemented.
///
/// Lines are filtered structurally — absolute, non-empty — and **not** by
/// asking the filesystem whether they exist. This runs on the window thread
/// from an action handler, and a `stat` there is the thing A-25 was about: one
/// pasted path pointing into a dropped network mount would block the window.
/// The paste task checks existence instead, where the I/O belongs, so pasting
/// a paragraph of prose still does nothing.
fn read_clipboard_paths(cx: &App) -> Option<Vec<VfsPath>> {
    let item = cx.read_from_clipboard()?;

    for entry in item.entries() {
        if let gpui::ClipboardEntry::ExternalPaths(paths) = entry {
            let paths: Vec<VfsPath> = paths.paths().iter().map(VfsPath::local).collect();
            if !paths.is_empty() {
                return Some(paths);
            }
        }
    }

    let text = item.entries().iter().find_map(|entry| match entry {
        gpui::ClipboardEntry::String(s) => Some(s.text()),
        _ => None,
    })?;
    let paths: Vec<VfsPath> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(std::path::Path::new)
        .filter(|path| path.is_absolute())
        .map(VfsPath::local)
        .collect();
    (!paths.is_empty()).then_some(paths)
}
