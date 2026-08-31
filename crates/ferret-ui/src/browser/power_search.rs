//! Power Search modal: bounded live results with generation-safe cancellation.

use std::time::Duration;

use ferret_search::{
    CaseMode, ContentQuery, EntryKindFilter, InProcessSearchService, NameQuery, PatternSyntax,
    SearchEvent, SearchFinished, SearchGeneration, SearchMatch, SearchRequest, SearchResult,
    SearchScope, SearchScopeKind, SearchService as _,
};
use gpui::{
    AppContext as _, ClickEvent, Context, Entity, Focusable as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use super::*;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(140);
const DIALOG_WIDTH: f32 = 860.;
const DIALOG_HEIGHT: f32 = 610.;
const ROW_HEIGHT: f32 = 34.;
/// Dialog padding plus its two one-pixel borders.
const DIALOG_CHROME: f32 = 34.;
const MIN_MARGIN_TOP: f32 = 24.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    All,
    Files,
    Folders,
}

impl SearchKind {
    fn filter(self) -> EntryKindFilter {
        match self {
            Self::All => EntryKindFilter::All,
            Self::Files => EntryKindFilter::Files,
            Self::Folders => EntryKindFilter::Folders,
        }
    }
}

pub(super) struct PowerSearch {
    browser: WeakEntity<Browser>,
    current_folder: VfsPath,
    name_input: Entity<InputState>,
    extension_input: Entity<InputState>,
    content_input: Entity<InputState>,
    kind: SearchKind,
    scope: SearchScopeKind,
    case_sensitive: bool,
    content_regex: bool,
    content_whole_word: bool,
    results: Vec<SearchResult>,
    selected: Option<usize>,
    generation: u64,
    searching: bool,
    truncated: bool,
    warning: Option<String>,
    search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl PowerSearch {
    fn new(
        browser: WeakEntity<Browser>,
        current_folder: VfsPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Name, extension, or pattern"));
        let extension_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("swift, png, rs…"));
        let content_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Text or regular expression"));

        let mut subscriptions = Vec::with_capacity(3);
        for input in [&name_input, &extension_input, &content_input] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.restart_search(cx),
                    InputEvent::PressEnter { .. } => this.open_selected(window, cx),
                    _ => {}
                },
            ));
        }
        Self {
            browser,
            current_folder,
            name_input,
            extension_input,
            content_input,
            kind: SearchKind::All,
            scope: SearchScopeKind::CurrentFolder,
            case_sensitive: false,
            content_regex: false,
            content_whole_word: false,
            results: Vec::new(),
            selected: None,
            generation: 0,
            searching: false,
            truncated: false,
            warning: None,
            search_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn restart_search(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.search_task.take();
        self.results.clear();
        self.selected = None;
        self.truncated = false;
        self.warning = None;

        let name_pattern = self.name_input.read(cx).value().trim().to_string();
        let content_pattern = self.content_input.read(cx).value().trim().to_string();
        if name_pattern.is_empty() && content_pattern.is_empty() {
            self.searching = false;
            cx.notify();
            return;
        }
        let extensions = self
            .extension_input
            .read(cx)
            .value()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(Into::into)
            .collect();
        let root = if self.scope == SearchScopeKind::Home {
            dirs::home_dir()
                .map(VfsPath::local)
                .unwrap_or_else(|| self.current_folder.clone())
        } else {
            self.current_folder.clone()
        };
        let mut name = NameQuery::substring(name_pattern);
        if self.case_sensitive {
            name.case = CaseMode::Sensitive;
        }
        let content = (!content_pattern.is_empty()).then_some(ContentQuery {
            pattern: content_pattern,
            syntax: if self.content_regex {
                PatternSyntax::Regex
            } else {
                PatternSyntax::Literal
            },
            case: if self.case_sensitive {
                CaseMode::Sensitive
            } else {
                CaseMode::Smart
            },
            whole_word: self.content_whole_word,
        });
        let request = SearchRequest {
            generation: SearchGeneration(generation),
            scope: SearchScope {
                kind: self.scope,
                roots: vec![root],
            },
            name,
            content,
            filters: ferret_search::SearchFilters {
                kind: self.kind.filter(),
                extensions,
                ..Default::default()
            },
            limits: Default::default(),
        };

        self.searching = true;
        cx.notify();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let session = match InProcessSearchService.start(request) {
                Ok(session) => session,
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.searching = false;
                        this.warning = Some(error.to_string());
                        cx.notify();
                    });
                    return;
                }
            };
            while let Some(event) = session.next().await {
                let terminal = matches!(event, SearchEvent::Finished(_));
                let stale = this
                    .update(cx, |this, cx| {
                        if this.generation != generation {
                            return true;
                        }
                        match event {
                            SearchEvent::Batch(batch) => this.results.extend(batch.results),
                            SearchEvent::Warning(warning) => this.warning = Some(warning.message),
                            SearchEvent::Finished(SearchFinished::Complete { stats, .. }) => {
                                this.searching = false;
                                this.truncated = stats.truncated;
                            }
                            SearchEvent::Finished(_) => this.searching = false,
                            _ => {}
                        }
                        cx.notify();
                        false
                    })
                    .unwrap_or(true);
                if stale || terminal {
                    break;
                }
            }
        }));
    }

    fn set_kind(&mut self, kind: SearchKind, cx: &mut Context<Self>) {
        if self.kind != kind {
            self.kind = kind;
            self.restart_search(cx);
        }
    }

    fn set_scope(&mut self, scope: SearchScopeKind, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.restart_search(cx);
        }
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.selected.and_then(|row| self.results.get(row)).cloned() else {
            return;
        };
        open_result(&self.browser, result, window, cx);
    }

    fn segment(id: &'static str, label: &'static str, active: bool) -> Button {
        let button = Button::new(id).compact().label(label);
        if active {
            button.primary()
        } else {
            button.ghost()
        }
    }
}

