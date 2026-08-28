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

        // Every index derived from the old listing goes with it. Leaving
        // `top_index` behind is not cosmetic: a watcher probe arriving during
        // the reload would look a stale path up, get an index that now points
        // at a different entry, and overwrite that one instead.
        self.entries.clear();
        self.order.clear();
        self.row_of_path.clear();
        self.top_index.clear();
        self.known_paths.clear();
        self.selected_visible_paths = std::sync::Arc::from([]);
        self.selected_visible_bytes = 0;
        // The children are re-read once the parent listing lands; which
        // folders are *open* is the user's intent and survives a refresh.
        self.children.clear();
        self.error = None;
        self.loading = true;
        self.loading_since = Some(Instant::now());
        // Keep the selection paths: a watcher reload must preserve them even
        // though rows will be rebuilt.
        self.watch = None;
        cx.notify();

        let fs = cx.global::<crate::state::FerretState>().fs;
        let dir = self.dir.clone();

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
            let mut stream = fs.read_dir(&dir);
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

    fn apply_batch(&mut self, batch: Vec<Entry>, cx: &mut Context<Self>) {
        // Appended, never rebuilt: a full re-index per batch is O(n) forty
        // times over on a 200,000-entry walk, which is Gate 1 gone. Arrival
        // order *is* display order while loading, and nothing can be expanded
        // yet, so every new row is a flat one at the end.
        for entry in batch {
            let ix = self.entries.len();
            let path = entry.path.clone();
            let visible = self.is_visible(&entry);
            self.entries.push(entry);
            self.top_index.insert(path.clone(), ix);
            self.known_paths.insert(path.clone());
            if visible {
                self.row_of_path.insert(path, self.order.len());
                self.order.push(Row {
                    source: RowSource::Top(ix),
                    depth: 0,
                });
            }
        }
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn finish_loading(&mut self, error: Option<VfsError>, cx: &mut Context<Self>) {
        self.loading = false;
        self.loading_since = None;
        if error.as_ref().is_some_and(VfsError::is_gone) {
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
        self.reload_expanded_children(cx);
        self.start_metadata_backfill(cx);
        self.request_os_icons(cx);
        cx.emit(FolderEvent::LoadFinished);
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn attach_watch(&mut self, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
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
    fn start_watch_apply(&mut self, mut paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
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
