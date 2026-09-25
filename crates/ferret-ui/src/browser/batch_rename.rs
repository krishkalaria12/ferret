use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ferret_ops::{
    BatchRenameError, BatchRenamePlan, CaseStyle, ExistingNames, MatchSyntax, NumberPlacement,
    RuleTarget,
};
use gpui::{
    App, Context, Entity, Focusable as _, IntoElement, Render, SharedString, Task, WeakEntity,
    Window, px,
};
use gpui_component::dialog::{Cancel, Confirm};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::table::{DataTable, TableState};

use super::*;
use preview_table::{PREVIEW_ROW_HEIGHT, PreviewDisplayRow, PreviewStatus, RenamePreviewDelegate};
use rule_editor::{RuleEditor, RuleKind};

mod preview_table;
mod rule_editor;

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(90);
const DIALOG_WIDTH: f32 = 920.;
const DIALOG_HEIGHT: f32 = 660.;
const DIALOG_BORDER: f32 = 2.;
const MIN_MARGIN_VERTICAL: f32 = 24.;
const MIN_MARGIN_HORIZONTAL: f32 = 16.;
pub(super) struct BatchRenameDialog {
    folder: Entity<Folder>,
    sources: Arc<[VfsPath]>,
    rules: Vec<RuleEditor>,
    table: Entity<TableState<RenamePreviewDelegate>>,
    existing: Option<Arc<ExistingNames>>,
    plan: Option<Arc<BatchRenamePlan>>,
    preview_task: Option<Task<()>>,
    generation: u64,
    checking: bool,
    applying: bool,
    fatal: bool,
    error: Option<SharedString>,
    recovery_paths: Arc<[VfsPath]>,
}

impl BatchRenameDialog {
    fn new(
        folder: Entity<Folder>,
        sources: Arc<[VfsPath]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| {
            TableState::new(RenamePreviewDelegate::default(), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
        });
        let mut this = Self {
            folder,
            sources,
            rules: Vec::new(),
            table,
            existing: None,
            plan: None,
            preview_task: None,
            generation: 0,
            checking: false,
            applying: false,
            fatal: false,
            error: None,
            recovery_paths: Arc::default(),
        };
        this.rules
            .push(RuleEditor::new(RuleKind::FindReplace, window, cx));
        this.load_existing(cx);
        this
    }

    fn reset(
        &mut self,
        folder: Entity<Folder>,
        sources: Arc<[VfsPath]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        self.preview_task.take();
        self.release_existing(cx);
        self.folder = folder;
        self.sources = sources;
        self.rules.clear();
        self.rules
            .push(RuleEditor::new(RuleKind::FindReplace, window, cx));
        self.plan = None;
        self.checking = false;
        self.applying = false;
        self.fatal = false;
        self.error = None;
        self.recovery_paths = Arc::default();
        self.table = cx.new(|cx| {
            TableState::new(RenamePreviewDelegate::default(), window, cx)
                .row_selectable(false)
                .col_selectable(false)
                .col_movable(false)
        });
        self.load_existing(cx);
    }