fn open_result(
    browser: &WeakEntity<Browser>,
    result: SearchResult,
    window: &mut Window,
    cx: &mut Context<PowerSearch>,
) {
    let is_dir = result.kind.is_dir_like();
    let path = result.path;
    let _ = browser.update(cx, |browser, cx| {
        if is_dir {
            browser.navigate(path, window, cx);
        } else {
            browser.open_with_default(path, window, cx);
        }
    });
    window.close_dialog(cx);
}

impl Render for PowerSearch {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let count = self.results.len();
        let selected = self.selected;
        let rows = gpui::uniform_list(
            "power-search-results",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        let Some(result) = this.results.get(index).cloned() else {
                            return div().into_any_element();
                        };
                        let (display_name, location, kind_label) = match &result.matched {
                            SearchMatch::Filename { .. } => (
                                result.name.to_string(),
                                result
                                    .path
                                    .parent()
                                    .map(|path| path.to_string())
                                    .unwrap_or_default(),
                                if result.kind.is_dir_like() {
                                    "Folder"
                                } else {
                                    "File"
                                },
                            ),
                            SearchMatch::Content { line, preview, .. } => (
                                format!("{}:{line}", result.name),
                                preview.to_string(),
                                "Match",
                            ),
                        };
                        let is_dir = result.kind.is_dir_like();
                        let open_target = result.clone();
                        h_flex()
                            .id(("power-search-row", index))
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .border_b_1()
                            .border_color(cx.theme().border.opacity(0.55))
                            .when(selected == Some(index), |row| {
                                row.bg(cx.theme().accent.opacity(0.16))
                            })
                            .hover(|row| row.bg(cx.theme().secondary.opacity(0.65)))
                            .child(
                                h_flex()
                                    .w(px(310.))
                                    .min_w_0()
                                    .gap_2()
                                    .child(Icon::new(if is_dir {
                                        gpui_component::IconName::Folder
                                    } else {
                                        gpui_component::IconName::File
                                    }))
                                    .child(div().truncate().child(display_name)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(location),
                            )
                            .child(
                                div()
                                    .w(px(72.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(kind_label),
                            )
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                this.selected = Some(index);
                                if event.click_count() >= 2 {
                                    open_result(&this.browser, open_target.clone(), window, cx);
                                }
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .size_full();

        v_flex()
            .w_full()
            .h(px(DIALOG_HEIGHT))
            .text_size(px(13.))
            .child(
                h_flex()
                    .h(px(46.))
                    .px_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(gpui_component::IconName::Search).text_color(theme.accent))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Power Search"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("power-search-done")
                            .ghost()
                            .label("Done")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(72.))
                                    .text_color(theme.muted_foreground)
                                    .child("Filename"),
                            )
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.name_input)
                                        .prefix(gpui_component::IconName::Search)
                                        .cleanable(true),
                                ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().w(px(72.)))
                            .child(
                                Self::segment("search-all", "All", self.kind == SearchKind::All)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_kind(SearchKind::All, cx)
                                    })),
                            )
                            .child(
                                Self::segment(
                                    "search-files",
                                    "Files",
                                    self.kind == SearchKind::Files,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.set_kind(SearchKind::Files, cx),
                                )),
                            )
                            .child(
                                Self::segment(
                                    "search-folders",
                                    "Folders",
                                    self.kind == SearchKind::Folders,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.set_kind(SearchKind::Folders, cx),
                                )),
                            )
                            .child(
                                div()
                                    .ml_2()
                                    .text_color(theme.muted_foreground)
                                    .child("Ext:"),
                            )
                            .child(
                                div()
                                    .w(px(190.))
                                    .child(Input::new(&self.extension_input).cleanable(true)),
                            )
                            .child(
                                Button::new("search-case")
                                    .compact()
                                    .label("Aa")
                                    .when(self.case_sensitive, |button| button.primary())
                                    .when(!self.case_sensitive, |button| button.outline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.case_sensitive = !this.case_sensitive;
                                        this.restart_search(cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(72.))
                                    .text_color(theme.muted_foreground)
                                    .child("Content"),
                            )
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.content_input)
                                        .prefix(gpui_component::IconName::Search)
                                        .cleanable(true),
                                ),
                            )
                            .child(
                                Button::new("search-content-regex")
                                    .compact()
                                    .label("Regex")
                                    .when(self.content_regex, |button| button.primary())
                                    .when(!self.content_regex, |button| button.outline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.content_regex = !this.content_regex;
                                        this.restart_search(cx);
                                    })),
                            )
                            .child(
                                Button::new("search-content-word")
                                    .compact()
                                    .label("Word")
                                    .when(self.content_whole_word, |button| button.primary())
                                    .when(!self.content_whole_word, |button| button.outline())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.content_whole_word = !this.content_whole_word;
                                        this.restart_search(cx);
                                    })),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .h(px(42.))
                    .px_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Self::segment(
                            "scope-current",
                            "Current Folder",
                            self.scope == SearchScopeKind::CurrentFolder,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_scope(SearchScopeKind::CurrentFolder, cx)
                        })),
                    )
                    .child(
                        Self::segment("scope-home", "Home", self.scope == SearchScopeKind::Home)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_scope(SearchScopeKind::Home, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(if self.searching {
                                SharedString::from("Searching…")
                            } else {
                                SharedString::from(format!("{count} results"))
                            }),
                    ),
            )
            .child(
                h_flex()
                    .h(px(32.))
                    .px_3()
                    .bg(theme.secondary.opacity(0.45))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(310.)).child("Name"))
                    .child(div().flex_1().child("Location / matching line"))
                    .child(div().w(px(72.)).child("Kind")),
            )
            .child(div().flex_1().min_h_0().child(if count == 0 {
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child(
                        if self.name_input.read(cx).value().trim().is_empty()
                            && self.content_input.read(cx).value().trim().is_empty()
                        {
                            "Type a filename or content pattern to search"
                        } else if self.searching {
                            "Searching the filesystem…"
                        } else {
                            "No matching files"
                        },
                    )
                    .into_any_element()
            } else {
                rows.into_any_element()
            }))
            .child(
                h_flex()
                    .h(px(30.))
                    .px_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(if self.truncated {
                        SharedString::from(format!("Showing first {count} results"))
                    } else if let Some(warning) = &self.warning {
                        SharedString::from(warning.clone())
                    } else {
                        SharedString::from(format!("{count} results"))
                    })
                    .child(div().flex_1())
                    .child("Double-click to open · Esc to close"),
            )
    }
}

impl Browser {
    pub(super) fn toggle_power_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
            return;
        }
        let current_folder = self.active().folder.read(cx).dir.clone();
        let search = match &self.power_search {
            Some(existing) => {
                existing.update(cx, |search, cx| {
                    search.current_folder = current_folder.clone();
                    search.restart_search(cx);
                });
                existing.clone()
            }
            None => {
                let browser = cx.weak_entity();
                let created = cx.new(|cx| PowerSearch::new(browser, current_folder, window, cx));
                self.power_search = Some(created.clone());
                created
            }
        };
        let focus = search.read(cx).name_input.read(cx).focus_handle(cx).clone();
        window.focus(&focus, cx);
        window.open_dialog(cx, move |dialog, window, _cx| {
            let viewport = f32::from(window.viewport_size().height);
            let top = ((viewport - (DIALOG_HEIGHT + DIALOG_CHROME)) / 2.).max(MIN_MARGIN_TOP);
            dialog
                .w(px(DIALOG_WIDTH))
                .margin_top(px(top))
                .p_0()
                .close_button(false)
                .child(search.clone())
        });
        cx.notify();
    }
}
