use libghostty_vt::render::{CellIterator, CursorVisualStyle, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::{RenderState, Terminal};

use super::TerminalEngine;
use crate::{
    CursorShape, Result, TerminalColor, TerminalCursor, TerminalError, TerminalRow, TerminalRun,
    TerminalSnapshot, TerminalStyle,
};

pub(super) struct GhosttyEngine {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    generation: u64,
}

impl GhosttyEngine {
    pub(super) fn new(columns: u16, rows: u16, scrollback_lines: usize) -> Result<Self> {
        let mut terminal =
            Terminal::new(columns, rows).map_err(|error| engine_error(error.to_string()))?;
        terminal
            .set_scrollback_max_lines(Some(scrollback_lines))
            .map_err(|error| engine_error(error.to_string()))?;
        Ok(Self {
            terminal,
            render_state: RenderState::new().map_err(|error| engine_error(error.to_string()))?,
            rows: RowIterator::new().map_err(|error| engine_error(error.to_string()))?,
            cells: CellIterator::new().map_err(|error| engine_error(error.to_string()))?,
            generation: 0,
        })
    }
}

impl TerminalEngine for GhosttyEngine {
    fn advance(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
        self.generation = self.generation.wrapping_add(1);
    }

    fn resize(&mut self, columns: u16, rows: u16, cell_width: u32, cell_height: u32) -> Result<()> {
        self.terminal
            .resize(columns, rows, cell_width, cell_height)
            .map_err(|error| engine_error(error.to_string()))?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    fn snapshot(&mut self) -> Result<TerminalSnapshot> {
        let snapshot = self
            .render_state
            .update(&self.terminal)
            .map_err(|error| engine_error(error.to_string()))?;
        let visible = snapshot
            .cursor_visible()
            .map_err(|error| engine_error(error.to_string()))?;
        let position = snapshot
            .cursor_viewport()
            .map_err(|error| engine_error(error.to_string()))?;
        let shape = snapshot
            .cursor_visual_style()
            .map_err(|error| engine_error(error.to_string()))?;
        let mut rows = Vec::new();
        let mut row_iter = self
            .rows
            .update(&snapshot)
            .map_err(|error| engine_error(error.to_string()))?;
        while let Some(row) = row_iter.next() {
            let mut runs: Vec<TerminalRun> = Vec::new();
            let mut cell_iter = self
                .cells
                .update(row)
                .map_err(|error| engine_error(error.to_string()))?;
            while let Some(cell) = cell_iter.next() {
                let mut text = String::new();
                cell.graphemes_utf8(&mut text)
                    .map_err(|error| engine_error(error.to_string()))?;
                let wide = cell
                    .raw_cell()
                    .and_then(|cell| cell.wide())
                    .map_err(|error| engine_error(error.to_string()))?;
                if text.is_empty() && !matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    text.push(' ');
                }
                let ghostty_style = cell
                    .style()
                    .map_err(|error| engine_error(error.to_string()))?;
                let style = TerminalStyle {
                    foreground: cell
                        .fg_color()
                        .map_err(|error| engine_error(error.to_string()))?
                        .map(rgb)
                        .unwrap_or(TerminalColor::Foreground),
                    background: cell
                        .bg_color()
                        .map_err(|error| engine_error(error.to_string()))?
                        .map(rgb)
                        .unwrap_or(TerminalColor::Background),
                    bold: ghostty_style.bold,
                    dim: ghostty_style.faint,
                    italic: ghostty_style.italic,
                    underline: ghostty_style.underline != Underline::None,
                    strikeout: ghostty_style.strikethrough,
                    inverse: ghostty_style.inverse,
                };
                if let Some(run) = runs.last_mut().filter(|run| run.style == style) {
                    run.text.push_str(&text);
                    run.columns += 1;
                } else {
                    runs.push(TerminalRun {
                        text,
                        columns: 1,
                        style,
                    });
                }
            }
            rows.push(TerminalRow { runs });
        }
        let (row, column) = position
            .map(|point| (point.y.into(), point.x.into()))
            .unwrap_or_default();
        Ok(TerminalSnapshot {
            rows,
            cursor: TerminalCursor {
                row,
                column,
                visible,
                shape: match shape {
                    CursorVisualStyle::Bar => CursorShape::Beam,
                    CursorVisualStyle::Block => CursorShape::Block,
                    CursorVisualStyle::Underline => CursorShape::Underline,
                    CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
                    _ => CursorShape::Block,
                },
            },
            generation: self.generation,
        })
    }
}

fn rgb(color: RgbColor) -> TerminalColor {
    TerminalColor::Rgb {
        red: color.r,
        green: color.g,
        blue: color.b,
    }
}

fn engine_error(message: String) -> TerminalError {
    TerminalError::InitializeEngine {
        backend: "Ghostty",
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn produces_owned_styled_snapshot() {
        let mut engine = GhosttyEngine::new(20, 3, 100).unwrap();
        engine.advance(b"hello\r\n\x1b[38;2;12;34;56mcolor\x1b[0m");
        let snapshot = engine.snapshot().unwrap();
        assert!(
            snapshot.rows[0]
                .runs
                .iter()
                .any(|run| run.text.contains("hello"))
        );
        assert!(snapshot.rows[1].runs.iter().any(|run| run.style.foreground
            == TerminalColor::Rgb {
                red: 12,
                green: 34,
                blue: 56
            }));
        assert!(snapshot.cursor.visible);
    }

    #[test]
    fn wide_cells_keep_grid_width_without_rendering_the_tail() {
        let mut engine = GhosttyEngine::new(6, 2, 100).unwrap();
        engine.advance("界x".as_bytes());
        let snapshot = engine.snapshot().unwrap();
        assert_eq!(
            snapshot.rows[0]
                .runs
                .iter()
                .map(|run| run.columns)
                .sum::<usize>(),
            6
        );
        let text: String = snapshot.rows[0]
            .runs
            .iter()
            .map(|run| run.text.as_str())
            .collect();
        assert!(text.starts_with("界x"), "{text:?}");
    }
}