    fn load_existing(&mut self, cx: &mut Context<Self>) {
        if self.applying || self.fatal {
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.preview_task.take();
        self.release_existing(cx);
        self.recovery_paths = Arc::default();
        self.checking = true;
        self.error = None;
        self.clear_preview(cx);

        let sources = self.sources.clone();
        let fs = cx.global::<FerretState>().fs.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { ferret_ops::load_existing(&fs, &sources).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match loaded {
                    Ok(existing) => {
                        this.existing = Some(Arc::new(existing));
                        this.schedule_preview(cx);
                    }
                    Err(error) => {
                        this.checking = false;
                        this.error = Some(SharedString::from(error.to_string()));
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn clear_preview(&mut self, cx: &mut Context<Self>) {
        let stale_plan = self.plan.take();
        let stale_rows = self.table.read(cx).delegate().rows.clone();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().rows = Arc::default();
            table.refresh(cx);
            cx.notify();
        });
        if let Some(plan) = stale_plan {
            cx.background_executor()
                .spawn(async move { drop(plan) })
                .detach();
        }
        cx.background_executor()
            .spawn(async move { drop(stale_rows) })
            .detach();
        cx.notify();
    }

    fn release_existing(&mut self, cx: &mut Context<Self>) {
        if let Some(existing) = self.existing.take() {
            cx.background_executor()
                .spawn(async move { drop(existing) })
                .detach();
        }
    }

    fn schedule_preview(&mut self, cx: &mut Context<Self>) {
        if self.applying || self.fatal {
            return;
        }
        let Some(existing) = self.existing.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.preview_task.take();
        self.clear_preview(cx);
        self.checking = true;
        self.error = None;

        let mut rules = Vec::with_capacity(self.rules.len());
        let mut first_error = None;
        for (index, rule) in self.rules.iter_mut().enumerate() {
            rule.error = None;
            match rule.record(cx) {
                Ok(record) => rules.push(record),
                Err(error) => {
                    rule.error = Some(error.clone());
                    first_error.get_or_insert_with(|| {
                        SharedString::from(format!("Rule {}: {error}", index + 1))
                    });
                }
            }
        }
        if let Some(error) = first_error {
            self.checking = false;
            self.error = Some(error);
            cx.notify();
            return;
        }

        let sources = self.sources.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
            let preview = cx
                .background_executor()
                .spawn(async move {
                    ferret_ops::plan_with_existing(&sources, &rules, &existing).map(|plan| {
                        let rows = plan
                            .rows()
                            .iter()
                            .map(|row| PreviewDisplayRow {
                                old_name: SharedString::from(row.old_name.clone()),
                                new_name: SharedString::from(row.new_name.clone()),
                                status: match row.issues().first() {
                                    Some(issue) => PreviewStatus::Issue(issue.clone()),
                                    None if !row.changed() => PreviewStatus::Unchanged,
                                    None => PreviewStatus::Ready,
                                },
                                issue: !row.issues().is_empty(),
                                changed: row.changed(),
                            })
                            .collect();
                        (plan, rows)
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.checking = false;
                match preview {
                    Ok((plan, rows)) => {
                        this.set_plan(plan, rows, cx);
                    }
                    Err(error) => {
                        this.error = Some(SharedString::from(error.to_string()));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn set_plan(
        &mut self,
        plan: BatchRenamePlan,
        rows: Vec<PreviewDisplayRow>,
        cx: &mut Context<Self>,
    ) {
        self.plan = Some(Arc::new(plan));
        self.table.update(cx, |table, cx| {
            table.delegate_mut().rows = Arc::from(rows);
            table.refresh(cx);
            cx.notify();
        });
    }

    fn can_apply(&self) -> bool {
        !self.applying
            && !self.fatal
            && !self.checking
            && self.plan.as_ref().is_some_and(|plan| plan.can_apply())
    }

    fn render_rule(&mut self, index: usize, applying: bool, cx: &mut Context<Self>) -> AnyElement {
        let rule = &self.rules[index];
        let kind = rule.kind;
        let target = rule.target;
        let case_style = rule.case_style;
        let syntax = rule.syntax;
        let case_sensitive = rule.case_sensitive;
        let placement = rule.placement;
        let find = rule.find.clone();
        let replacement = rule.replacement.clone();
        let text = rule.text.clone();
        let start = rule.start.clone();
        let step = rule.step.clone();
        let width = rule.width.clone();
        let error = rule.error.clone();
        let last_rule = index + 1 == self.rules.len();
        let weak = cx.weak_entity();
        let weak_for_kind = weak.clone();

        let kind_button = Button::new(("batch-rule-kind", index))
            .compact()
            .label(kind.label())
            .disabled(applying)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for option in RuleKind::ALL {
                    let weak = weak_for_kind.clone();
                    menu = menu.item(
                        PopupMenuItem::new(option.label())
                            .checked(option == kind)
                            .on_click(move |_, _, cx| {
                                if let Some(dialog) = weak.upgrade() {
                                    dialog.update(cx, |dialog, cx| {
                                        dialog.rules[index].kind = option;
                                        dialog.schedule_preview(cx);
                                    });
                                }
                            }),
                    );
                }
                menu
            });

        let controls: AnyElement = match kind {
            RuleKind::FindReplace => h_flex()
                .gap_2()
                .items_center()
                .child(target_button(index, target, weak.clone(), applying))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&find).small().disabled(applying)),
                )
                .child(div().text_color(cx.theme().muted_foreground).child("→"))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&replacement).small().disabled(applying)),
                )
                .child(
                    Button::new(("batch-rule-regex", index))
                        .compact()
                        .label(".*")
                        .disabled(applying)
                        .when(syntax == MatchSyntax::Regex, |button| button.primary())
                        .when(syntax != MatchSyntax::Regex, |button| button.outline())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let syntax = this.rules[index].syntax;
                            this.rules[index].syntax = match syntax {
                                MatchSyntax::Literal => MatchSyntax::Regex,
                                MatchSyntax::Regex => MatchSyntax::Literal,
                            };
                            this.schedule_preview(cx);
                        })),
                )
                .child(
                    Button::new(("batch-rule-case", index))
                        .compact()
                        .label("Aa")
                        .disabled(applying)
                        .when(case_sensitive, |button| button.primary())
                        .when(!case_sensitive, |button| button.outline())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.rules[index].case_sensitive = !this.rules[index].case_sensitive;
                            this.schedule_preview(cx);
                        })),
                )
                .into_any_element(),
            RuleKind::Prefix | RuleKind::Suffix => h_flex()
                .gap_2()
                .items_center()
                .child(target_button(index, target, weak.clone(), applying))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&text).small().disabled(applying)),
                )
                .into_any_element(),
            RuleKind::Number => h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(92.))
                        .child(Input::new(&start).small().disabled(applying)),
                )
                .child(
                    div()
                        .w(px(92.))
                        .child(Input::new(&step).small().disabled(applying)),
                )
                .child(
                    div()
                        .w(px(92.))
                        .child(Input::new(&width).small().disabled(applying)),
                )
                .child(
                    Button::new(("batch-number-prefix", index))
                        .compact()
                        .label("Before")
                        .disabled(applying)
                        .when(placement == NumberPlacement::Prefix, |button| {
                            button.primary()
                        })
                        .when(placement != NumberPlacement::Prefix, |button| {
                            button.outline()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.rules[index].placement = NumberPlacement::Prefix;
                            this.schedule_preview(cx);
                        })),
                )
                .child(
                    Button::new(("batch-number-suffix", index))
                        .compact()
                        .label("After")
                        .disabled(applying)
                        .when(placement == NumberPlacement::Suffix, |button| {
                            button.primary()
                        })
                        .when(placement != NumberPlacement::Suffix, |button| {
                            button.outline()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.rules[index].placement = NumberPlacement::Suffix;
                            this.schedule_preview(cx);
                        })),
                )
                .into_any_element(),
            RuleKind::Case => h_flex()
                .gap_2()
                .items_center()
                .child(target_button(index, target, weak.clone(), applying))
                .child(case_style_button(index, case_style, weak, applying))
                .into_any_element(),
            RuleKind::Extension => h_flex()
                .gap_2()
                .items_center()
                .child(div().w(px(110.)).child("Extension"))
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(&text).small().disabled(applying)),
                )
                .into_any_element(),
        };

        v_flex()
            .id(("batch-rule", index))
            .gap_2()
            .p_3()
            .rounded(theme_radius(cx))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary.opacity(0.35))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(kind_button)
                    .child(div().flex_1())
                    .child(
                        Button::new(("batch-rule-up", index))
                            .compact()
                            .ghost()
                            .label("↑")
                            .disabled(applying || index == 0)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_rule(index, -1, cx)),
                            ),
                    )
                    .child(
                        Button::new(("batch-rule-down", index))
                            .compact()
                            .ghost()
                            .label("↓")
                            .disabled(applying || last_rule)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.move_rule(index, 1, cx)),
                            ),
                    )
                    .child(
                        Button::new(("batch-rule-remove", index))
                            .compact()
                            .ghost()
                            .label("Remove")
                            .disabled(applying)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.rules.remove(index);
                                this.schedule_preview(cx);
                            })),
                    ),
            )
            .child(controls)
            .when_some(error, |element, error| {
                element.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .into_any_element()
    }

    fn move_rule(&mut self, index: usize, offset: isize, cx: &mut Context<Self>) {
        let target = index as isize + offset;
        if target < 0 || target >= self.rules.len() as isize {
            return;
        }
        let rule = self.rules.remove(index);
        self.rules.insert(target as usize, rule);
        self.schedule_preview(cx);
    }
}

