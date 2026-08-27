//! The path bar: the breadcrumb, the inline filter, and the editable
//! path with its completion popover.
//!
//! Completion is deliberately directory-only and capped — a popover
//! taller than a handful of rows stops being a hint and becomes a second
//! file list.

use super::*;

impl Browser {
    pub(super) fn begin_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_editing.is_some() {
            return;
        }
        let current = self.active().folder.read(cx).dir.to_string();

        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).default_value(&current);
            // The whole path starts selected, so typing replaces it the way
            // every other path field does. Without this the caret sits at
            // offset 0 and the first keystroke prepends to the current path.
            state.set_selected_range(0..current.chars().count(), cx);
            state
        });

        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.path_editing = None;
                    if let Some(target) = resolve_path_input(&text) {
                        this.navigate(target, window, cx);
                    }
                    cx.notify();
                }
                // Enter navigates to what is typed; the completions only
                // narrow, so they are refreshed on every keystroke.
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    this.refresh_completions(text, cx);
                }
                InputEvent::Blur => {
                    this.cancel_path_edit(cx);
                }
                _ => {}
            },
        );

        // Focus the editor directly. Preselection of the whole path arrives
        // with inline-editing polish; the fresh value is committed whole on
        // Enter either way.
        use gpui::Focusable as _;
        let handle = input.read(cx).focus_handle(cx).clone();
        window.focus(&handle, cx);

        self.path_editing = Some(PathEdit {
            input,
            _subscription: subscription,
            completions: Vec::new(),
            highlighted: 0,
            generation: 0,
        });
        cx.notify();
    }

    /// The single path into the folder's filter.
    ///
    /// `InputState::set_value` deliberately suppresses its change event, so a
    /// programmatic clear cannot rely on the subscription to notice — both
    /// routes call this instead.
    pub(super) fn apply_filter(&mut self, filter: Option<String>, cx: &mut Context<Self>) {
        self.active().folder.update(cx, |f, cx| {
            f.set_filter(filter);
            cx.emit(FolderEvent::RowsChanged);
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn cancel_path_edit(&mut self, cx: &mut Context<Self>) {
        if self.path_editing.take().is_some() {
            cx.notify();
        }
    }

    /// Re-list the directory being typed into and keep the matches.
    ///
    /// The listing runs on the executor like every other one — a path bar
    /// that stutters on a slow network mount is worse than no completion at
    /// all — and a stale result is dropped by generation.
    fn refresh_completions(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_mut() else {
            return;
        };
        edit.generation += 1;
        let generation = edit.generation;

        let Some((dir, prefix)) = completion_target(&text) else {
            edit.completions.clear();
            edit.highlighted = 0;
            cx.notify();
            return;
        };

        let fs = cx.global::<FerretState>().fs;
        cx.spawn(async move |this, cx| {
            let mut stream = fs.read_dir(&VfsPath::local(dir));
            let mut entries = Vec::new();
            while let Some(Ok(entry)) = futures::StreamExt::next(&mut stream).await {
                entries.push(entry);
            }
            let matches = matching_directories(&entries, &prefix);

            let _ = this.update(cx, |this, cx| {
                let Some(edit) = this.path_editing.as_mut() else {
                    return;
                };
                if edit.generation != generation {
                    return; // A newer keystroke already owns the popover.
                }
                edit.completions = matches;
                edit.highlighted = 0;
                cx.notify();
            });
        })
        .detach();
    }

    /// Move the highlight; the list wraps so holding the key cycles.
    pub(super) fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_mut() else {
            return;
        };
        let n = edit.completions.len();
        if n == 0 {
            return;
        }
        let next = (edit.highlighted as isize + delta).rem_euclid(n as isize) as usize;
        edit.highlighted = next;
        cx.notify();
    }

    /// Put the highlighted completion in the field, with a trailing separator
    /// so the next Tab completes the level below it.
    ///
    /// With nothing to accept, Tab means what it means everywhere else and
    /// moves to the next region — otherwise focus would be stuck in the path
    /// bar whenever the typed path has no completions.
    pub(super) fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.path_editing.as_ref() else {
            return;
        };
        let Some(path) = edit.completions.get(edit.highlighted).cloned() else {
            self.focus_region(true, window, cx);
            return;
        };
        let input = edit.input.clone();
        let mut text = path.to_string_lossy().into_owned();
        text.push(std::path::MAIN_SEPARATOR);
        input.update(cx, |state, cx| state.set_value(&text, window, cx));
        // Same reason as the filter: no change event follows `set_value`, so
        // the next level's completions are requested here rather than waiting
        // for a keystroke that may never come.
        self.refresh_completions(text, cx);
        cx.notify();
    }

    pub(super) fn render_path_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();

        // The filter rides the path bar in both states (§2: "inline filter"),
        // so narrowing a listing never costs you the breadcrumb.
        let filter = div()
            .w(px(160.))
            .flex_shrink_0()
            .child(Input::new(&self.filter_input).small().cleanable(true));

        let bar = h_flex()
            .h(px(30.))
            .px(px(8.))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background);

        if self.path_editing.is_some() {
            let completions = self.render_completions(cx);
            let input = self.path_editing.as_ref().map(|edit| edit.input.clone());
            return bar
                .key_context(actions::context::PATH_BAR)
                .when_some(input, |bar, input| {
                    bar.child(
                        div()
                            .relative()
                            .flex_1()
                            .child(Input::new(&input).small())
                            // Deferred so it paints above the file pane below
                            // it: an absolutely-positioned sibling would still
                            // be covered by elements that come later.
                            .child(gpui::deferred(completions)),
                    )
                })
                .child(filter)
                .into_any_element();
        }

        let dir = self.active().folder.read(cx).dir.clone();
        let crumbs = breadcrumb_targets(&dir);
        let this = cx.weak_entity();

        let mut items = Vec::with_capacity(crumbs.len());
        for (label, target) in crumbs {
            let is_last = target == dir;
            items.push(BreadcrumbItem::new(label).when(!is_last, |item| {
                let this = this.clone();
                let target = target.clone();
                item.on_click(move |_, window, cx| {
                    // A crumb navigates; without this the click would also
                    // reach the bar behind it and open the editor instead.
                    cx.stop_propagation();
                    let Some(this) = this.upgrade() else {
                        return;
                    };
                    let target = target.clone();
                    this.update(cx, |this, cx| {
                        this.navigate(target, window, cx);
                    });
                })
            }));
        }

        // §4: the bar itself becomes a text input on click — the pencil is one
        // hit target for that, not the only one. The filter and the pencil sit
        // outside this region so clicking them does not start a path edit.
        let crumbs = h_flex()
            .id("path-bar-crumbs")
            .flex_1()
            .min_w_0()
            .h_full()
            .items_center()
            .gap_2()
            .on_click(
                cx.listener(|this, _: &ClickEvent, window, cx| this.begin_path_edit(window, cx)),
            )
            .child(
                Icon::default()
                    .path("icons/file/folder-base.svg")
                    .size_4()
                    .text_color(BrowserTint::folder(cx)),
            )
            .child(Breadcrumb::new().children(items).flex_1().truncate());

        bar.child(crumbs)
            .child(tool_button_pencil(window, cx, this))
            .child(filter)
            .into_any_element()
    }

    /// The completion list under the path field (§6). Empty until a query
    /// lands, so it never flashes an empty box while the listing runs.
    fn render_completions(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let Some(edit) = &self.path_editing else {
            return div().into_any_element();
        };
        if edit.completions.is_empty() {
            return div().into_any_element();
        }

        let highlighted = edit.highlighted;
        let rows: Vec<_> = edit
            .completions
            .iter()
            .enumerate()
            .map(|(ix, path)| {
                let label = path.to_string_lossy().into_owned();
                let target = path.clone();
                h_flex()
                    .id(("completion", ix))
                    .w_full()
                    .h(px(24.))
                    .px(px(8.))
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .when(ix == highlighted, |row| row.bg(theme.selection))
                    .when(ix != highlighted, |row| {
                        row.hover(|row| row.bg(theme.secondary))
                    })
                    .child(
                        Icon::default()
                            .path("icons/file/folder-base.svg")
                            .size_4()
                            .text_color(BrowserTint::folder(cx)),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(if ix == highlighted {
                                crate::theme::text_on(theme.selection, theme.foreground)
                            } else {
                                theme.foreground
                            })
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if let Some(edit) = this.path_editing.as_mut() {
                            edit.highlighted = ix;
                        }
                        let _ = &target;
                        this.accept_completion(window, cx);
                    }))
            })
            .collect();

        v_flex()
            .absolute()
            .top_full()
            .left_0()
            .mt(px(4.))
            .w(px(420.))
            .p(px(4.))
            .gap(px(1.))
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_md()
            .children(rows)
            .into_any_element()
    }
}

