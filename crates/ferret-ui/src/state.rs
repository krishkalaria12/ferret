//! The application-wide state bag, registered as a GPUI [`Global`].
//!
//! One place for the things every view needs and nothing else does: the
//! parsed settings, the folder-preference store, the storage layer, the
//! platform integrations and the icon table. Views read it through
//! `cx.global::<FerretState>()`; writes go through the small mutation helpers
//! so saving stays consistent.

use std::path::PathBuf;
use std::sync::Arc;

use ferret_platform::Platform;
use ferret_remote::ConnectionStore;
use ferret_settings::{FolderPrefs, FolderView, Settings};
use ferret_vfs::{Router, VfsPath};

use crate::icons::{FileIcons, RenderedIconCache};

/// Everything global, in one struct.
pub struct FerretState {
    pub settings: Settings,
    pub prefs: FolderPrefs,
    /// Saved SSH endpoints. Secrets are referenced by id and remain in the OS
    /// keychain; this store contains only non-secret connection metadata.
    pub connections: ConnectionStore,
    /// Which of those are connected, and the live sessions holding them open.
    ///
    /// Here rather than on a view because dropping a session closes the SSH
    /// connection: a mount has to outlive the tab that opened it.
    pub mounts: crate::mounts::Mounts,
    /// Local disk plus whatever is mounted. Cloned into every task that lists,
    /// watches, probes or mutates; the clone is a refcount bump, and local
    /// paths still go straight to `LocalFs` inside it.
    pub fs: Router,
    pub platform: Arc<dyn Platform>,
    /// The bundled fallback set: always answers, never blocks (D-18).
    pub icons: &'static FileIcons,
    /// Real Finder/Explorer icons, when the platform can supply them. Layers
    /// over `icons` and wins once resolved.
    pub os_icons: Arc<dyn ferret_platform::IconProvider>,
    /// Decoded GPUI images keyed by icon type. This is distinct from the
    /// platform cache: it preserves texture identity across frames and keeps
    /// byte-to-image conversion out of the steady-state render path.
    pub rendered_os_icons: RenderedIconCache,
    /// Whether `settings` has changed since it last reached disk.
    ///
    /// `FolderPrefs` tracks its own dirtiness; the settings file has no
    /// equivalent, and the debounced writer needs somewhere to ask before it
    /// rewrites a file nothing touched.
    settings_dirty: bool,
    /// The pending debounced write. Held here rather than on a view because
    /// settings are mutated from context-menu closures that have an [`App`]
    /// and no entity to hang a task off. Replacing it drops the previous
    /// timer, which is what collapses a burst into one write.
    ///
    /// [`App`]: gpui::App
    save_task: Option<gpui::Task<()>>,
    /// Payloads serialised but not yet on disk.
    ///
    /// Shared, because the debounced writer runs on the background executor
    /// while the quit flush runs on the main thread and the two must see the
    /// same set. See [`OutstandingWrites`].
    outstanding: Arc<OutstandingWrites>,
}

/// Config that has been serialised but has not reached disk yet.
///
/// Clearing the dirty flags at serialisation time is what stops a read-only
/// config directory from re-serialising the same bytes on every debounce, but
/// on its own it makes the payload invisible: quitting in the window between
/// `take_pending_saves` and the write completing would find nothing to flush
/// and exit with the user's last change still in memory. Holding it here keeps
/// it findable until it actually lands.
#[derive(Default)]
pub(crate) struct OutstandingWrites {
    /// Keyed by path, so a newer serialisation *replaces* an older one that
    /// has not been written yet instead of racing it to the same file.
    payloads: parking_lot::Mutex<std::collections::HashMap<PathBuf, String>>,
    /// Held across the file writes themselves.
    ///
    /// Taking a batch and writing it has to be atomic against another drain,
    /// or a quit-time write of new contents can be overtaken by an older
    /// background write that was already in flight — leaving the stale
    /// version on disk. This is also what makes the quit flush *wait* for an
    /// in-flight background write rather than racing it.
    writing: parking_lot::Mutex<()>,
}

impl OutstandingWrites {
    fn queue(&self, payloads: impl IntoIterator<Item = (PathBuf, String)>) {
        let mut pending = self.payloads.lock();
        pending.extend(payloads);
    }

