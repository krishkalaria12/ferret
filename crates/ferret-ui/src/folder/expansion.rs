//! Inline expansion (§8): opening a folder in place, beneath its own row,
//! without changing the browsed directory.
//!
//! Expanded listings are kept beside the browsed directory's own rather
//! than spliced into it, so sorting, the watcher and the metadata backfill
//! all keep working on the flat listing they were written for.

use super::sort::cmp_entries;
use super::*;

impl Folder {
    // ------------------------------------------------------------------
    // Inline expansion (§8)

    /// Expand or collapse the folder at `row` in place, without changing the
    /// browsed directory.
    ///
    /// Returns whether anything happened, so callers can tell a no-op on a
    /// file row from a real toggle.
    pub fn toggle_expanded(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.entry_at(row) else {
            return false;
        };
        if !entry.is_dir_like() {
            return false;
        }
        let path = entry.path.clone();

        if self.expanded.remove(&path) {
            // Collapsing drops the loaded children: re-reading on the next
            // expand is cheap, and holding them would keep a deep tree's
            // worth of entries alive for a row nobody can see.
            self.forget_subtree(&path);
            self.rebuild_index_and_order();
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
            return true;
        }

        self.expanded.insert(path.clone());
        self.load_children(path, cx);
        cx.notify();
        true
    }

    /// Expand a folder row, or do nothing if it already is (→ key).
    pub fn expand(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        match self.expansion_at(row) {
            Some(false) => self.toggle_expanded(row, cx),
            _ => false,
        }
    }

    /// Collapse a folder row, or do nothing if it is not expanded (← key).
    pub fn collapse(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        match self.expansion_at(row) {
            Some(true) => self.toggle_expanded(row, cx),
            _ => false,
        }
    }

    /// Drop a collapsed folder's children, and its descendants' with it.
    pub(super) fn forget_subtree(&mut self, parent: &VfsPath) {
        let Some(children) = self.children.remove(parent) else {
            return;
        };
        for child in children {
            if self.expanded.remove(&child.path) {
                self.forget_subtree(&child.path);
            }
        }
    }