/// How many completions the path bar offers at once.
///
/// A popover taller than this stops being a hint and becomes a second file
/// list; typing one more character is the faster way to narrow it.
const MAX_COMPLETIONS: usize = 8;

/// Split typed path text into "the directory to list" and "the prefix to
/// match inside it".
///
/// A trailing separator means the directory itself is complete and everything
/// in it matches; anything else completes the last segment.
fn completion_target(text: &str) -> Option<(PathBuf, String)> {
    let expanded = expand_home(text.trim())?;
    if text.trim().is_empty() {
        return None;
    }
    if text.ends_with('/') || text.ends_with('\\') {
        return Some((expanded, String::new()));
    }
    let parent = expanded.parent()?.to_path_buf();
    let prefix = expanded.file_name()?.to_string_lossy().into_owned();
    Some((parent, prefix))
}

/// Directories in `dir` whose name starts with `prefix`, case-insensitively.
///
/// Directories only: the path bar navigates, and offering files would only
/// produce completions that cannot be accepted. Hidden entries stay out
/// unless the prefix reaches for them explicitly.
fn matching_directories(entries: &[ferret_vfs::Entry], prefix: &str) -> Vec<PathBuf> {
    let wanted = prefix.to_lowercase();
    let mut hits: Vec<&ferret_vfs::Entry> = entries
        .iter()
        .filter(|e| e.is_dir_like())
        .filter(|e| !e.is_hidden() || wanted.starts_with('.'))
        .filter(|e| e.name.to_lowercase().starts_with(&wanted))
        .collect();
    hits.sort_by(|a, b| crate::format::natural_compare(&a.name, &b.name));
    hits.into_iter()
        .take(MAX_COMPLETIONS)
        .filter_map(|e| e.path.as_local().map(Path::to_path_buf))
        .collect()
}

