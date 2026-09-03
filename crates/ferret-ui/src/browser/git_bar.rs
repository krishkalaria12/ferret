//! The git segment at the right of the status bar (F-50, F-53).
//!
//! It is a segment, not a panel: nothing docks, nothing resizes, and the status
//! bar keeps its 24 px. Outside a repository it renders nothing at all.

use super::*;

/// Branch glyph. Not in `gpui-component`'s icon set, so it is one of Ferret's
/// own (`assets/icons/ui/`), in the same Lucide line style as the toolbar.
const BRANCH_ICON: &str = "icons/ui/git-branch.svg";

/// Type size for the segment. One step below the row font, matching the item
/// count at the other end of the bar (`ui-base.html` §4).
const GIT_FONT_SIZE: f32 = 11.5;
/// The branch chip's glyph.
const GIT_ICON_SIZE: f32 = 12.;
/// Widest the branch name may grow before it truncates. Long enough for
/// `release/2026.08-hotfix`, short enough that it cannot push the buttons off.
const BRANCH_MAX_WIDTH: f32 = 160.;
/// The change-count dot.
const COUNT_DOT_SIZE: f32 = 6.;

impl Browser {
    /// The segment, or nothing when this folder is not in a repository.
    pub(super) fn render_git_segment(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let snapshot = self.git.snapshot();
        let status = snapshot.status()?;
        let palette = crate::theme::BrowserPalette::resolve(cx);
        let theme = cx.theme();
        let busy = self.git.busy();
        let workdir = self.git.repo()?.workdir().to_path_buf();

        let mut tooltip = status.tooltip(&workdir);
        if let Some(error) = self.git.error() {
            tooltip.push_str("\n\nThe last status read failed:\n");
            tooltip.push_str(error);
        }

        let mut row = h_flex()
            .id("git-segment")
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .pr(px(8.))
            .text_size(px(GIT_FONT_SIZE))
            // The counts change under a watcher without the user doing
            // anything, so proportional figures would make the buttons beside
            // them jump sideways when 9 becomes 10 (§3).
            .font_features(crate::theme::tabular_numerals())
            .child(with_tooltip(
                h_flex()
                    .id("git-branch")
                    .items_center()
                    .gap_1()
                    .max_w(px(BRANCH_MAX_WIDTH))
                    .text_color(palette.git_branch)
                    .child(
                        Icon::default()
                            .path(BRANCH_ICON)
                            .with_size(px(GIT_ICON_SIZE)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::new(status.head.label())),
                    ),
                tooltip,
            ));

        // Counts. A clean tree says nothing rather than "0 changed".
        if !status.counts.is_clean() {
            let dot = palette.git(worst_change(status));
            row = row.child(with_tooltip(
                h_flex()
                    .id("git-count")
                    .items_center()
                    .gap_1()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .w(px(COUNT_DOT_SIZE))
                            .h(px(COUNT_DOT_SIZE))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(dot),
                    )
                    .child(SharedString::from(status.counts.changed.to_string())),
                count_tooltip(&status.counts),
            ));
        }

        // Ahead/behind. Shown only where there is an upstream to be ahead of,
        // and each half only when it is non-zero.
        if let Some(tracking) = status.tracking {
            if tracking.ahead > 0 {
                row = row.child(counter("↑", tracking.ahead, theme.muted_foreground));
            }
            if tracking.behind > 0 {
                row = row.child(counter("↓", tracking.behind, theme.muted_foreground));
            }
        }

        let can_push = can_push(status);
        let can_commit = status.counts.has_something_to_commit();
        let has_remote = status.upstream.is_some();
        // Everything is disabled while a mutation is in flight (D-50).
        let idle = busy.is_none();

        if has_remote {
            row = row.child(
                Button::new("git-pull")
                    .ghost()
                    .xsmall()
                    .label("Pull")
                    .disabled(!idle)
                    .on_click(cx.listener(|this, _, window, cx| this.git_pull(window, cx))),
            );
        }
        if can_push {
            row = row.child(
                Button::new("git-push")
                    .ghost()
                    .xsmall()
                    .label("Push")
                    .disabled(!idle)
                    .on_click(cx.listener(|this, _, window, cx| this.git_push(window, cx))),
            );
        }
        row = row.child(
            Button::new("git-commit")
                .ghost()
                .xsmall()
                // The ellipsis is the promise that this opens a dialog rather
                // than committing where you stand.
                .label("Commit…")
                .disabled(!idle || !can_commit)
                .on_click(cx.listener(|this, _, window, cx| this.open_commit_dialog(window, cx))),
        );

        // The one place the in-flight operation is named. A spinner with no
        // label is indistinguishable from a stuck window.
        if let Some(action) = busy {
            row = row.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(action.present().to_string())),
            );
        }

        Some(row.into_any_element())
    }
}

