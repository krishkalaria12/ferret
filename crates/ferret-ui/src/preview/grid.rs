//! The CSV/TSV grid (F-35): a virtualised table over the bounded sample.
//!
//! Parsed with the `csv` crate upstream in `ferret-preview`; this module owns
//! *how to draw the sample*: one virtualised `DataTable` whose delegate holds
//! the rows as shared strings, so a visible cell is one `Arc` clone, never a
//! re-parse. Column sorting is offered only within what has been loaded, and
//! says so — the footer always reads "sample", and sorting never pretends to
//! order a file it has not read.
//!
//! Header inference is a reversible toggle: when the parser detected a header
//! it labels the columns and the toggle puts it back as the first row under
//! `Column 1…N` labels instead. Ragged records (fewer or more fields than the
//! widest row) render their missing cells as an em dash and are counted in
//! the footer — a preview that hides the broken row is worse than useless
//! for the person trying to find it.

use super::*;
use gpui::{
    App, AppContext, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window,
    div, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};

/// A grid over one parsed sample, with its sort and header state.
pub(crate) struct CsvGrid {
    pub table: Entity<TableState<CsvDelegate>>,
}

impl CsvGrid {
    pub fn new(
        parsed: &ferret_preview::ParsedTable,
        widths: Vec<f32>,
        window: &mut Window,
        cx: &mut impl AppContext,
    ) -> Self {
        let delegate = CsvDelegate::new(parsed, widths);
        let table = cx.new(|cx| {
            TableState::new(delegate, window, cx)
                .sortable(true)
                .row_selectable(false)
                .col_selectable(false)
        });
        Self { table }
    }

    pub fn element(&self) -> DataTable<CsvDelegate> {
        DataTable::new(&self.table)
    }

    /// Whether the inferred-header toggle exists, and whether it is on.
    ///
    /// `None` means the menu item stays hidden rather than disabled: without
    /// a detected header there is nothing to toggle.
    pub fn header_state(&self, cx: &App) -> Option<bool> {
        let delegate = self.table.read(cx);
        delegate
            .delegate()
            .has_header_toggle()
            .then(|| delegate.delegate().header_on())
    }

    /// The background-computed column widths, for the overlay to reuse
    /// without remeasuring O(the sample) on the window thread.
    pub fn widths(&self, cx: &App) -> Vec<f32> {
        self.table.read(cx).delegate().widths.clone()
    }
}

impl PreviewPanel {
    /// Flip between the inferred header and the first row as data.
    ///
    /// Reversible, sample-local, and silent about the file it has not read:
    /// toggling never re-parses, never re-reads, never re-sorts.
    pub(crate) fn toggle_csv_header(&mut self, cx: &mut Context<Self>) {
        if let PaneState::Table { grid, .. } = &mut self.state {
            grid.table.update(cx, |table, cx| {
                if table.delegate_mut().toggle_header() {
                    cx.notify();
                }
            });
        }
    }
}

pub(crate) struct CsvDelegate {
    labels: Vec<SharedString>,
    rows: Vec<Vec<SharedString>>,
    /// The display rows before the current sort: `ColumnSort::Default`
    /// restores these rather than re-reading anything.
    base_rows: Vec<Vec<SharedString>>,
    /// The parser-detected header, when there is one. `None` means the
    /// toggle does not exist for this file.
    inferred: Option<Vec<SharedString>>,
    header_on: bool,
    sort: Option<(usize, bool)>,
    widths: Vec<f32>,
}

