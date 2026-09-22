//! The git panel's state, its refresh scheduling, and the four operations
//! (F-50, F-53). Rendering lives in `git_bar.rs`; the dialog in
//! `commit_dialog.rs`.
//!
//! Nothing here spawns git on the window thread. Every call into `ferret-git`
//! blocks, so it goes to the background executor and comes back through a
//! generation check — the same rule `Folder` uses for a listing that the user
//! has already navigated away from.

use std::sync::Arc;

use arc_swap::ArcSwap;
use ferret_git::{FileStatus, GitError, RepoStatus, Repository};

use super::*;

/// Shared with every [`FilePane`] the browser builds.
///
/// `ArcSwap` rather than a mutex, for the same reason `ColumnLayout` uses one:
/// this is read once per cell per frame and written once per refresh, so a read
/// must be an atomic load and a write may take as long as it likes.
pub(crate) type GitHandle = Arc<ArcSwap<GitSnapshot>>;

/// What one refresh produced, in the shape the listing and the status bar both
/// read it in.
#[derive(Default)]
pub(crate) struct GitSnapshot {
    /// `None` when the folder is not in a repository, or the status could not
    /// be read. Either way the segment is absent and no row carries a badge.
    status: Option<RepoStatus>,
    /// The wholly-untracked directory the browsed folder sits inside, if it
    /// does. git collapses such a directory to a single entry and says nothing
    /// about anything within it, so the listing has to (`docs/git.html` §7,
    /// rule 4).
    ///
    /// Resolved once here, when the snapshot is built, rather than per row: the
    /// answer is the same for every row in one listing, and a per-row ancestor
    /// walk is exactly the O(the directory) work Gate 7 forbids.
    ///
    /// The path is kept rather than a bare flag because the rows a snapshot is
    /// asked about are **not** all in the browsed folder. Column view draws
    /// every ancestor column at once, so browsing `repo/vendor/lib` under an
    /// untracked `vendor/` also renders `repo`'s own listing — where the
    /// tracked, clean siblings of `vendor` would inherit the fallback and be
    /// badged as untracked.
    untracked_root: Option<PathBuf>,
}

impl GitSnapshot {
    /// The badge for one row. One hash of a borrowed path, no allocation.
    pub(crate) fn status_for(&self, path: &Path, is_dir: bool) -> Option<FileStatus> {
        let status = self.status.as_ref()?;
        status.index.status_for(path, is_dir).or_else(|| {
            // `starts_with` is component-wise and borrows both sides, so the
            // narrowing costs no allocation.
            let root = self.untracked_root.as_deref()?;
            path.starts_with(root).then_some(FileStatus::Untracked)
        })
    }

    /// What the status bar reads. `None` outside a repository, which is what
    /// makes the whole segment absent rather than empty.
    pub(super) fn status(&self) -> Option<&RepoStatus> {
        self.status.as_ref()
    }

    /// Build one directly, for the allocations-per-frame budget.
    ///
    /// The budget has to measure the badge lookup against a *populated* index:
    /// an empty snapshot returns on the first `?` before it ever hashes a path,
    /// so a version of this that allocated per row would sail through.
    #[cfg(all(test, feature = "dhat-heap"))]
    pub(crate) fn from_status(status: RepoStatus, untracked_root: Option<PathBuf>) -> Self {
        Self {
            status: Some(status),
            untracked_root,
        }
    }
}

/// Which mutation is in flight. At most one per window (D-50): two concurrent
/// operations on one index is how a repository ends up with a stale
/// `index.lock` and a user who has to delete a file to get their tools working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GitAction {
    Commit,
    Pull,
    Push,
}

impl GitAction {
    pub(super) fn present(self) -> &'static str {
        match self {
            GitAction::Commit => "Committing…",
            GitAction::Pull => "Pulling…",
            GitAction::Push => "Pushing…",
        }
    }
}