fn theme_radius(cx: &App) -> gpui::Pixels {
    cx.theme().radius
}

fn target_button(
    index: usize,
    current: RuleTarget,
    weak: WeakEntity<BatchRenameDialog>,
    disabled: bool,
) -> impl IntoElement {
    Button::new(("batch-rule-target", index))
        .compact()
        .label(target_label(current))
        .disabled(disabled)
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for target in [
                RuleTarget::Stem,
                RuleTarget::FullName,
                RuleTarget::Extension,
            ] {
                let weak = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new(target_label(target))
                        .checked(current == target)
                        .on_click(move |_, _, cx| {
                            if let Some(dialog) = weak.upgrade() {
                                dialog.update(cx, |dialog, cx| {
                                    dialog.rules[index].target = target;
                                    dialog.schedule_preview(cx);
                                });
                            }
                        }),
                );
            }
            menu
        })
}

fn target_label(target: RuleTarget) -> &'static str {
    match target {
        RuleTarget::Stem => "Name",
        RuleTarget::FullName => "Full name",
        RuleTarget::Extension => "Extension",
    }
}

fn case_style_button(
    index: usize,
    current: CaseStyle,
    weak: WeakEntity<BatchRenameDialog>,
    disabled: bool,
) -> impl IntoElement {
    Button::new(("batch-rule-case-style", index))
        .compact()
        .label(case_style_label(current))
        .disabled(disabled)
        .dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for style in [
                CaseStyle::Lower,
                CaseStyle::Upper,
                CaseStyle::Title,
                CaseStyle::Kebab,
                CaseStyle::Snake,
            ] {
                let weak = weak.clone();
                menu = menu.item(
                    PopupMenuItem::new(case_style_label(style))
                        .checked(current == style)
                        .on_click(move |_, _, cx| {
                            if let Some(dialog) = weak.upgrade() {
                                dialog.update(cx, |dialog, cx| {
                                    dialog.rules[index].case_style = style;
                                    dialog.schedule_preview(cx);
                                });
                            }
                        }),
                );
            }
            menu
        })
}

