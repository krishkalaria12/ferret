//! The listing lifecycle: streaming a directory in, backfilling metadata,
//! and watching it for changes.
//!
//! Everything here needs a GPUI context and is driven by the file pane.
//! Nothing blocks a frame: batches are appended as they land, and a stale
//! generation makes a background walk drop its own results.

use super::*;

impl Folder {
    // ------------------------------------------------------------------
    // Listing lifecycle (needs a GPUI context; driven by the pane)

    /// Begin (or restart) streaming the listing of `self.dir`.
    ///
    /// Runs on the GPUI executor; batches arrive via [`Self::apply_batch`]
    /// and completion via [`Self::finish_loading`]. Stale generations drop
    /// their work silently — navigation ahead of a slow walk is normal.
    pub fn start_loading(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;

        // The user's identity state is theirs and survives a refresh, but the
        // ids it is expressed in belong to the listing being discarded. Carry
        // it across as paths; `apply_batch` resolves each one back to a fresh
        // id as its entry lands, so a selected row is painted selected the
        // moment it reappears.
        self.carried = CarriedState {
            selection: self.selected_ids_as_paths(),
            anchor: self
                .anchor
                .and_then(|id| Some(self.entry(id)?.path.clone())),
            cursor: self
                .cursor
                .and_then(|id| Some(self.entry(id)?.path.clone())),
            // Which folders are *open* is intent too; the children themselves
            // are re-read once the parent listing lands.
            expanded: self
                .expanded
                .iter()
                .filter_map(|&id| Some(self.entry(id)?.path.clone()))
                .collect(),
        };

        // Every index derived from the old listing goes with it. Leaving the
        // slab behind is not cosmetic: a watcher probe arriving during the
        // reload would resolve a stale name to a slot that now holds a
        // different entry, and overwrite that one instead.
        self.slab.clear();
        self.free.clear();
        self.entries.clear();
        self.order.clear();
        self.row_of_id.clear();
        self.by_path.clear();
        self.by_name.clear();
        self.children.clear();
        self.expanded.clear();
        // Child listings from the previous generation will see the bumped
        // generation and drop their results without decrementing, so the
        // count starts again rather than being adjusted.
        self.pending_child_loads = 0;
        self.selection.clear();
        self.anchor = None;
        self.cursor = None;
        self.selected_visible_count = 0;
        self.selected_visible_bytes = 0;
        self.invalidate_selected_paths();
        self.error = None;
        self.loading = true;
        self.loading_since = Some(Instant::now());
        self.watch = None;
        cx.notify();

        let fs = cx.global::<crate::state::FerretState>().fs.clone();
        let dir = self.dir.clone();
        // An ancestor column draws an icon, a name and a chevron; asking for
        // size and dates it will never show is a `stat` per entry for nothing
        // (A-14). `detail` decides whether that is actually safe here.
        // The folder's backend, not the router's: whether metadata rides along
        // with the listing is a property of the server this directory is on.
        let detail = self
            .profile
            .detail(self.rules, self.show_hidden, fs.capabilities_for(&dir));

        // Nothing else will wake the view once the delay passes — the whole
        // point of the hint is that no rows have arrived — so the deadline
        // schedules its own repaint.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_HINT_DELAY).await;
            let _ = this.update(cx, |folder, cx| {
                if folder.generation == generation && folder.is_slow_to_start() {
                    cx.notify();
                }
            });
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&dir, detail);
            let mut first_error: Option<VfsError> = None;