/// `~` and `~/…` expand against the home directory; everything else is taken
/// literally.
fn expand_home(text: &str) -> Option<PathBuf> {
    if text == "~" || text.starts_with("~/") {
        let home = dirs::home_dir()?;
        return Some(home.join(text.trim_start_matches('~').trim_start_matches('/')));
    }
    Some(PathBuf::from(text))
}

/// Expand `~`, trim slashes. Relative paths are accepted as-is from the
/// home-relative expansion only; typing a bare name navigates nowhere rather
/// than guessing a directory.
fn resolve_path_input(text: &str) -> Option<VfsPath> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let expanded = if text == "~" || text.starts_with("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(text.trim_start_matches('~').trim_start_matches('/')),
            None => return None,
        }
    } else {
        PathBuf::from(text)
    };
    Some(VfsPath::local(expanded))
}

/// Tiny shim so the path-bar pencil can reach the browser without another
/// listener context.
fn tool_button_pencil(
    _window: &mut Window,
    _cx: &mut Context<'_, Browser>,
    this: WeakEntity<Browser>,
) -> Button {
    Button::new("edit-path")
        .ghost()
        .xsmall()
        .icon(Icon::default().path(toolbar_icon::PENCIL))
        .tooltip("Edit Path")
        .on_click(move |_, window, cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.begin_path_edit(window, cx));
            }
        })
}

