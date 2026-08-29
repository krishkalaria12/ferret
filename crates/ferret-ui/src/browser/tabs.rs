//! Tabs and history: opening, closing and switching tabs, and the
//! navigation that moves each one through its own back/forward stack.
//!
//! A tab owns its folder, pane and history, so switching tabs swaps
//! entities rather than re-listing a directory that is already loaded.

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
        self.active = self.tabs.len() - 1;
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
        let ((sort_key, sort_order), mode, hidden_columns, show_hidden) = {
            let state = cx.global_mut::<FerretState>();
            let view = state.folder_view(&dir);
            (
                view.sort_key_and_order(),
                view.mode,
                view.hidden_columns.clone(),
                state.settings.browser.show_hidden,
            )
        };

        let folder = cx.new(|_| Folder::new(dir.clone(), sort_key, sort_order, show_hidden));
        folder.update(cx, |f, cx| f.start_loading(cx));

        let pane = cx.new(|cx| FilePane::new(folder.clone(), mode, &hidden_columns, window, cx));

        let mut subscriptions = vec![cx.subscribe_in(
            &pane,
            window,
            |this, _pane, event: &PaneEvent, window, cx| match event {
                PaneEvent::EnterDirectory(path) => this.navigate(path.clone(), window, cx),
                PaneEvent::OpenFile(path) => this.open_with_default(path.clone(), window, cx),
                PaneEvent::DropPaths {
                    sources,
                    dest,
                    force_copy,
                } => this.transfer(sources.clone(), dest.clone(), *force_copy, window, cx),
                // Sort/view preferences changed: persist them for this folder.
                PaneEvent::ViewChanged => this.record_current_view(cx),
            },
        )];
        // Listing progress repaints the status bar and title.
        subscriptions.push(cx.subscribe_in(&folder, window, |_, _, _, _, cx| {
            cx.notify();
        }));

        (folder, pane, mode, subscriptions)
    }

    pub(super) fn active(&self) -> &BrowserTab {
        &self.tabs[self.active]
    }

    pub(super) fn active_mut(&mut self) -> &mut BrowserTab {
        &mut self.tabs[self.active]
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
        // A filter belongs to the folder you typed it in, not to the tab.
        self.filter_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.sync_terminal_to(&terminal_dir, cx);
        cx.notify();
    }

    pub(super) fn navigate(&mut self, dir: VfsPath, window: &mut Window, cx: &mut Context<Self>) {
        if self.active().folder.read(cx).dir == dir {
            return;
        }
        self.swap_active_tab(dir, HistoryOp::Push, window, cx);
    }

    pub(super) fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(previous) = self.active().back.last().cloned() else {
            return;
        };
        self.swap_active_tab(previous, HistoryOp::Back, window, cx);
    }

    pub(super) fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.active().forward.last().cloned() else {
            return;
        };
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
    pub(super) fn go_to_existing_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dir = self.active().folder.read(cx).dir.clone();
        while let Some(parent) = dir.parent() {
            if parent.as_local().is_some_and(Path::is_dir) {
                self.navigate(parent, window, cx);
                return;
            }
            dir = parent;
        }
        let home = cx.global::<FerretState>().home_dir();
        self.navigate(VfsPath::local(home), window, cx);
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

    pub(super) fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return; // One tab always stays so `+` has something to clone.
        }
        self.tabs.remove(ix);
        self.active = self.active.min(self.tabs.len() - 1);
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
        let state = cx.global_mut::<FerretState>();
        state.remember_folder_view(&dir, view);
        state.save_prefs();
    }
}
