//! Tabs and history: opening, closing and switching tabs, and the
//! navigation that moves each one through its own back/forward stack.
//!
//! A tab owns its folder, pane and history, so switching tabs swaps
//! entities rather than re-listing a directory that is already loaded.

use gpui::Focusable as _;

use super::*;

impl Browser {
    pub(super) fn push_tab(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        let (folder, pane, mode, subscriptions) = self.build_tab_entities(dir, window, cx);
        self.tabs.push(BrowserTab {
            folder,
            pane,
            view_mode: mode,
            back: Vec::new(),
            forward: Vec::new(),
            _subscriptions: subscriptions,
        });
        self.activate_tab(self.tabs.len() - 1, window, cx);
        cx.notify();
    }

    /// Build a fresh Folder + FilePane pair already loading `dir`.
    ///
    /// Per-folder preferences load here so navigating restores what the
    /// folder remembers before its first frame paints.
    fn build_tab_entities(
        &self,
        dir: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<Folder>,
        Entity<FilePane>,
        ViewMode,
        Vec<Subscription>,
    ) {
        let (rules, mode, hidden_columns, show_hidden) = {
            let state = cx.global_mut::<FerretState>();
            let view = state.folder_view(&dir);
            let browser = &state.settings.browser;
            (
                SortRules {
                    key: view.sort_key,
                    order: view.sort_order,
                    directories_first: browser.directories_first,
                    natural: browser.natural_sort,
                },
                view.mode,
                view.hidden_columns.clone(),
                browser.show_hidden,
            )
        };

        let folder =
            cx.new(|_| Folder::new(dir.clone(), rules, show_hidden, ListingProfile::BROWSING));
        folder.update(cx, |f, cx| f.start_loading(cx));

        let pane = cx.new(|cx| {
            FilePane::new(
                folder.clone(),
                mode,
                &hidden_columns,
                self.git.handle(),
                window,
                cx,
            )
        });

        let mut subscriptions = vec![cx.subscribe_in(
            &pane,
            window,
            |this, _pane, event: &PaneEvent, window, cx| match event {
                PaneEvent::EnterDirectory(path) => this.navigate(path.clone(), window, cx),
                PaneEvent::OpenFile(path) => this.open_with_default(path.clone(), window, cx),
                PaneEvent::BatchRename(paths) => this.open_batch_rename(paths.clone(), window, cx),
                PaneEvent::CreateSymlink {
                    target,
                    target_kind,
                } => this.open_symlink_dialog(target.clone(), *target_kind, window, cx),
                PaneEvent::DropPaths {
                    sources,
                    dest,
                    force_copy,
                } => this.transfer(sources.clone(), dest.clone(), *force_copy, window, cx),
                // Sort/view preferences changed: persist them for this folder.
                PaneEvent::ViewChanged => this.record_current_view(cx),
            },
        )];
        // Listing progress repaints the status bar and title — and is also the
        // "something in this folder changed" signal git status needs
        // (`docs/git.html` §5, trigger 2). Requests coalesce behind a debounce,
        // so a streaming listing that emits one of these per batch still costs
        // exactly one status read once it settles.
        subscriptions.push(cx.subscribe_in(
            &folder,
            window,
            |this, _, event: &FolderEvent, _, cx| {
                if matches!(event, FolderEvent::RowsChanged | FolderEvent::LoadFinished) {
                    this.request_git_refresh(cx);
                }
                cx.notify();
            },
        ));
        // The preview pane follows the selection, and refreshes its cached
        // directory totals when a load finishes. Only the active tab drives
        // it — background tabs are ignored inside `sync_preview`.
        subscriptions.push(cx.subscribe_in(
            &folder,
            window,
            |this, folder, event: &FolderEvent, window, cx| match event {
                FolderEvent::SelectionChanged | FolderEvent::PathsChanged => {
                    this.sync_preview(folder, false, window, cx)
                }
                FolderEvent::LoadFinished => this.sync_preview(folder, true, window, cx),
                _ => {}
            },
        ));

        (folder, pane, mode, subscriptions)
    }

    pub(super) fn active(&self) -> &BrowserTab {
        &self.tabs[self.active]
    }

    pub(super) fn active_mut(&mut self) -> &mut BrowserTab {
        &mut self.tabs[self.active]
    }

    /// Make a tab visible and keep the persistent terminal aligned with the
    /// folder the window now shows.
    pub(super) fn activate_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        // Another tab's selection would replace the buffer: ask first, and
        // only the requested switch resumes afterwards.
        if index != self.active && self.preview.read(cx).is_dirty() {
            self.open_edit_guard(preview_panel::GuardPending::ActivateTab(index), window, cx);
            return;
        }
        self.active = index;
        let directory = self.active().folder.read(cx).dir.clone();
        self.sync_terminal_to(&directory, cx);
        self.git_follow(&directory, cx);
        let folder = self.active().folder.clone();
        self.sync_preview(&folder, false, window, cx);
        let focus = self.active().pane.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Swap in fresh entities for the active tab, pushing history.
    fn swap_active_tab(
        &mut self,
        dir: VfsPath,
        history: HistoryOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal_dir = dir.clone();
        self.record_current_view(cx);
        let current = self.active().folder.read(cx).dir.clone();
        let (folder, pane, mode, subscriptions) = self.build_tab_entities(dir, window, cx);

        let tab = self.active_mut();
        match history {
            HistoryOp::Push => {
                tab.back.push(current);
                tab.forward.clear();
            }
            HistoryOp::Back => {
                tab.back.pop();
                tab.forward.push(current);
            }
            HistoryOp::Forward => {
                tab.forward.pop();
                tab.back.push(current);
            }
        }
        tab.folder = folder;
        tab.pane = pane;
        tab.view_mode = mode;
        tab._subscriptions = subscriptions;

        self.path_editing = None;
        self.filter_task = None;
        // A filter belongs to the folder you typed it in, not to the tab.
        self.filter_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.sync_terminal_to(&terminal_dir, cx);
        self.git_follow(&terminal_dir, cx);
        let folder = self.active().folder.clone();
        self.sync_preview(&folder, false, window, cx);
        cx.notify();
    }