    /// Write everything outstanding, blocking the calling thread.
    fn drain_and_write(&self) {
        let _writing = self.writing.lock();
        loop {
            let batch = std::mem::take(&mut *self.payloads.lock());
            if batch.is_empty() {
                return;
            }
            // A failed write drops its payload rather than retrying forever:
            // `write_config` reports the reason, and a config directory that
            // cannot be written will not start working on the next debounce.
            for (path, contents) in batch {
                write_config(&path, &contents);
            }
        }
    }
}

impl gpui::Global for FerretState {}

/// Load settings and preferences from disk. A missing or broken file costs
/// nothing: `Settings::from_json` degrades per section and reports why.
pub fn load_state() -> FerretState {
    let settings = match ferret_settings::settings_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
    {
        Some(text) => {
            let (settings, warnings) = Settings::from_json(&text);
            for warning in &warnings {
                eprintln!(
                    "settings: {} {} {}",
                    warning.key, warning.problem, warning.resolution
                );
            }
            settings
        }
        None => Settings::default(),
    };

    let prefs = ferret_settings::folder_prefs_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|text| FolderPrefs::from_json(&text))
        .unwrap_or_default();

    let (connections, connection_warning) = ConnectionStore::load();
    if let Some(warning) = connection_warning {
        eprintln!("connections: {warning}");
    }

    FerretState {
        settings,
        prefs,
        connections,
        mounts: crate::mounts::Mounts::default(),
        fs: Router::new(),
        platform: ferret_platform::platform(),
        icons: FileIcons::embedded(),
        os_icons: ferret_platform::icon_provider(),
        rendered_os_icons: RenderedIconCache::default(),
        settings_dirty: false,
        save_task: None,
        outstanding: Arc::default(),
    }
}

/// Arm the debounced write of settings and folder preferences (A-09).
///
/// Every call restarts the timer, so a run of sort clicks — or a column drag,
/// which fires per frame — collapses into one write. The write itself never
/// touches the window thread: once the timer fires the payloads are serialised
/// and handed to the background executor. Quitting flushes whatever is still
/// pending; see [`crate::flush_pending_saves`].
pub(crate) fn arm_config_save(cx: &mut gpui::App) {
    let task = cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(ferret_settings::config::SAVE_DEBOUNCE)
            .await;
        // Serialised and queued in one step, so the payload is never
        // unaccounted for: the dirty flags are cleared here, and from this
        // point the quit flush finds the work in `outstanding` instead.
        let Some(outstanding) = cx.update(|cx| {
            let state = cx.global_mut::<FerretState>();
            let pending = state.take_pending_saves();
            if pending.is_empty() {
                return None; // Nothing changed since the last write.
            }
            state.outstanding.queue(pending);
            Some(Arc::clone(&state.outstanding))
        }) else {
            return;
        };
        cx.background_executor()
            .spawn(async move { outstanding.drain_and_write() })
            .detach();
    });
    cx.global_mut::<FerretState>().save_task = Some(task);
}

impl FerretState {
    /// Record that `settings` has changed and owes the disk a write.
    ///
    /// Every toggle that mutates `settings` calls this and then arms the
    /// browser's debounce; nothing writes from an action handler.
    pub fn touch_settings(&mut self) {
        self.settings_dirty = true;
    }

    /// Serialise whatever config is outstanding and mark it saved.
    ///
    /// Returns the payloads instead of writing them: serialising is a
    /// millisecond and the write is a `create_dir_all`, a write and a rename,
    /// so the caller hands these to the background executor. An empty result
    /// is the common one — it is what lets an idle app write nothing at all
    /// (`FolderPrefs::is_dirty`).
    ///
    /// Marking saved here rather than after the write is deliberate: a failed
    /// write already reports itself, and holding the flag would make the next
    /// debounce re-serialise the same bytes forever on a read-only config
    /// directory. What makes that safe is that every caller hands the result
    /// straight to [`OutstandingWrites::queue`] — the work stays visible to
    /// the quit flush even though the flags no longer show it.
    pub fn take_pending_saves(&mut self) -> Vec<(PathBuf, String)> {
        let mut pending = Vec::new();
        if self.settings_dirty
            && let Ok(path) = ferret_settings::settings_path()
        {
            pending.push((path, self.settings.to_json()));
            self.settings_dirty = false;
        }
        if self.prefs.is_dirty()
            && let Ok(path) = ferret_settings::folder_prefs_path()
        {
            pending.push((path, self.prefs.to_json()));
            self.prefs.mark_saved();
        }
        if let Some(save) = self.connections.take_pending_save() {
            pending.push(save);
        }
        pending
    }