            while let Some(item) = stream.next().await {
                match item {
                    Ok(batch) => {
                        let ok = this.update(cx, |folder, cx| {
                            if folder.generation != generation {
                                return false;
                            }
                            folder.apply_batch(batch, cx);
                            true
                        });
                        if ok.is_err() {
                            return; // Folder dropped: stop walking.
                        }
                    }
                    Err(err) => {
                        // A failed open ends the stream; a bad child does not.
                        first_error = Some(err);
                        break;
                    }
                }
            }

            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation {
                    return;
                }
                folder.finish_loading(first_error, cx);
            });
        })
        .detach();
    }

    /// The selected entries' paths, for carrying a selection over a reload.
    fn selected_ids_as_paths(&self) -> std::collections::HashSet<VfsPath> {
        self.selection
            .iter()
            .filter_map(|&id| Some(self.entry(id)?.path.clone()))
            .collect()
    }

    fn apply_batch(&mut self, batch: Vec<Entry>, cx: &mut Context<Self>) {
        // Appended, never rebuilt: a full re-index per batch is O(n) forty
        // times over on a 200,000-entry walk, which is Gate 1 gone. Arrival
        // order *is* display order while loading, and nothing can be expanded
        // yet, so every new row is a flat one at the end.
        let restoring = !self.carried.is_empty();
        for entry in batch {
            let path = entry.path.clone();
            let visible = self.is_visible(&entry);
            let id = self.insert(entry, Placement::Top);
            self.entries.push(id);
            if visible {
                self.row_of_id.insert(id, self.order.len());
                self.order.push(Row { id, depth: 0 });
            }
            if restoring {
                self.restore_carried(id, &path);
            }
        }
        if restoring {
            self.recount_selection();
        }
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    /// Re-attach whatever the reload carried for `path` to its fresh id.
    ///
    /// Expansion is restored here too, but the children behind it are not:
    /// `reload_expanded_children` re-reads them once the parent listing is
    /// complete, because a listing spliced in mid-stream would shift every
    /// row below it.
    pub(super) fn restore_carried(&mut self, id: EntryId, path: &VfsPath) {
        if self.carried.selection.remove(path) {
            self.selection.insert(id);
        }
        if self.carried.anchor.as_ref() == Some(path) {
            self.carried.anchor = None;
            self.anchor = Some(id);
        }
        if self.carried.cursor.as_ref() == Some(path) {
            self.carried.cursor = None;
            self.cursor = Some(id);
        }
        if self.carried.expanded.remove(path) {
            self.expanded.insert(id);
        }
    }

    /// Drop whatever the reload carried, once nothing can still resolve it.
    ///
    /// Anything left over names a path the new listing does not have — it was
    /// deleted or renamed while we were away — so it is forgotten rather than
    /// held for a row that will never arrive. The wait for
    /// `pending_child_loads` is the whole point: a selected row inside an
    /// expanded folder is only resolvable once that folder's listing lands,
    /// which is several async hops after the top-level walk finished.
    pub(super) fn settle_carried(&mut self) {
        if self.pending_child_loads > 0 || self.carried.is_empty() {
            return;
        }
        self.carried = CarriedState::default();
    }

    fn finish_loading(&mut self, error: Option<VfsError>, cx: &mut Context<Self>) {
        self.loading = false;
        self.loading_since = None;
        if error.as_ref().is_some_and(VfsError::is_gone) {
            // Nothing is coming back, so nothing can still resolve what the
            // reload carried.
            self.carried = CarriedState::default();
            self.error = Some(VfsError::NotFound {
                path: self.dir.clone(),
            });
            cx.emit(FolderEvent::Gone);
            cx.notify();
            return;
        }
        self.error = error;
        self.resort();
        self.attach_watch(cx);
        // Before `settle_carried`, and that order is load-bearing: this is
        // what raises `pending_child_loads` for the folders whose listings
        // still owe the carried state a home.
        self.reload_expanded_children(cx);
        self.settle_carried();
        self.start_metadata_backfill(cx);
        self.request_os_icons(cx);
        cx.emit(FolderEvent::LoadFinished);
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn attach_watch(&mut self, cx: &mut Context<Self>) {
        if !self.profile.watch {
            // A listing nobody is working in does not hold a subscription
            // (A-14) — and on macOS every registration costs the directories
            // that *are* being worked in a gap in delivery.
            return;
        }
        let fs = cx.global::<crate::state::FerretState>().fs.clone();
        let watch = match fs.watch(&self.dir) {
            Ok(watch) => watch,
            // No notification is a degraded mode, not a failure: manual
            // refresh still works.
            Err(_) => return,
        };
        let rx = watch.events.clone();
        self.watch = Some(watch);

        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                // Debounce bursts: editors rewrite files several times in a
                // few tens of milliseconds.
                cx.background_executor()
                    .timer(ferret_vfs::watch::DEBOUNCE)
                    .await;

                let mut paths: Vec<std::path::PathBuf> = Vec::new();
                let mut gone = false;
                let mut desynced = false;
                match event {
                    WatchEvent::Changed(changed) => paths.extend(changed),
                    WatchEvent::Desynced => desynced = true,
                    WatchEvent::Gone => gone = true,
                }
                // Drain whatever else piled up behind the debounce.
                while let Ok(more) = rx.try_recv() {
                    match more {
                        WatchEvent::Changed(changed) => paths.extend(changed),
                        WatchEvent::Desynced => desynced = true,
                        WatchEvent::Gone => gone = true,
                    }
                }

                let ok = this.update(cx, |folder, cx| {
                    if folder.generation != generation {
                        return false;
                    }
                    if gone {
                        folder.mark_gone(cx);
                    } else if desynced {
                        // A full watcher queue means the individual paths no
                        // longer form a complete delta. One clean walk is the
                        // only way to converge with the filesystem again.
                        folder.reload_preserving_state(cx);
                    } else if !paths.is_empty() {
                        // A huge burst is cheaper to answer with one clean
                        // walk than with hundreds of stats.
                        if paths.len() > WATCH_RELOAD_THRESHOLD {
                            folder.reload_preserving_state(cx);
                        } else {
                            folder.start_watch_apply(paths, cx);
                        }
                    }
                    true
                });
                if ok.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Turn watcher paths into row updates: one `probe` per path, applied in
    /// a single model update.
    ///
    /// Paths outside `self.dir` are dropped. That filter used to be dead code
    /// dressed as a safeguard — the watch is `NonRecursive`, so events for an
    /// expanded folder's children never arrived to be filtered. It is live
    /// now: the registry keys on canonicalised paths, so a directory reached
    /// through a symlink can deliver a sibling's event, and this is what keeps
    /// that out.
    ///
    /// **Expanded subtrees are a snapshot, by design** (A-24). A folder opened
    /// in place shows what it held when it was opened, and refreshes when it
    /// is collapsed and reopened, or when the browsed directory reloads. The
    /// registry made watching each one cheap enough to reconsider — one
    /// subscription rather than one native stream — but not free: on macOS
    /// `notify` restarts its whole FSEvents stream on every registration, and
    /// every restart is a window where *other* watched directories lose
    /// events (see `ferret_vfs::watch`). Paying that so a casually-opened
    /// disclosure triangle stays live is the wrong trade. If expanded
    /// subtrees ever need to be live, the fix is a recursive watch on
    /// `self.dir` bounded by the expanded set — one registration, not one per
    /// open row.
    fn start_watch_apply(&mut self, mut paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs.clone();
        let generation = self.generation;
        let dir = self.dir.clone();

        cx.spawn(async move |this, cx| {
            let mut probed = Vec::with_capacity(paths.len());
            for path in paths.drain(..) {
                let under_dir = dir
                    .as_local()
                    .zip(path.parent())
                    .is_some_and(|(d, parent)| parent == d);
                if !under_dir {
                    continue;
                }
                let result = fs.probe(&VfsPath::local(path.clone())).await;
                probed.push((path, result));
            }
            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation {
                    return;
                }
                folder.integrate_probes(probed, cx);
            });
        })
        .detach();
    }

    fn integrate_probes(
        &mut self,
        probed: Vec<(std::path::PathBuf, ferret_vfs::Result<Entry>)>,
        cx: &mut Context<Self>,
    ) {
        if self.apply_probes(probed) {
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        }
    }
}