/// Whether the Push button is shown at all.
///
/// Either there is an upstream and commits it does not have, or there is no
/// upstream yet and a first push would create one.
///
/// The second arm requires a **named branch**, not merely "not unborn". A
/// detached HEAD normally has no upstream and is not unborn either, so without
/// that it renders an enabled Push whose `git push` has no current branch to
/// send and fails the moment it is clicked — a button offering to do something
/// git will refuse.
fn can_push(status: &ferret_git::RepoStatus) -> bool {
    status.tracking.is_some_and(|tracking| tracking.ahead > 0)
        || (status.upstream.is_none() && status.head.is_branch())
}

/// One `↑n` / `↓n` group.
fn counter(arrow: &'static str, value: u32, color: gpui::Hsla) -> impl IntoElement {
    h_flex()
        .items_center()
        .gap(px(2.))
        .text_color(color)
        .child(arrow)
        .child(SharedString::from(value.to_string()))
}

/// Attach a plain-text tooltip to a stateful element.
///
/// GPUI's own `tooltip` takes a builder that runs on hover, so the text has to
/// be owned by the closure; `gpui-component`'s string-shaped `tooltip` exists
/// only on its own components, not on a `div`.
fn with_tooltip(element: gpui::Stateful<gpui::Div>, text: String) -> gpui::Stateful<gpui::Div> {
    let text = SharedString::from(text);
    element.tooltip(move |window, cx| {
        gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
    })
}

/// Which colour the count's dot takes: the most urgent state present, so a
/// conflicted tree is red at a glance rather than the same amber as an ordinary
/// edit.
fn worst_change(status: &ferret_git::RepoStatus) -> ferret_git::FileStatus {
    use ferret_git::FileStatus::*;
    let counts = &status.counts;
    if counts.conflicted > 0 {
        Conflicted
    } else if counts.unstaged > 0 {
        Modified
    } else if counts.staged > 0 {
        Staged
    } else {
        Untracked
    }
}

/// The breakdown behind the single number, written as a sentence rather than a
/// table of abbreviations.
fn count_tooltip(counts: &ferret_git::Counts) -> String {
    let mut parts = Vec::new();
    if counts.staged > 0 {
        parts.push(format!("{} staged", counts.staged));
    }
    if counts.unstaged > 0 {
        parts.push(format!("{} not staged", counts.unstaged));
    }
    if counts.untracked > 0 {
        parts.push(format!("{} untracked", counts.untracked));
    }
    if counts.conflicted > 0 {
        parts.push(format!("{} conflicted", counts.conflicted));
    }
    if parts.is_empty() {
        return "No changes".to_string();
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_git::{Counts, RepoStatus};

    /// Parse real porcelain rather than building a `RepoStatus` by hand, so
    /// these assert against the same headers git actually emits.
    fn status(text: &str) -> RepoStatus {
        RepoStatus::parse(
            &text.replace('@', "\0").into_bytes(),
            std::path::Path::new("/w"),
        )
    }

    #[test]
    fn push_is_offered_only_where_git_has_somewhere_to_send() {
        // A branch with an upstream it is ahead of: the ordinary case.
        assert!(can_push(&status(
            "# branch.oid abc1234@# branch.head main@\
             # branch.upstream origin/main@# branch.ab +3 -0@"
        )));
        // Ahead of nothing is nothing to push.
        assert!(!can_push(&status(
            "# branch.oid abc1234@# branch.head main@\
             # branch.upstream origin/main@# branch.ab +0 -2@"
        )));
        // A branch with no upstream at all: `git push` would create one, so
        // the button is the invitation to do it.
        assert!(can_push(&status(
            "# branch.oid abc1234@# branch.head feat/x@"
        )));

        // A detached HEAD also has no upstream, and is not unborn — the shape
        // that used to slip through. Plain `git push` has no current branch
        // here and fails outright, so nothing may be advertised.
        assert!(!can_push(&status(
            "# branch.oid 51be0adc951f1e88a30b5b60857c9442c58d13f0@# branch.head (detached)@"
        )));
        // A fresh `git init` has no commit to send.
        assert!(!can_push(&status(
            "# branch.oid (initial)@# branch.head main@"
        )));
        // Headers absent or unparseable: no guess.
        assert!(!can_push(&status("")));
    }

    #[test]
    fn the_breakdown_names_only_what_is_there() {
        assert_eq!(
            count_tooltip(&Counts {
                changed: 3,
                staged: 1,
                unstaged: 1,
                untracked: 1,
                conflicted: 0,
            }),
            "1 staged · 1 not staged · 1 untracked"
        );
        // A clean tree never renders this, but the wording must not be a lie
        // if it ever does.
        assert_eq!(count_tooltip(&Counts::default()), "No changes");
    }
}