    pub(super) fn navigate(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.active().folder.read(cx).dir == dir {
            return;
        }
        // Leaving the directory over unsaved changes asks first: the new
        // folder's selection would otherwise replace the buffer.
        if self.preview.read(cx).is_dirty() {
            self.open_edit_guard(preview_panel::GuardPending::Navigate(dir), window, cx);
            return;
        }
        self.swap_active_tab(dir, HistoryOp::Push, window, cx);
    }

    pub(super) fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.active().back.last().cloned() else {
            return;
        };
        if self.preview.read(cx).is_dirty() {
            self.open_edit_guard(preview_panel::GuardPending::Back, window, cx);
            return;
        }
        self.swap_active_tab(previous, HistoryOp::Back, window, cx);
    }

    pub(super) fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.active().forward.last().cloned() else {
            return;
        };
        if self.preview.read(cx).is_dirty() {
            self.open_edit_guard(preview_panel::GuardPending::Forward, window, cx);
            return;
        }
        self.swap_active_tab(next, HistoryOp::Forward, window, cx);
    }

    pub(super) fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.active().folder.read(cx).dir.parent() else {
            return;
        };
        self.navigate(parent, window, cx);
    }

    /// The open folder vanished: walk up to the nearest parent that still
    /// exists, so the user lands somewhere real rather than on another
    /// missing page.
    ///
    /// The walk runs on the background executor. Each step is an `is_dir`
    /// stat, and the reason this is being called at all is that something in
    /// this chain has stopped answering — an unmounted share is precisely
    /// where a stat takes seconds, and this is reachable from a button.
    pub(super) fn go_to_existing_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let start = self.active().folder.read(cx).dir.clone();
        let expected_dir = start.clone();
        let home = cx.global::<FerretState>().home_dir();
        let window_handle = window.window_handle();
        let this = cx.weak_entity();

        cx.spawn(async move |_, cx| {
            let target = cx
                .background_executor()
                .spawn(async move {
                    let mut dir = start;
                    while let Some(parent) = dir.parent() {
                        if parent.as_local().is_some_and(Path::is_dir) {
                            return parent;
                        }
                        dir = parent;
                    }
                    VfsPath::local(home)
                })
                .await;

            let _ = window_handle.update(cx, |_, window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        if this.active().folder.read(cx).dir == expected_dir {
                            this.navigate(target, window, cx);
                        }
                    });
                }
            });
        })
        .detach();
    }

    /// Whether the active folder is gone (deleted or unreachable), which
    /// swaps the pane for a banner.
    pub(super) fn active_folder_is_gone(&self, cx: &Context<Self>) -> bool {
        self.active()
            .folder
            .read(cx)
            .error()
            .is_some_and(ferret_vfs::VfsError::is_gone)
    }

    pub(super) fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // One tab always stays so `+` has something to clone.
        }
        // Closing the tab whose file is being edited would drop the buffer:
        // ask first. Other tabs never touch it.
        if ix == self.active && self.preview.read(cx).is_dirty() {
            self.open_edit_guard(preview_panel::GuardPending::CloseTab(ix), window, cx);
            return;
        }
        let active_changed = ix == self.active;
        self.tabs.remove(ix);
        let next_active = if ix < self.active {
            self.active - 1
        } else {
            self.active.min(self.tabs.len() - 1)
        };
        self.active = next_active;
        if active_changed {
            let directory = self.active().folder.read(cx).dir.clone();
            self.sync_terminal_to(&directory, cx);
            // The tab that closed may have been the one in a repository, so the
            // segment has to follow the tab that is now on screen rather than
            // keep describing one nobody can see.
            self.git_follow(&directory, cx);
            let folder = self.active().folder.clone();
            self.sync_preview(&folder, false, window, cx);
            let focus = self.active().pane.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Persist the current folder's view preferences: on every sort or
    /// view-mode change, and again before navigating away.
    pub(super) fn record_current_view(&mut self, cx: &mut Context<Self>) {
        let tab = self.active();
        let dir = tab.folder.read(cx).dir.clone();
        let (sort_key, sort_order) = {
            let f = tab.folder.read(cx);
            (f.sort_key(), f.sort_order())
        };
        // The mode comes from the pane, not this tab's cached copy: a mode
        // set from the pane's own menu must not be reverted by a later sort.
        let view = FolderView {
            mode: tab.pane.read(cx).view_mode(),
            sort_key,
            sort_order,
            hidden_columns: tab.pane.read(cx).hidden_column_keys(),
        };
        cx.global_mut::<FerretState>()
            .remember_folder_view(&dir, view);
        // Debounced, not written here: this runs on every sort click, every
        // column toggle and every folder entered, and the store is up to
        // FOLDER_PREFS_CAPACITY records (A-09).
        crate::state::arm_config_save(cx);
    }
}