/// `(label, target)` pairs from root to `dir`, with the home prefix folded
/// into a single `~`.
fn breadcrumb_targets(dir: &VfsPath) -> Vec<(String, VfsPath)> {
    let Some(local) = dir.as_local() else {
        return vec![];
    };
    let home = dirs::home_dir();

    let mut stack: Vec<PathBuf> = local.ancestors().map(Path::to_path_buf).collect();
    stack.reverse(); // root first

    // Fold the home prefix away when we are inside it.
    if let Some(home) = home.as_deref()
        && local.starts_with(home)
        && local != home
    {
        stack.retain(|p| !p.starts_with(home));
        let mut out = vec![("~".to_string(), VfsPath::local(home.to_path_buf()))];
        let suffix = local.strip_prefix(home).unwrap_or(local);
        let mut acc = home.to_path_buf();
        for component in suffix.components() {
            acc.push(component.as_os_str());
            out.push((
                component.as_os_str().to_string_lossy().into_owned(),
                VfsPath::local(acc.clone()),
            ));
        }
        return out;
    }

    stack
        .into_iter()
        .map(|p| {
            let label = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".to_string());
            (label, VfsPath::local(p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breadcrumb_folds_the_home_prefix_into_a_tilde() {
        let Some(home) = dirs::home_dir() else {
            return; // No home on this machine; nothing to fold.
        };
        let crumbs = breadcrumb_targets(&VfsPath::local(home.join("Projects/ferret")));
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["~", "Projects", "ferret"]);

        // The tilde still has to navigate somewhere real.
        assert_eq!(crumbs[0].1, VfsPath::local(home));
    }

    fn dir_entry(name: &str, hidden: bool) -> ferret_vfs::Entry {
        ferret_vfs::Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind: ferret_vfs::EntryKind::Dir,
            metadata: Some(ferret_vfs::Metadata {
                hidden,
                ..Default::default()
            }),
            link_target: None,
        }
    }

    fn file_entry(name: &str) -> ferret_vfs::Entry {
        ferret_vfs::Entry {
            path: VfsPath::local(format!("/x/{name}")),
            name: name.to_string(),
            kind: ferret_vfs::EntryKind::File,
            metadata: Some(ferret_vfs::Metadata::default()),
            link_target: None,
        }
    }

    #[test]
    fn a_trailing_separator_lists_the_directory_itself() {
        let (dir, prefix) = completion_target("/usr/local/").expect("a directory to list");
        assert_eq!(dir, PathBuf::from("/usr/local"));
        assert_eq!(prefix, "", "everything inside it matches");

        // Without one, the last segment is what is being completed.
        let (dir, prefix) = completion_target("/usr/lo").expect("a directory to list");
        assert_eq!(dir, PathBuf::from("/usr"));
        assert_eq!(prefix, "lo");
    }

    #[test]
    fn completion_expands_the_home_prefix() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let (dir, prefix) = completion_target("~/Doc").expect("a directory to list");
        assert_eq!(dir, home);
        assert_eq!(prefix, "Doc");
    }

    #[test]
    fn only_directories_complete_a_path() {
        let entries = vec![
            dir_entry("Documents", false),
            dir_entry("Downloads", false),
            file_entry("Doc.txt"),
            dir_entry("Music", false),
        ];

        // A file can never be navigated to, so offering it would produce a
        // completion that cannot be accepted.
        let hits = matching_directories(&entries, "Do");
        let names: Vec<_> = hits
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Documents", "Downloads"]);

        // Matching is case-insensitive.
        assert_eq!(matching_directories(&entries, "do").len(), 2);
    }

    #[test]
    fn hidden_directories_need_to_be_asked_for() {
        let entries = vec![dir_entry(".config", true), dir_entry("code", false)];
        assert_eq!(matching_directories(&entries, "").len(), 1, "code only");

        // Typing the dot reaches for them explicitly.
        let hits = matching_directories(&entries, ".");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with(".config"));
    }

    #[test]
    fn the_completion_list_is_capped() {
        let entries: Vec<_> = (0..50)
            .map(|i| dir_entry(&format!("dir-{i:02}"), false))
            .collect();
        assert_eq!(matching_directories(&entries, "dir").len(), MAX_COMPLETIONS);
    }

    #[test]
    fn a_bare_name_is_not_guessed_into_a_path() {
        assert!(resolve_path_input("   ").is_none());
        assert_eq!(
            resolve_path_input("/usr/local"),
            Some(VfsPath::local("/usr/local"))
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolve_path_input("~"), Some(VfsPath::local(home)));
        }
    }
}
