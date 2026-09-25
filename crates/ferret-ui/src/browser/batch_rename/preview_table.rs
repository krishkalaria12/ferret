use std::sync::Arc;

use ferret_ops::PlanIssue;
use gpui::{
    App, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use gpui_component::table::{Column, TableDelegate, TableState};
use gpui_component::{ActiveTheme as _, v_flex};

pub(super) const PREVIEW_ROW_HEIGHT: f32 = 34.;

#[derive(Default)]
pub(super) struct RenamePreviewDelegate {
    pub(super) rows: Arc<[PreviewDisplayRow]>,
}

pub(super) enum PreviewStatus {
    Ready,
    Unchanged,
    Issue(PlanIssue),
}

pub(super) struct PreviewDisplayRow {
    pub(super) old_name: SharedString,
    pub(super) new_name: SharedString,
    pub(super) status: PreviewStatus,
    pub(super) issue: bool,
    pub(super) changed: bool,
}

impl TableDelegate for RenamePreviewDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        3
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, index: usize, _cx: &App) -> Column {
        match index {
            0 => Column::new("batch-original", "Original name").width(px(285.)),
            1 => Column::new("batch-renamed", "New name").width(px(285.)),
            _ => Column::new("batch-status", "Status").width(px(250.)),
        }
    }

    fn render_td(
        &mut self,
        row_index: usize,
        column_index: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.rows.get(row_index) else {
            return div().into_any_element();
        };
        let (text, color) = match column_index {
            0 => (row.old_name.clone(), cx.theme().foreground),
            1 => (
                row.new_name.clone(),
                if row.issue {
                    cx.theme().danger
                } else if row.changed {
                    cx.theme().success
                } else {
                    cx.theme().muted_foreground
                },
            ),
            _ => {
                let status = match &row.status {
                    PreviewStatus::Ready => "Ready".to_string(),
                    PreviewStatus::Unchanged => "Unchanged".to_string(),
                    PreviewStatus::Issue(issue) => issue.to_string(),
                };
                (
                    SharedString::from(status),
                    if row.issue {
                        cx.theme().danger
                    } else if row.changed {
                        cx.theme().success
                    } else {
                        cx.theme().muted_foreground
                    },
                )
            }
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .px_3()
            .truncate()
            .text_size(px(12.5))
            .text_color(color)
            .child(text)
            .into_any_element()
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child("Add a rule to preview the selected items")
            .into_any_element()
    }
}