fn case_style_label(style: CaseStyle) -> &'static str {
    match style {
        CaseStyle::Lower => "lowercase",
        CaseStyle::Upper => "UPPERCASE",
        CaseStyle::Title => "Title Case",
        CaseStyle::Kebab => "kebab-case",
        CaseStyle::Snake => "snake_case",
    }
}

enum RenameFailureKind {
    InvalidPlan,
    RecoveryRequired,
    Other,
}

struct RenameFailure {
    kind: RenameFailureKind,
    message: SharedString,
    retry_plan: Option<Arc<BatchRenamePlan>>,
    recovery_paths: Arc<[VfsPath]>,
}

struct PreparedRename {
    result: std::result::Result<usize, RenameFailure>,
    renames: Vec<(VfsPath, VfsPath)>,
    selection: HashSet<VfsPath>,
    cursor: Option<VfsPath>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct DialogGeometry {
    body_width: f32,
    body_height: f32,
    margin_top: f32,
}

fn dialog_geometry(viewport_width: f32, viewport_height: f32) -> DialogGeometry {
    let body_width =
        DIALOG_WIDTH.min((viewport_width - MIN_MARGIN_HORIZONTAL * 2. - DIALOG_BORDER).max(0.));
    let body_height =
        DIALOG_HEIGHT.min((viewport_height - MIN_MARGIN_VERTICAL * 2. - DIALOG_BORDER).max(0.));
    let margin_top = ((viewport_height - body_height - DIALOG_BORDER) / 2.).max(0.);
    DialogGeometry {
        body_width,
        body_height,
        margin_top,
    }
}

impl Render for BatchRenameDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.sources.len();
        let changed = self
            .plan
            .as_ref()
            .map(|plan| plan.changed_len())
            .unwrap_or(0);
        let ready = self.can_apply();
        let applying = self.applying;
        let locked = applying || self.fatal;
        let weak = cx.weak_entity();
        let weak_for_close = weak.clone();
        let recovery_paths = self.recovery_paths.clone();
        let add_rule = Button::new("batch-add-rule")
            .compact()
            .label("Add rule")
            .icon(gpui_component::IconName::Plus)
            .disabled(locked || self.rules.len() >= ferret_ops::config::MAX_RULES)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for kind in RuleKind::ALL {
                    let weak = weak.clone();
                    menu = menu.item(PopupMenuItem::new(kind.label()).on_click(
                        move |_, window, cx| {
                            if let Some(dialog) = weak.upgrade() {
                                dialog.update(cx, |dialog, cx| {
                                    dialog.rules.push(RuleEditor::new(kind, window, cx));
                                    dialog.schedule_preview(cx);
                                });
                            }
                        },
                    ));
                }
                menu
            });
        let rules: Vec<AnyElement> = (0..self.rules.len())
            .map(|index| self.render_rule(index, locked, cx))
            .collect();
        let theme = cx.theme();

        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .overflow_hidden()
            .text_size(px(13.))
            .child(
                h_flex()
                    .h(px(48.))
                    .px_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(gpui_component::IconName::Settings2).text_color(theme.accent))
                    .child(
                        v_flex()
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Batch Rename"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{count} selected items")),
                            ),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("batch-cancel-header")
                            .ghost()
                            .label("Cancel")
                            .disabled(applying || self.fatal)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .when(self.fatal, |header| {
                        let paths = recovery_paths.clone();
                        header
                            .child(
                                Button::new("batch-copy-recovery")
                                    .ghost()
                                    .label("Copy paths")
                                    .on_click(move |_, _, cx| {
                                        let text = paths
                                            .iter()
                                            .map(ToString::to_string)
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            text,
                                        ));
                                    }),
                            )
                            .child(
                                Button::new("batch-close-fatal")
                                    .primary()
                                    .label("Close")
                                    .on_click(move |_, window, cx| {
                                        if let Some(dialog) = weak_for_close.upgrade() {
                                            dialog.update(cx, |dialog, cx| {
                                                dialog.generation =
                                                    dialog.generation.wrapping_add(1);
                                                dialog.preview_task.take();
                                                dialog.recovery_paths = Arc::default();
                                                dialog.release_existing(cx);
                                                dialog.clear_preview(cx);
                                                dialog.fatal = false;
                                            });
                                        }
                                        window.close_dialog(cx);
                                    }),
                            )
                    }),
            )
            .child(
                v_flex()
                    .px_4()
                    .pt_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("RULES"),
                            )
                            .child(div().flex_1())
                            .child(add_rule),
                    )
                    .child(
                        v_flex()
                            .max_h(px(238.))
                            .overflow_y_scrollbar()
                            .gap_2()
                            .children(rules),
                    ),
            )
            .when_some(self.error.clone(), |element, error| {
                element.child(
                    h_flex()
                        .mx_4()
                        .mt_3()
                        .px_3()
                        .py_2()
                        .gap_2()
                        .rounded(theme.radius)
                        .bg(theme.danger.opacity(0.1))
                        .text_color(theme.danger)
                        .child(Icon::new(gpui_component::IconName::TriangleAlert))
                        .child(div().text_size(px(12.)).child(error))
                        .child(
                            Button::new("batch-retry-load")
                                .ghost()
                                .label("Retry")
                                .disabled(locked)
                                .on_click(cx.listener(|this, _, _, cx| this.load_existing(cx))),
                        ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).with_size(px(PREVIEW_ROW_HEIGHT))),
            )
            .child(
                h_flex()
                    .h(px(48.))
                    .px_4()
                    .gap_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .child(if self.fatal {
                                SharedString::from(
                                    "Resolve the recovery paths or close this dialog.",
                                )
                            } else if self.checking {
                                SharedString::from("Checking the full plan…")
                            } else {
                                let issues =
                                    self.plan.as_ref().map_or(0, |plan| plan.issue_count());
                                if issues > 0 {
                                    SharedString::from(format!(
                                        "{issues} conflicts block this batch"
                                    ))
                                } else {
                                    SharedString::from(format!(
                                        "{changed} of {count} items will be renamed"
                                    ))
                                }
                            }),
                    )
                    .child(
                        Button::new("batch-cancel")
                            .outline()
                            .label("Cancel")
                            .disabled(applying || self.fatal)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("batch-apply")
                            .primary()
                            .label(if applying {
                                "Renaming…"
                            } else {
                                "Rename items"
                            })
                            .disabled(!ready)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(Confirm { secondary: false }), cx)
                            }),
                    ),
            )
    }
}

mod orchestration;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_geometry_fits_inside_minimum_window() {
        let geometry = dialog_geometry(760., 480.);
        assert_eq!(geometry.body_width, 726.);
        assert_eq!(geometry.body_height, 430.);
        assert_eq!(geometry.margin_top, MIN_MARGIN_VERTICAL);
        assert!(geometry.body_width + DIALOG_BORDER + MIN_MARGIN_HORIZONTAL * 2. <= 760.);
        assert!(geometry.body_height + DIALOG_BORDER + MIN_MARGIN_VERTICAL * 2. <= 480.);
    }

    #[test]
    fn issue_messages_name_the_selected_paths() {
        let source = VfsPath::local("/tmp/a.txt");
        let target = VfsPath::local("/tmp/b.txt");
        let issue = ferret_ops::PlanIssue::TargetExists {
            source: source.clone(),
            target: target.clone(),
        }
        .to_string();
        assert!(issue.contains("/tmp/a.txt"), "{issue}");
        assert!(issue.contains("/tmp/b.txt"), "{issue}");
    }
}