pub(super) struct GitPanel {
    snapshot: GitHandle,
    repo: Option<Arc<Repository>>,
    /// Bumped whenever the repository being followed changes. Every background
    /// result is checked against it, so a slow status for a folder the user has
    /// left is dropped rather than shown.
    generation: u64,
    /// The debounce timer. Replacing it drops the previous one, which is what
    /// collapses a watcher burst into one status read (D-48).
    refresh: Option<gpui::Task<()>>,
    /// True while a status process is running.
    reading: bool,
    /// A refresh was requested while one was running. Re-run once, rather than
    /// starting a second process.
    rerun: bool,
    /// The `.git` watcher and the task pumping it. Held so both stop when the
    /// repository changes or the window closes.
    watch: Option<ferret_vfs::Watch>,
    watch_task: Option<gpui::Task<()>>,
    action: Option<GitAction>,
    /// git could not be started. Asked once and then never again (D-43): a
    /// missing git means the feature is absent, not broken.
    unavailable: bool,
    /// Why the last status read failed, for the chip's tooltip.
    error: Option<String>,
}

impl GitPanel {
    pub(super) fn new() -> Self {
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(GitSnapshot::default())),
            repo: None,
            generation: 0,
            refresh: None,
            reading: false,
            rerun: false,
            watch: None,
            watch_task: None,
            action: None,
            unavailable: false,
            error: None,
        }
    }

    pub(super) fn handle(&self) -> GitHandle {
        self.snapshot.clone()
    }

    pub(super) fn snapshot(&self) -> arc_swap::Guard<Arc<GitSnapshot>> {
        self.snapshot.load()
    }

    pub(super) fn repo(&self) -> Option<&Repository> {
        self.repo.as_deref()
    }

    pub(super) fn busy(&self) -> Option<GitAction> {
        self.action
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Forget everything about the repository being followed.
    fn clear(&mut self) {
        self.generation += 1;
        self.repo = None;
        self.watch = None;
        self.watch_task = None;
        self.refresh = None;
        self.reading = false;
        self.rerun = false;
        self.error = None;
        self.snapshot.store(Arc::new(GitSnapshot::default()));
    }
}