impl CsvDelegate {
    fn new(parsed: &ferret_preview::ParsedTable, widths: Vec<f32>) -> Self {
        let width = column_width(parsed);
        let mut labels: Vec<SharedString> = match &parsed.header {
            Some(header) => header
                .iter()
                .map(|cell| SharedString::from(cell.clone()))
                .collect(),
            None => default_labels(width),
        };
        // A short header must not narrow the grid below the widest data
        // row: pad with positional labels so every cell has a column.
        for index in labels.len() + 1..=width {
            labels.push(SharedString::from(format!("Column {index}")));
        }
        let rows: Vec<Vec<SharedString>> = parsed
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| SharedString::from(cell.clone()))
                    .collect()
            })
            .collect();
        let inferred = parsed.header.as_ref().map(|header| {
            header
                .iter()
                .map(|cell| SharedString::from(cell.clone()))
                .collect()
        });
        Self {
            labels,
            base_rows: rows.clone(),
            rows,
            inferred,
            header_on: parsed.header.is_some(),
            sort: None,
            widths,
        }
    }

    /// Flip between the inferred header and the first row as data.
    ///
    /// Returns whether anything changed: without a detected header there is
    /// no toggle, and the menu item stays hidden rather than disabled.
    pub fn toggle_header(&mut self) -> bool {
        let Some(inferred) = self.inferred.clone() else {
            return false;
        };
        self.header_on = !self.header_on;
        if self.header_on {
            // Restore the padded labels: a short header must not narrow the
            // grid below the widest data row on the way back on.
            let width = self
                .base_rows
                .iter()
                .map(Vec::len)
                .fold(inferred.len(), usize::max);
            let mut labels = inferred;
            for index in labels.len() + 1..=width {
                labels.push(SharedString::from(format!("Column {index}")));
            }
            self.labels = labels;
            // The header row leaves the data: it labels, not doubles.
            if self.base_rows.first().is_some_and(|first| {
                first.len() == self.inferred.as_ref().map(Vec::len).unwrap_or(0)
                    && first.iter().zip(self.labels.iter()).all(|(a, b)| a == b)
            }) {
                self.base_rows.remove(0);
            }
        } else {
            self.labels = default_labels(self.width());
            // ...and comes back as the first row when toggled off.
            self.base_rows.insert(0, self.labels_as_row());
        }
        self.rows = self.base_rows.clone();
        self.sort = None;
        true
    }

    /// The current labels as a data row, for the toggled-off shape.
    fn labels_as_row(&self) -> Vec<SharedString> {
        // `inferred` is `Some` whenever this runs: the toggle exits early
        // without one, so falling back to the labels is unreachable.
        self.inferred.clone().unwrap_or_else(|| self.labels.clone())
    }

    fn width(&self) -> usize {
        self.labels.len()
    }

    /// Whether the header toggle exists for this file.
    pub fn has_header_toggle(&self) -> bool {
        self.inferred.is_some()
    }

    /// Whether the inferred header is currently labelling the columns.
    pub fn header_on(&self) -> bool {
        self.header_on
    }
}

fn default_labels(width: usize) -> Vec<SharedString> {
    (1..=width)
        .map(|index| SharedString::from(format!("Column {index}")))
        .collect()
}

/// The grid width: the widest row wins, so ragged records fit rather than
/// overflow. Zero for a file with no rows and no header at all.
fn column_width(parsed: &ferret_preview::ParsedTable) -> usize {
    let header = parsed.header.as_ref().map(Vec::len).unwrap_or(0);
    parsed.rows.iter().map(Vec::len).fold(header, usize::max)
}

impl TableDelegate for CsvDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.width()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        let sort = match self.sort {
            Some((col, true)) if col == col_ix => ColumnSort::Ascending,
            Some((col, false)) if col == col_ix => ColumnSort::Descending,
            _ => ColumnSort::Default,
        };
        Column::new(format!("csv-col-{col_ix}"), self.labels[col_ix].clone())
            .sortable()
            .sort(sort)
            .width(px(self.widths.get(col_ix).copied().unwrap_or(120.)))
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        match sort {
            ColumnSort::Ascending => {
                ferret_preview::table::sort_rows(&mut self.rows, col_ix, true);
                self.sort = Some((col_ix, true));
            }
            ColumnSort::Descending => {
                ferret_preview::table::sort_rows(&mut self.rows, col_ix, false);
                self.sort = Some((col_ix, false));
            }
            // Back to the loaded order: the sample on disk did not move.
            ColumnSort::Default => {
                self.rows = self.base_rows.clone();
                self.sort = None;
            }
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = self
            .rows
            .get(row_ix)
            .and_then(|row| row.get(col_ix))
            .cloned();
        match cell {
            Some(text) => div()
                .px(px(8.))
                .py(px(3.))
                .text_size(px(12.5))
                .text_color(cx.theme().foreground)
                .child(text)
                .into_any_element(),
            // A ragged record's missing field: visible, counted in the
            // footer, never silently padded with the neighbour's data.
            None => div()
                .px(px(8.))
                .py(px(3.))
                .text_size(px(12.5))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from("—"))
                .into_any_element(),
        }
    }
}

