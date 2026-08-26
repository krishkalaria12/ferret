//! One browsed directory: the streamed listing plus everything the file pane
//! manipulates — sort, filter, selection.
//!
//! Two rules shape this type:
//!
//! 1. **Nothing blocks a frame.** Entries arrive from the VFS stream in
//!    batches and are appended as they land (`docs/ui-base.html` §9: show
//!    what has arrived, no spinner-then-swap). Sorting happens once, when the
//!    walk finishes — re-sorting mid-stream jumbles rows under the user's
//!    cursor for no benefit on directories small enough to care.
//! 2. **Selection survives re-sorting.** Selection, anchor and cursor are
//!    stored as paths, never indices. A watcher reload reshuffles rows; the
//!    user's selection must not follow the shuffle.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use ferret_settings::{SortKey, SortOrder};
use ferret_vfs::{Entry, Vfs, VfsError, VfsPath, WatchEvent};
use futures::StreamExt;
use gpui::{Context, EventEmitter};

/// How many entries accumulate before a batch is flushed to the model.
///
/// Large enough that a 200,000-entry walk costs ~50 model updates rather than
/// 200,000; small enough that rows visibly stream in.
pub const BATCH_SIZE: usize = 4_096;

/// How often the metadata backfill reports progress to the UI.
pub const META_NOTIFY_EVERY: Duration = Duration::from_millis(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderEvent {
    /// Row content or order changed; the list should repaint.
    RowsChanged,
    /// Only the selection moved.
    SelectionChanged,
    /// Loading finished (successfully or not).
    LoadFinished,
    /// The watched directory vanished; offer the nearest existing parent.
    Gone,
}

pub struct Folder {
    pub dir: VfsPath,

    entries: Vec<Entry>,
    /// Visible rows only: display position → index into `entries`.
    order: Vec<usize>,
    row_of_path: HashMap<VfsPath, usize>,

    loading: bool,
    error: Option<VfsError>,

    sort_key: SortKey,
    sort_order: SortOrder,
    show_hidden: bool,
    filter: Option<String>,

    selection: HashSet<VfsPath>,
    anchor: Option<VfsPath>,
    cursor: Option<VfsPath>,

    /// Bumps on every navigation/reload; background tasks check it before
    /// touching state, which is how stale walks cancel themselves.
    generation: u64,
    watch: Option<ferret_vfs::Watch>,
    last_meta_notify: Option<Instant>,
}

impl Folder {
    pub fn new(dir: VfsPath, sort_key: SortKey, sort_order: SortOrder, show_hidden: bool) -> Self {
        Self {
            dir,
            entries: Vec::new(),
            order: Vec::new(),
            row_of_path: HashMap::new(),
            loading: true,
            error: None,
            sort_key,
            sort_order,
            show_hidden,
            filter: None,
            selection: HashSet::new(),
            anchor: None,
            cursor: None,
            generation: 0,
            watch: None,
            last_meta_notify: None,
        }
    }

    // ------------------------------------------------------------------
    // Introspection for the views

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn error(&self) -> Option<&VfsError> {
        self.error.as_ref()
    }

    pub fn row_count(&self) -> usize {
        self.order.len()
    }

    pub fn entry_at(&self, row: usize) -> Option<&Entry> {
        self.order.get(row).map(|&ix| &self.entries[ix])
    }

    #[allow(dead_code)] // wired to the filter UI and status bar polish
    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }

    pub fn sort_key(&self) -> SortKey {
        self.sort_key
    }

    pub fn sort_order(&self) -> SortOrder {
        self.sort_order
    }

    #[allow(dead_code)]
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    #[allow(dead_code)]
    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// Whether the row at `row` is in the selection.
    pub fn is_row_selected(&self, row: usize) -> bool {
        self.entry_at(row)
            .is_some_and(|e| self.selection.contains(&e.path))
    }

    /// Paths of all selected entries, in display order.
    pub fn selected_paths(&self) -> Vec<VfsPath> {
        self.order
            .iter()
            .filter_map(|&ix| {
                let e = &self.entries[ix];
                self.selection.contains(&e.path).then(|| e.path.clone())
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn has_selection(&self) -> bool {
        !self.selection.is_empty()
    }

    pub fn selection_count(&self) -> usize {
        self.selection.len()
    }

    /// Sum of selected sizes. Directories contribute nothing rather than a
    /// lie (`docs/ui-base.html` §9).
    pub fn selection_size(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| self.selection.contains(&e.path))
            .filter_map(|e| e.display_len())
            .sum()
    }

    pub fn cursor_row(&self) -> Option<usize> {
        let ix = *self.cursor.as_ref().and_then(|p| self.row_of_path.get(p))?;
        self.order.iter().position(|&o| o == ix)
    }

    /// The cursor row's path, for Open/Rename actions.
    pub fn cursor_path(&self) -> Option<VfsPath> {
        self.cursor.clone()
    }

    pub fn has_entry(&self, path: &VfsPath) -> bool {
        self.row_of_path.contains_key(path)
    }

    /// Every listed name, for conflict-free "new file" naming.
    pub fn entry_names(&self) -> std::collections::HashSet<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// Whether the entry at `path` navigates like a directory.
    pub fn is_dir_like(&self, path: &VfsPath) -> Option<bool> {
        self.row_of_path
            .get(path)
            .and_then(|ix| self.entries.get(*ix))
            .map(|e| e.is_dir_like())
    }

    // ------------------------------------------------------------------
    // Selection

    /// Select exactly one row (plain click, arrow-key move).
    pub fn select_only(&mut self, row: usize) {
        let Some(path) = self.path_at(row) else {
            return;
        };
        self.selection.clear();
        self.selection.insert(path.clone());
        self.anchor = Some(path.clone());
        self.cursor = Some(path);
    }

    /// Toggle one row in the selection (Cmd/Ctrl-click).
    pub fn toggle_select(&mut self, row: usize) {
        let Some(path) = self.path_at(row) else {
            return;
        };
        if !self.selection.remove(&path) {
            self.selection.insert(path.clone());
        }
        self.anchor = Some(path.clone());
        self.cursor = Some(path);
    }

    /// Extend the selection from the anchor through `row` (Shift-click).
    pub fn extend_select(&mut self, row: usize) {
        let Some(anchor_row) = self
            .anchor
            .as_ref()
            .and_then(|p| self.row_of_path.get(p))
            .and_then(|ix| self.order.iter().position(|&o| o == *ix))
        else {
            self.select_only(row);
            return;
        };
        let (lo, hi) = if anchor_row <= row {
            (anchor_row, row)
        } else {
            (row, anchor_row)
        };
        self.selection.clear();
        for r in lo..=hi {
            if let Some(e) = self.entry_at(r) {
                self.selection.insert(e.path.clone());
            }
        }
        self.cursor = self.path_at(row);
    }

    /// Right-click on an unselected row selects it first (Finder behaviour).
    pub fn prepare_context_menu(&mut self, row: usize) {
        if !self.is_row_selected(row) {
            self.select_only(row);
        }
    }

    pub fn select_all(&mut self) {
        self.selection.clear();
        for ix in &self.order {
            self.selection.insert(self.entries[*ix].path.clone());
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.selection.clear();
    }

    fn path_at(&self, row: usize) -> Option<VfsPath> {
        self.entry_at(row).map(|e| e.path.clone())
    }

    // ------------------------------------------------------------------
    // Sorting and filtering

    pub fn set_sort(&mut self, key: SortKey, order: SortOrder) {
        let changed = self.sort_key != key || self.sort_order != order;
        self.sort_key = key;
        self.sort_order = order;
        if changed && !self.loading {
            self.resort();
        }
    }

    #[allow(dead_code)]
    pub fn toggle_sort_order(&mut self) {
        self.sort_order = match self.sort_order {
            SortOrder::Ascending => SortOrder::Descending,
            SortOrder::Descending => SortOrder::Ascending,
        };
        if !self.loading {
            self.resort();
        }
    }

    pub fn set_show_hidden(&mut self, show: bool) {
        if self.show_hidden == show {
            return;
        }
        self.show_hidden = show;
        if !self.loading {
            self.rebuild_order();
        }
    }

    #[allow(dead_code)] // the inline filter box arrives with view modes
    pub fn set_filter(&mut self, filter: Option<String>) {
        self.filter = filter;
        if !self.loading {
            self.rebuild_order();
        }
    }

    /// Type-ahead: select the first row at or after `from` whose name starts
    /// with `prefix`, wrapping around. Returns whether anything matched.
    pub fn select_by_prefix(&mut self, prefix: &str, from: usize) -> bool {
        let n = self.order.len();
        if n == 0 || prefix.is_empty() {
            return false;
        }
        let prefix = prefix.to_lowercase();
        for step in 0..n {
            let row = (from + step) % n;
            let matches = self
                .entry_at(row)
                .is_some_and(|e| e.name.to_lowercase().starts_with(&prefix));
            if matches {
                self.select_only(row);
                return true;
            }
        }
        false
    }

    fn resort(&mut self) {
        let (key, order) = (self.sort_key, self.sort_order);
        self.entries.sort_by(|a, b| cmp_entries(a, b, key, order));
        self.rebuild_index_and_order();
    }

    fn rebuild_order(&mut self) {
        // Preserve the sorted entry order; just refresh visibility.
        self.rebuild_index_and_order();
    }

    fn rebuild_index_and_order(&mut self) {
        self.row_of_path.clear();
        for (ix, e) in self.entries.iter().enumerate() {
            self.row_of_path.insert(e.path.clone(), ix);
        }
        self.order = (0..self.entries.len())
            .filter(|&ix| self.is_visible(&self.entries[ix]))
            .collect();
        self.prune_stale_paths();
    }

    fn prune_stale_paths(&mut self) {
        self.selection.retain(|p| self.row_of_path.contains_key(p));
        self.anchor.retain(|p| self.row_of_path.contains_key(p));
        self.cursor.retain(|p| self.row_of_path.contains_key(p));
    }

    fn is_visible(&self, e: &Entry) -> bool {
        if !self.show_hidden && e.is_hidden() {
            return false;
        }
        if let Some(f) = &self.filter {
            return e.name.to_lowercase().contains(&f.to_lowercase());
        }
        true
    }

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

        self.entries.clear();
        self.order.clear();
        self.row_of_path.clear();
        self.error = None;
        self.loading = true;
        // Keep the selection paths: a watcher reload must preserve them even
        // though rows will be rebuilt.
        self.watch = None;
        cx.notify();

        let fs = cx.global::<crate::state::FerretState>().fs;
        let dir = self.dir.clone();

        cx.spawn(async move |this, cx| {
            eprintln!("[folder] stream task started for {dir}");
            let mut stream = fs.read_dir(&dir);
            let mut buffer: Vec<Entry> = Vec::new();
            let mut first_error: Option<VfsError> = None;

            let mut count = 0usize;
            while let Some(item) = stream.next().await {
                count += 1;
                match item {
                    Ok(entry) => buffer.push(entry),
                    Err(err) => {
                        // A failed open ends the stream; a bad child does not.
                        first_error = Some(err);
                        break;
                    }
                }
                if buffer.len() >= BATCH_SIZE {
                    let batch = std::mem::take(&mut buffer);
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
            }

            eprintln!("[folder] stream ended, saw {count} items, err={first_error:?}");
            let _ = this.update(cx, |folder, cx| {
                if folder.generation != generation {
                    return;
                }
                if !buffer.is_empty() {
                    folder.apply_batch(buffer, cx);
                }
                folder.finish_loading(first_error, cx);
            });
        })
        .detach();
    }

    fn apply_batch(&mut self, batch: Vec<Entry>, cx: &mut Context<Self>) {
        for entry in batch {
            self.row_of_path
                .insert(entry.path.clone(), self.entries.len());
            self.entries.push(entry);
        }
        // Arrival order *is* display order while loading; `order` is a plain
        // 0..n map until the sort lands.
        self.order = (0..self.entries.len())
            .filter(|&ix| self.is_visible(&self.entries[ix]))
            .collect();
        cx.emit(FolderEvent::RowsChanged);
        cx.notify();
    }

    fn finish_loading(&mut self, error: Option<VfsError>, cx: &mut Context<Self>) {
        self.loading = false;
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
        self.start_metadata_backfill(cx);
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
            while let Ok(_event) = rx.recv().await {
                // Debounce bursts: editors rewrite files several times in a
                // few tens of milliseconds.
                cx.background_executor()
                    .timer(ferret_vfs::watch::DEBOUNCE)
                    .await;
                // Drain whatever else piled up behind the debounce.
                while let Ok(more) = rx.try_recv() {
                    if more == WatchEvent::Gone {
                        let _ = this.update(cx, |folder, cx| {
                            if folder.generation == generation {
                                folder.mark_gone(cx);
                            }
                        });
                        return;
                    }
                }

                let ok = this.update(cx, |folder, cx| {
                    if folder.generation != generation {
                        return false;
                    }
                    folder.reload_preserving_state(cx);
                    true
                });
                if ok.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn mark_gone(&mut self, cx: &mut Context<Self>) {
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

    fn start_metadata_backfill(&mut self, cx: &mut Context<Self>) {
        let fs = cx.global::<crate::state::FerretState>().fs;
        if fs.capabilities().metadata_in_listing {
            return; // Windows filled it during the walk.
        }

        let generation = self.generation;
        let targets: Vec<VfsPath> = self
            .entries
            .iter()
            .filter(|e| e.metadata.is_none())
            .map(|e| e.path.clone())
            .collect();

        cx.spawn(async move |this, cx| {
            const CHUNK: usize = 256;
            for chunk in targets.chunks(CHUNK) {
                let mut filled: Vec<(VfsPath, ferret_vfs::Metadata)> =
                    Vec::with_capacity(chunk.len());
                for path in chunk {
                    if let Ok(md) = fs.metadata(path).await {
                        filled.push((path.clone(), md));
                    }
                }
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
        for (path, md) in filled {
            if let Some(entry) = self
                .row_of_path
                .get(&path)
                .and_then(|ix| self.entries.get_mut(*ix))
            {
                entry.metadata = Some(md);
            }
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

trait OptionExt<T> {
    fn retain(&mut self, f: impl Fn(&T) -> bool);
}

impl<T> OptionExt<T> for Option<T> {
    fn retain(&mut self, f: impl Fn(&T) -> bool) {
        if self.as_ref().is_some_and(|v| !f(v)) {
            *self = None;
        }
    }
}

impl EventEmitter<FolderEvent> for Folder {}

/// Directories sort above files regardless of the sort key
/// (`settings.browser.directories_first`).
fn cmp_entries(a: &Entry, b: &Entry, key: SortKey, order: SortOrder) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a.is_dir_like(), b.is_dir_like()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    let ord = match key {
        SortKey::Name => crate::format::natural_compare(&a.name, &b.name),
        SortKey::Modified => {
            let av = a
                .metadata
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let bv = b
                .metadata
                .and_then(|m| m.modified)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            av.cmp(&bv)
        }
        SortKey::Size => a
            .display_len()
            .unwrap_or(0)
            .cmp(&b.display_len().unwrap_or(0)),
        // Kind: group by extension, then by name.
        SortKey::Kind => a
            .path
            .extension()
            .cmp(&b.path.extension())
            .then_with(|| crate::format::natural_compare(&a.name, &b.name)),
    };

    let ord = match order {
        SortOrder::Ascending => ord,
        SortOrder::Descending => ord.reverse(),
    };

    // Stable, sensible tiebreak so equal keys keep names findable.
    ord.then_with(|| crate::format::natural_compare(&a.name, &b.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_vfs::{EntryKind, Metadata, SymlinkTarget};

    fn entry(name: &str, kind: EntryKind, len: u64, modified: Option<SystemTime>) -> Entry {
        Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind,
            metadata: Some(Metadata {
                len,
                modified,
                // Mirror what LocalFs produces: the leading dot is hidden.
                hidden: name.starts_with('.'),
                ..Default::default()
            }),
        }
    }

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn populated() -> Folder {
        let mut f = Folder::new(
            VfsPath::local("/x"),
            SortKey::Name,
            SortOrder::Ascending,
            false,
        );
        f.entries = vec![
            entry("report.pdf", EntryKind::File, 500, t(300)),
            entry("zebra.txt", EntryKind::File, 100, t(200)),
            entry("src", EntryKind::Dir, 0, t(100)),
            entry(".hidden", EntryKind::File, 10, t(400)),
        ];
        // Tests skip the streaming phase; sort as `finish_loading` would.
        f.loading = false;
        f.resort();
        f
    }

    #[test]
    fn hidden_entries_are_filtered_until_shown() {
        let f = populated();
        assert_eq!(f.row_count(), 3);
        assert_eq!(f.entry_at(0).unwrap().name, "src");

        let mut f = populated();
        f.set_show_hidden(true);
        assert_eq!(f.row_count(), 4);
    }

    #[test]
    fn folders_come_first_then_natural_name_order() {
        let mut f = populated();
        f.set_show_hidden(true);
        let names: Vec<_> = (0..f.row_count())
            .map(|r| f.entry_at(r).unwrap().name.clone())
            .collect();
        assert_eq!(names, ["src", ".hidden", "report.pdf", "zebra.txt"]);
    }

    #[test]
    fn size_sort_uses_display_len_and_reverses() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.set_sort(SortKey::Size, SortOrder::Ascending);
        assert_eq!(f.entry_at(0).unwrap().name, "src"); // dirs pinned first
        assert_eq!(
            f.entry_at(1).unwrap().display_len(),
            Some(10),
            "smallest file first"
        );

        f.set_sort(SortKey::Size, SortOrder::Descending);
        assert_eq!(f.entry_at(1).unwrap().display_len(), Some(500));
    }

    #[test]
    fn modified_sort_orders_by_time() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.set_sort(SortKey::Modified, SortOrder::Ascending);
        assert_eq!(f.entry_at(0).unwrap().name, "src");
        assert_eq!(f.entry_at(1).unwrap().name, "zebra.txt");
        assert_eq!(f.entry_at(f.row_count() - 1).unwrap().name, ".hidden");
    }

    #[test]
    fn selection_survives_resorting() {
        let mut f = populated();
        f.set_show_hidden(true);
        // Select "zebra" (last row).
        f.select_only(f.row_count() - 1);
        assert_eq!(f.selected_paths().len(), 1);

        // Flip the sort; zebra moves but stays selected.
        f.set_sort(SortKey::Name, SortOrder::Descending);
        let sel = &f.selected_paths()[0];
        assert!(sel.to_string().ends_with("zebra.txt"));
    }

    #[test]
    fn shift_click_extends_from_anchor() {
        let mut f = populated();
        f.set_show_hidden(true);
        f.select_only(1); // .hidden
        f.extend_select(3); // report.pdf
        assert_eq!(f.selection_count(), 3);
        assert_eq!(f.selection_size(), 10 + 100 + 500);

        // Extend backwards past the anchor collapses onto the new range.
        f.extend_select(2);
        assert_eq!(f.selection_count(), 2);
    }

    #[test]
    fn cmd_click_toggles_without_disturbing_others() {
        let mut f = populated();
        f.select_only(0);
        f.toggle_select(2);
        assert_eq!(f.selection_count(), 2);
        f.toggle_select(2);
        assert_eq!(f.selection_count(), 1);
    }

    #[test]
    fn filter_narrows_and_reports_misses() {
        let mut f = populated();
        f.set_filter(Some("PDF".into()));
        assert_eq!(f.row_count(), 1);
        assert_eq!(f.entry_at(0).unwrap().name, "report.pdf");

        f.set_filter(Some("zzz".into()));
        assert_eq!(f.row_count(), 0);
        // And clearing restores.
        f.set_filter(None);
        assert_eq!(f.row_count(), 3);
    }

    #[test]
    fn type_ahead_finds_wrapping_matches() {
        let mut f = populated();
        f.set_show_hidden(true);
        // Visible order: src, .hidden, report.pdf, zebra.txt.
        assert!(f.select_by_prefix("rep", 0));
        assert_eq!(f.cursor_row(), Some(2));

        // From after the match it wraps around.
        assert!(f.select_by_prefix("z", 3));
        assert_eq!(f.cursor_row(), Some(3));

        assert!(!f.select_by_prefix("qq", 0));
    }

    #[test]
    fn selecting_out_of_range_is_harmless() {
        let mut f = populated();
        f.select_only(99);
        assert!(f.selection.is_empty());
        assert!(f.cursor.is_none());
    }

    #[test]
    fn symlinked_directories_sort_as_directories() {
        let mut f = Folder::new(
            VfsPath::local("/x"),
            SortKey::Name,
            SortOrder::Ascending,
            true,
        );
        f.entries = vec![
            entry("b.txt", EntryKind::File, 1, None),
            entry(
                "a-link",
                EntryKind::Symlink {
                    target_kind: Some(SymlinkTarget::Dir),
                },
                0,
                None,
            ),
        ];
        f.loading = false;
        f.resort();
        assert_eq!(f.entry_at(0).unwrap().name, "a-link");
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use gpui::AppContext as _;
    use std::fs;

    #[gpui::test]
    async fn streams_a_listing_into_rows(cx: &mut gpui::TestAppContext) {
        let root = std::env::temp_dir().join(format!(
            "ferret-ui-stream-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        for i in 0..50 {
            fs::write(root.join(format!("file-{i:03}.txt")), b"x").unwrap();
        }
        fs::create_dir(root.join("subdir")).unwrap();

        cx.update(|cx| {
            cx.set_global(crate::state::load_state());
        });

        let folder = cx.new(|_| {
            Folder::new(
                VfsPath::local(root.clone()),
                ferret_settings::SortKey::Name,
                ferret_settings::SortOrder::Ascending,
                true,
            )
        });
        folder.update(cx, |f, cx| f.start_loading(cx));

        // The listing producer runs on its own OS thread (smol::unblock), so
        // this is not a fully deterministic test: allow real threads, then
        // wait for the walk to land.
        cx.executor().allow_parking();
        let mut rows = 0;
        let mut loading = true;
        for _ in 0..600 {
            cx.run_until_parked();
            (rows, loading) = folder.read_with(cx, |f, _| (f.row_count(), f.is_loading()));
            if rows == 51 && !loading {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(rows, 51, "all entries must land");
        assert!(!loading, "loading must finish");

        // Dirs first, then natural name order.
        let first = folder.read_with(cx, |f, _| f.entry_at(0).unwrap().name.clone());
        assert_eq!(first, "subdir");

        fs::remove_dir_all(&root).ok();
    }
}