impl Browser {
    /// Point the panel at `dir` (navigation, or a tab becoming active).
    ///
    /// Staying inside the same work tree keeps the snapshot and queues a
    /// refresh — the branch has not changed, and clearing it would make every
    /// step down a tree flash the segment out and back. Leaving it clears
    /// first, because a branch name shown for a folder it does not describe is
    /// worse than no branch name at all.
    pub(super) fn git_follow(&mut self, dir: &VfsPath, cx: &mut Context<Self>) {
        if self.git.unavailable {
            return;
        }
        if let (Some(repo), Some(local)) = (self.git.repo.clone(), dir.as_local())
            && repo.contains(local)
        {
            self.request_git_refresh(cx);
            return;
        }

        self.git.clear();
        cx.notify();

        let generation = self.git.generation;
        let dir = dir.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { ferret_git::discover(&dir) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation != generation {
                    return;
                }
                match found {
                    Ok(Some(repo)) => {
                        this.git.repo = Some(Arc::new(repo));
                        this.attach_git_watch(cx);
                        this.request_git_refresh(cx);
                    }
                    // The ordinary answer: not a repository, and not a problem.
                    Ok(None) => {}
                    Err(GitError::NotInstalled) => this.git.unavailable = true,
                    Err(err) => this.git.error = Some(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read the status whenever the window comes back to the front.
    ///
    /// **The watchers do not cover the working tree, and cannot.** Both of them
    /// — the active folder's, and the one on the resolved git directory — are
    /// `RecursiveMode::NonRecursive`, so an edit to `src/main.rs` made by
    /// another program while Ferret is showing the repository *root* changes
    /// neither watched directory: the root watcher does not see a write nested
    /// below it, and `.git` is untouched until something stages or commits. The
    /// count and every rolled-up badge then stay stale until the user happens to
    /// navigate.
    ///
    /// The obvious repair is a recursive watch on the work tree, and it is the
    /// wrong one: it means a descriptor per directory over `node_modules`,
    /// `target` and every other tree git is going to ignore anyway, on a feature
    /// whose whole design (D-48, "no timer") is that an idle window costs
    /// nothing.
    ///
    /// Activation is the cheap answer, because it is exactly the moment the
    /// staleness starts to matter. Editing elsewhere and coming back *is* the
    /// scenario, and the user cannot see a stale badge in a window that is not
    /// in front of them. One debounced read per return to the window, and
    /// nothing at all while it sits unfocused.
    pub(super) fn observe_git_activation(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.observe_window_activation(window, |this, window, cx| {
            // Fires on deactivation too; leaving is not a reason to read.
            if window.is_window_active() {
                this.request_git_refresh(cx);
            }
        })
    }

    /// Ask for a status read, eventually.
    ///
    /// Every trigger in `docs/git.html` §5 lands here, and the debounce is what
    /// turns a `git add -A`'s dozen watcher events into one process.
    pub(super) fn request_git_refresh(&mut self, cx: &mut Context<Self>) {
        if self.git.repo.is_none() || self.git.unavailable {
            return;
        }
        if self.git.reading {
            // Re-run once when the current read finishes, rather than starting
            // a second process against the same index.
            self.git.rerun = true;
            return;
        }

        let generation = self.git.generation;
        self.git.refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(ferret_git::config::REFRESH_DEBOUNCE)
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.git.generation == generation {
                    this.read_git_status(cx);
                }
            });
        }));
    }

    fn read_git_status(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.git.repo.clone() else {
            return;
        };
        let dir = self.active().folder.read(cx).dir.clone();
        let generation = self.git.generation;
        self.git.reading = true;
        self.git.rerun = false;

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ferret_git::status(&repo) })
                .await;

            let _ = this.update(cx, |this, cx| {
                // The generation check comes first, and `reading` is cleared
                // only inside it. A slow read for repository A landing after
                // the user has navigated to B would otherwise clear *B's*
                // guard, and the next watcher event would start a second
                // status process against B while the first was still running —
                // the single-flight guard defeated, and two snapshots free to
                // arrive out of order. `GitPanel::clear` has already cleared
                // the flag for the repository this result belongs to.
                if this.git.generation != generation {
                    return;
                }
                this.git.reading = false;
                match result {
                    Ok(status) => {
                        let untracked_root = dir
                            .as_local()
                            .and_then(|dir| status.index.untracked_root_for(dir))
                            .map(Path::to_path_buf);
                        this.git.error = None;
                        this.git.snapshot.store(Arc::new(GitSnapshot {
                            status: Some(status),
                            untracked_root,
                        }));
                    }
                    Err(GitError::NotInstalled) => {
                        this.git.unavailable = true;
                        this.git.clear();
                    }
                    // A failed read leaves the previous snapshot in place:
                    // stale badges are closer to the truth than none, and the
                    // tooltip says the read failed.
                    Err(err) => this.git.error = Some(err.to_string()),
                }
                if this.git.rerun {
                    this.read_git_status(cx);
                }
                this.repaint_git(cx);
            });
        })
        .detach();
    }

    /// One non-recursive watch on the resolved git directory.
    ///
    /// `index`, `HEAD`, `FETCH_HEAD`, `MERGE_HEAD` and `packed-refs` are all
    /// direct children of it, so committing, checking out, fetching or staging
    /// — from Ferret, from the terminal panel below, or from another
    /// application — all arrive here. That is what lets this feature run with
    /// no timer at all.
    fn attach_git_watch(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.git.repo.clone() else {
            return;
        };
        let fs = cx.global::<FerretState>().fs.clone();
        let git_dir = VfsPath::local(repo.git_dir().to_path_buf());
        let Ok(watch) = fs.watch(&git_dir) else {
            // No notification is a degraded mode, not a failure: navigating,
            // acting and ⌘R all still refresh.
            return;
        };
        let events = watch.events.clone();
        self.git.watch = Some(watch);

        let generation = self.git.generation;
        self.git.watch_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(_event) = events.recv().await {
                // Every event kind means the same thing here — something in
                // the repository moved — and the read that follows is a full
                // one regardless, so they are not distinguished.
                let carry_on = this.update(cx, |this, cx| {
                    if this.git.generation != generation {
                        return false;
                    }
                    this.request_git_refresh(cx);
                    true
                });
                if !matches!(carry_on, Ok(true)) {
                    return;
                }
            }
        }));
    }

    /// Repaint what the snapshot feeds: the status bar, and the active pane's
    /// rows.
    ///
    /// The pane is a separate entity, so notifying the browser alone leaves the
    /// listing drawing last refresh's badges until something else happens to
    /// touch it.
    fn repaint_git(&mut self, cx: &mut Context<Self>) {
        self.active()
            .pane
            .update(cx, |pane, cx| pane.repaint_rows(cx));
        cx.notify();
    }

    // ------------------------------------------------------------------
    // Operations (F-53)

    pub(super) fn git_pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git_action(GitAction::Pull, None, window, cx);
    }

    pub(super) fn git_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git_action(GitAction::Push, None, window, cx);
    }

    /// Stage everything (optionally) and commit. Called by the dialog.
    pub(super) fn git_commit(
        &mut self,
        message: String,
        stage_all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_git_action(GitAction::Commit, Some((message, stage_all)), window, cx);
    }

    /// The one path every mutation takes, so the single-flight guard, the
    /// notification policy and the follow-up refresh cannot be forgotten by one
    /// of them.
    fn run_git_action(
        &mut self,
        action: GitAction,
        commit: Option<(String, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.git.repo.clone() else {
            return;
        };
        if let Some(running) = self.git.action {
            window.push_notification(Notification::warning(running.present()), cx);
            return;
        }
        self.git.action = Some(action);
        cx.notify();

        let generation = self.git.generation;
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    match (action, commit) {
                        (GitAction::Pull, _) => ferret_git::pull(&repo),
                        (GitAction::Push, _) => ferret_git::push(&repo),
                        (GitAction::Commit, Some((message, stage_all))) => {
                            if stage_all {
                                ferret_git::stage_all(&repo)?;
                            }
                            ferret_git::commit(&repo, &message)
                        }
                        // Unreachable: `git_commit` is the only caller that
                        // passes `Commit`, and it always supplies a message.
                        (GitAction::Commit, None) => Ok(String::new()),
                    }
                })
                .await;

            // D-51: git's own words, whichever way it went.
            let _ = window_handle.update(cx, |_, window, cx| {
                match &outcome {
                    Ok(summary) if !summary.is_empty() => {
                        window.push_notification(Notification::success(summary.clone()), cx);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        window.push_notification(Notification::error(err.to_string()), cx);
                    }
                }
                // The dialog is still up — it stays up for the whole operation
                // (D-52) — so this is where it learns what happened. A message
                // that has been committed must not come back in the next
                // dialog; one that was *refused* must, and the dialog holding
                // it stays on screen for the user to correct.
                if action == GitAction::Commit
                    && let Some(browser) = this.upgrade()
                {
                    let committed = outcome.is_ok();
                    browser.update(cx, |browser, cx| {
                        browser.settle_commit_dialog(committed, window, cx)
                    });
                }
            });

            let _ = this.update(cx, |this, cx| {
                this.git.action = None;
                // Refreshed whichever way it went: a pull that refused
                // fast-forward may still have moved the remote-tracking ref,
                // and a commit that a hook rejected may still have staged.
                if this.git.generation == generation {
                    this.request_git_refresh(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}