/// Column widths in logical pixels, from the sample's longest cells.
///
/// A character-count heuristic, not a measurement: measuring text runs
///Report for a bounded sample on the window thread. Computed once per load
/// on the background task, clamped so one pathological field cannot evict
/// every other column.
pub(crate) fn column_widths(table: &ferret_preview::ParsedTable) -> Vec<f32> {
    let width = column_width(table);
    let mut longest = vec![0usize; width];
    let mut consider = |row: &[String]| {
        for (index, cell) in row.iter().enumerate().take(width) {
            longest[index] = longest[index].max(cell.chars().count());
        }
    };
    if let Some(header) = &table.header {
        consider(header);
    }
    for row in &table.rows {
        consider(row);
    }
    longest
        .into_iter()
        .map(|chars| (chars as f32 * 7.5 + 24.).clamp(64., 320.))
        .collect()
}

/// How many records disagree with the grid width — the footer's ragged note.
pub(crate) fn ragged_count(table: &ferret_preview::ParsedTable) -> usize {
    let width = column_width(table);
    table.rows.iter().filter(|row| row.len() != width).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str, delimiter: u8) -> ferret_preview::ParsedTable {
        ferret_preview::table::parse(text, delimiter)
    }

    #[test]
    fn headers_label_columns_and_toggle_restores_the_row() {
        let table = parsed("name,age\namy,30\nbo,41\n", b',');
        let mut delegate = CsvDelegate::new(&table, vec![100., 100.]);
        assert_eq!(delegate.labels[0].to_string(), "name");
        assert_eq!(delegate.rows.len(), 2);
        assert!(delegate.has_header_toggle());

        assert!(delegate.toggle_header());
        assert!(!delegate.header_on());
        assert_eq!(delegate.labels[0].to_string(), "Column 1");
        assert_eq!(delegate.rows.len(), 3, "the header is data again");
        assert_eq!(delegate.rows[0][0].to_string(), "name");

        assert!(delegate.toggle_header());
        assert!(delegate.header_on());
        assert_eq!(delegate.rows.len(), 2, "labels, not doubles");
    }

    #[test]
    fn without_a_detected_header_there_is_no_toggle() {
        let table = parsed("1,2\n3,4\n", b',');
        let mut delegate = CsvDelegate::new(&table, vec![100., 100.]);
        assert!(!delegate.has_header_toggle());
        assert!(!delegate.toggle_header());
        assert_eq!(delegate.labels[0].to_string(), "Column 1");
    }

    #[test]
    fn ragged_records_fit_the_widest_row() {
        let table = parsed("a,b,c\n1,2\n3,4,5,6\n", b',');
        assert_eq!(column_width(&table), 4);
        // Only data rows count: the short header labels three columns and
        // the fourth is padded, while the two-field record is the ragged one.
        assert_eq!(ragged_count(&table), 1);
        let delegate = CsvDelegate::new(&table, vec![100., 100., 100., 100.]);
        assert_eq!(delegate.labels.len(), 4);
        assert_eq!(delegate.labels[3].to_string(), "Column 4");
    }

    #[test]
    fn empty_files_have_no_grid() {
        let table = parsed("", b',');
        assert_eq!(column_width(&table), 0);
    }

    #[test]
    fn one_long_field_cannot_evict_the_other_columns() {
        let huge = "x".repeat(10_000);
        let table = parsed(&format!("a,b\n{huge},2\n"), b',');
        let widths = column_widths(&table);
        // The pathological column clamps instead of growing without bound…
        assert_eq!(widths[0], 320.);
        // …while the ordinary column keeps its minimum rather than the mean.
        assert_eq!(widths[1], 64.);
    }
}