    /// Write everything outstanding on the calling thread.
    ///
    /// The one place a config write is allowed to block: quitting, where
    /// there is no next frame to protect and a debounce that has not fired
    /// yet would otherwise lose the user's last sort click.
    ///
    /// This covers three cases, and needs to cover all three: a debounce that
    /// never fired (still dirty), one that fired and whose write is still in
    /// flight (queued, and `drain_and_write` blocks until it lands), and one
    /// that fired and finished (nothing to do).
    pub fn flush_pending_saves(&mut self) {
        let pending = self.take_pending_saves();
        self.outstanding.queue(pending);
        self.outstanding.drain_and_write();
    }

    /// The remembered view for a folder, falling back to browser defaults.
    pub fn folder_view(&mut self, dir: &VfsPath) -> FolderView {
        if !self.settings.browser.remember_folder_views {
            return default_folder_view(&self.settings);
        }
        self.prefs
            .get(&dir.preference_key())
            .unwrap_or_else(|| default_folder_view(&self.settings))
    }

    pub fn remember_folder_view(&mut self, dir: &VfsPath, view: FolderView) {
        if !self.settings.browser.remember_folder_views {
            return;
        }
        self.prefs.set(dir.preference_key(), view);
    }

    pub fn home_dir(&self) -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Write `contents` to `path` via a temp file in the same directory.
///
/// The config directory is created first: on a fresh machine nothing else
/// makes it, and without it every save fails — silently, because the write
/// goes to a path whose parent does not exist. Returns whether it landed.
///
/// This blocks. Callers reach it through [`FerretState::take_pending_saves`]
/// on the background executor, or through
/// [`FerretState::flush_pending_saves`] at quit; nothing calls it from a
/// frame.
pub(crate) fn write_config(path: &std::path::Path, contents: &str) -> bool {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        eprintln!("config: can't create {}: {err}", parent.display());
        return false;
    }

    // Same directory as the target, so the rename stays on one filesystem
    // and is therefore atomic.
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = std::fs::write(&tmp, contents) {
        eprintln!("config: can't write {}: {err}", tmp.display());
        return false;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        eprintln!("config: can't replace {}: {err}", path.display());
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

fn default_folder_view(settings: &Settings) -> FolderView {
    FolderView {
        mode: settings.browser.default_view,
        sort_key: settings.browser.sort_key,
        sort_order: settings.browser.sort_order,
        hidden_columns: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_creates_the_config_directory_it_needs() {
        let root = std::env::temp_dir().join(format!(
            "ferret-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Two levels deep and absent: the fresh-machine case, where a plain
        // write would fail and take the setting with it.
        let path = root.join("Ferret").join("settings.json");
        assert!(!path.exists());

        assert!(write_config(&path, "{\"a\":1}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");

        // Rewriting replaces the contents and leaves no temp file behind.
        assert!(write_config(&path, "{\"a\":2}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_idle_app_has_nothing_to_write() {
        let mut state = load_state();
        // Whatever the machine's real config looked like, loading it must not
        // by itself owe the disk a write — this is the check that keeps the
        // debounce from rewriting both files every two seconds forever.
        state.take_pending_saves();
        assert!(state.take_pending_saves().is_empty());

        // A settings change is one payload, and only one: `FolderPrefs` is
        // untouched, so its file is not rewritten alongside.
        state.touch_settings();
        let pending = state.take_pending_saves();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(
            pending[0]
                .0
                .ends_with(ferret_settings::config::SETTINGS_FILE)
        );

        // And taking it clears it: two debounces in a row write once.
        assert!(state.take_pending_saves().is_empty());
    }

    #[test]
    fn a_remembered_folder_view_owes_the_disk_a_write() {
        let mut state = load_state();
        state.take_pending_saves();

        state.settings.browser.remember_folder_views = true;
        state.remember_folder_view(
            &VfsPath::local("/x/ferret-state-test"),
            FolderView {
                mode: ferret_settings::ViewMode::Icons,
                ..Default::default()
            },
        );

        let pending = state.take_pending_saves();
        assert_eq!(pending.len(), 1, "{pending:?}");
        assert!(
            pending[0]
                .0
                .ends_with(ferret_settings::config::FOLDER_PREFS_FILE)
        );
        // Compact, not pretty: this file is written by the app, not read by
        // hand, and at capacity the formatting doubles it.
        assert!(!pending[0].1.contains('\n'), "{}", pending[0].1);
    }
}