    /// Read one expanded folder's listing.
    ///
    /// Collected rather than streamed: a child listing is spliced into the
    /// middle of the display order, so rows arriving one batch at a time
    /// would shift everything below them repeatedly.
    fn load_children(&mut self, parent: VfsPath, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        let generation = self.generation;
        let (key, order) = (self.sort_key, self.sort_order);

        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&parent);
            let mut entries = Vec::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(batch) => entries.extend(batch),
                    // An unreadable child folder expands to nothing rather
                    // than failing the whole listing it sits in.
                    Err(_) => break,
                }
            }
            entries.sort_by(|a, b| cmp_entries(a, b, key, order));

            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation || !folder.expanded.contains(&parent) {
                    return; // Navigated away, or collapsed while loading.
                }
                folder.children.insert(parent, entries);
                folder.rebuild_index_and_order();
                folder.start_metadata_backfill(cx);
                folder.request_os_icons(cx);
                cx.emit(FolderEvent::RowsChanged);
                cx.notify();
            });
        })
        .detach();
    }

    /// The pure model half of [`Self::integrate_probes`]; returns whether
    /// anything changed.
    pub(super) fn apply_probes(
        &mut self,
        probed: Vec<(std::path::PathBuf, ferret_vfs::Result<Entry>)>,
    ) -> bool {
        let mut changed = false;
        let mut metadata_changed = false;
        let mut updates = HashMap::new();
        let mut removed = HashSet::new();

        for (path, result) in probed {
            let vp = VfsPath::local(path);
            match result {
                Ok(entry) => {
                    // The last probe for a path wins. In particular, a file
                    // removed and recreated during one watcher burst remains
                    // present rather than being removed by an earlier probe.
                    removed.remove(&vp);
                    updates.insert(vp, entry);
                }
                Err(err) if err.is_gone() => {
                    updates.remove(&vp);
                    removed.insert(vp);
                }
                // Transient stat failures leave the row as it was; the next
                // event or a manual refresh corrects it.
                Err(_) => {}
            }
        }

        let mut inserts = Vec::new();
        for (path, entry) in updates {
            let Some(ix) = self.top_index.get(&path).copied() else {
                inserts.push(entry);
                continue;
            };
            let Some(existing) = self.entries.get_mut(ix) else {
                // `top_index` is rebuilt with `entries`; this is defensive
                // against an interrupted reload, where treating the probe as
                // a create is safer than overwriting an unrelated row.
                inserts.push(entry);
                continue;
            };
            debug_assert_eq!(existing.path, path);
            metadata_changed |= existing.metadata != entry.metadata;
            *existing = entry;
            changed = true;
        }

        // Do not remove or insert while using `top_index`: either operation
        // shifts the remaining indices. All replacements above have used the
        // pre-batch index, so structural mutation can now happen in one pass.
        if !removed.is_empty() {
            let old_len = self.entries.len();
            self.entries.retain(|entry| !removed.contains(&entry.path));
            changed |= self.entries.len() != old_len;
        }

        for entry in inserts {
            // A successful probe for a path absent from the old listing is a
            // create. Keep it sorted without consulting an index invalidated
            // by earlier insertions in this batch.
            let pos = self.entries.partition_point(|existing| {
                cmp_entries(existing, &entry, self.sort_key, self.sort_order)
                    == std::cmp::Ordering::Less
            });
            self.entries.insert(pos, entry);
            changed = true;
        }

        if !changed {
            return false;
        }
        // Size/Date ordering can be invalidated by metadata that arrived
        // after the sort; Name order never is.
        if metadata_changed
            && matches!(
                self.sort_key,
                ferret_settings::SortKey::Modified | ferret_settings::SortKey::Size
            )
        {
            self.entries
                .sort_by(|a, b| cmp_entries(a, b, self.sort_key, self.sort_order));
        }
        self.rebuild_index_and_order();
        true
    }

    pub(super) fn mark_gone(&mut self, cx: &mut Context<Self>) {
        self.error = Some(VfsError::NotFound {
            path: self.dir.clone(),
        });
        self.loading = false;
        self.watch = None;
        cx.emit(FolderEvent::Gone);
        cx.notify();
    }

    /// Re-list the same directory, keeping selection and cursor by path.
    pub fn reload_preserving_state(&mut self, cx: &mut Context<Self>) {
        self.start_loading(cx);
    }

    /// Re-read the listings of folders that were open before a reload.
    ///
    /// `start_loading` drops the cached children but keeps `expanded`, so a
    /// refresh has to fetch them again — otherwise a folder stays marked open
    /// with nothing under it. Only the ones that still exist are re-read; the
    /// rest are forgotten here, since nothing else will.
    pub(super) fn reload_expanded_children(&mut self, cx: &mut Context<Self>) {
        if self.expanded.is_empty() {
            return;
        }
        // A folder that survived the reload is worth re-opening; one that did
        // not is no longer expandable.
        let (alive, gone): (Vec<VfsPath>, Vec<VfsPath>) = self
            .expanded
            .iter()
            .cloned()
            .partition(|p| self.known_paths.contains(p));
        for path in gone {
            self.expanded.remove(&path);
        }
        for path in alive {
            self.load_children(path, cx);
        }
    }

    /// Ask the platform for the real icons of the types now listed (D-18).
    ///
    /// Deduplicated by type inside the provider, so this is a handful of OS
    /// calls even for a 200,000-entry directory, and it runs off the render
    /// path — a row draws the bundled icon until the real one lands.
    pub(super) fn request_os_icons(&mut self, cx: &mut Context<Self>) {
        let provider = cx.global::<crate::state::FerretState>().os_icons.clone();
        let wanted = crate::icons::collect_os_icon_requests(
            self.entries.iter().chain(self.children.values().flatten()),
            provider.as_ref(),
        );
        if wanted.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.update(|cx| crate::icons::resolve_os_icons(wanted, cx));
            // The provider fills its cache in the background; one repaint
            // shortly after is enough to pick the results up.
            cx.background_executor().timer(OS_ICON_REPAINT_DELAY).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    pub(super) fn start_metadata_backfill(&mut self, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        if fs.capabilities().metadata_in_listing {
            return; // Windows filled it during the walk.
        }

        let generation = self.generation;
        let targets: Vec<VfsPath> = self
            .entries
            .iter()
            .chain(self.children.values().flatten())
            .filter(|e| e.metadata.is_none())
            .map(|e| e.path.clone())
            .collect();
        if targets.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            for chunk in targets.chunks(METADATA_BATCH_SIZE) {
                let filled = fs
                    .metadata_batch(chunk.to_vec())
                    .await
                    .into_iter()
                    .filter_map(|(path, result)| result.ok().map(|metadata| (path, metadata)))
                    .collect();
                let done = targets.last() == chunk.last();
                let ok = this.update(cx, |folder, cx| {
                    if folder.generation != generation {
                        return false;
                    }
                    folder.apply_metadata(filled, done, cx);
                    true
                });
                if ok.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn apply_metadata(
        &mut self,
        filled: Vec<(VfsPath, ferret_vfs::Metadata)>,
        final_chunk: bool,
        cx: &mut Context<Self>,
    ) {
        let mut selection_changed = false;
        for (path, md) in filled {
            selection_changed |= self.selection.contains(&path);
            if let Some(entry) = self.entry_mut(&path) {
                entry.metadata = Some(md);
            }
        }
        if selection_changed {
            // A selected row may have acquired its first size. Keep the
            // status-bar cache coherent with the row repaint below.
            self.refresh_selection_summary();
        }

        let now = Instant::now();
        let due = self
            .last_meta_notify
            .map(|t| now.duration_since(t) >= META_NOTIFY_EVERY)
            .unwrap_or(true);
        if due || final_chunk {
            self.last_meta_notify = Some(now);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        }
    }
}
