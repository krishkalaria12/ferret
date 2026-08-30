use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape as AlacrittyCursorShape, NamedColor};

use super::TerminalEngine;
use crate::{
    CursorShape, Result, TerminalColor, TerminalCursor, TerminalRow, TerminalRun, TerminalSnapshot,
    TerminalStyle,
};

struct Size {
    columns: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

pub(super) struct AlacrittyEngine {
    parser: alacritty_terminal::vte::ansi::Processor,
    term: Term<AlacrittyListener>,
    events: Rc<RefCell<Vec<Event>>>,
    generation: u64,
    columns: usize,
    rows: usize,
    cell_width: u16,
    cell_height: u16,
}

impl AlacrittyEngine {
    pub(super) fn new(columns: u16, rows: u16, scrollback_lines: usize) -> Self {
        let size = Size {
            columns: columns.into(),
            rows: rows.into(),
        };
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        let events = Rc::new(RefCell::new(Vec::new()));
        let listener = AlacrittyListener {
            events: events.clone(),
        };
        Self {
            parser: alacritty_terminal::vte::ansi::Processor::new(),
            term: Term::new(config, &size, listener),
            events,
            generation: 0,
            columns: columns.into(),
            rows: rows.into(),
            cell_width: 0,
            cell_height: 0,
        }
    }
}

impl TerminalEngine for AlacrittyEngine {
    fn advance(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.parser.advance(&mut self.term, bytes);
        self.generation = self.generation.wrapping_add(1);
        let events: Vec<_> = self.events.borrow_mut().drain(..).collect();
        events
            .into_iter()
            .filter_map(|event| match event {
                Event::PtyWrite(text) => Some(text.into_bytes()),
                Event::ColorRequest(index, format) => {
                    self.term.colors()[index].map(|color| format(color).into_bytes())
                }
                Event::TextAreaSizeRequest(format) => Some(
                    format(WindowSize {
                        num_lines: self.rows.min(u16::MAX as usize) as u16,
                        num_cols: self.columns.min(u16::MAX as usize) as u16,
                        cell_width: self.cell_width,
                        cell_height: self.cell_height,
                    })
                    .into_bytes(),
                ),
                _ => None,
            })
            .collect()
    }

    fn resize(&mut self, columns: u16, rows: u16, cell_width: u32, cell_height: u32) -> Result<()> {
        self.columns = columns.into();
        self.rows = rows.into();
        self.cell_width = cell_width.min(u32::from(u16::MAX)) as u16;
        self.cell_height = cell_height.min(u32::from(u16::MAX)) as u16;
        self.term.resize(Size {
            columns: self.columns,
            rows: self.rows,
        });
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    fn snapshot(&mut self) -> Result<TerminalSnapshot> {
        let application_cursor_keys = self.term.mode().contains(TermMode::APP_CURSOR);
        let content = self.term.renderable_content();
        let default_style = TerminalStyle {
            foreground: TerminalColor::Foreground,
            background: TerminalColor::Background,
            ..TerminalStyle::default()
        };
        let mut cells = vec![vec![(String::from(" "), default_style); self.columns]; self.rows];
        for indexed in content.display_iter {
            let Line(line) = indexed.point.line;
            let Column(column) = indexed.point.column;
            if line >= 0 && (line as usize) < cells.len() && column < self.columns {
                let flags = indexed.cell.flags;
                let mut text = if flags.intersects(
                    Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER | Flags::HIDDEN,
                ) {
                    String::new()
                } else {
                    indexed.cell.c.to_string()
                };
                if let Some(zerowidth) = indexed.cell.zerowidth() {
                    text.extend(zerowidth);
                }
                cells[line as usize][column] = (
                    text,
                    TerminalStyle {
                        foreground: terminal_color(indexed.cell.fg),
                        background: terminal_color(indexed.cell.bg),
                        bold: flags.contains(Flags::BOLD),
                        dim: flags.contains(Flags::DIM),
                        italic: flags.contains(Flags::ITALIC),
                        underline: flags.intersects(Flags::ALL_UNDERLINES),
                        strikeout: flags.contains(Flags::STRIKEOUT),
                        inverse: flags.contains(Flags::INVERSE),
                    },
                );
            }
        }
        let rows = cells.into_iter().map(collapse_runs).collect();
        let Line(cursor_row) = content.cursor.point.line;
        let Column(cursor_column) = content.cursor.point.column;
        Ok(TerminalSnapshot {
            rows,
            cursor: TerminalCursor {
                row: cursor_row.max(0) as usize,
                column: cursor_column,
                shape: match content.cursor.shape {
                    AlacrittyCursorShape::Block => CursorShape::Block,
                    AlacrittyCursorShape::Underline => CursorShape::Underline,
                    AlacrittyCursorShape::Beam => CursorShape::Beam,
                    AlacrittyCursorShape::HollowBlock => CursorShape::HollowBlock,
                    AlacrittyCursorShape::Hidden => CursorShape::Block,
                },
                visible: content.cursor.shape != AlacrittyCursorShape::Hidden,
            },
            application_cursor_keys,
            generation: self.generation,
        })
    }
}

#[derive(Clone)]
struct AlacrittyListener {
    events: Rc<RefCell<Vec<Event>>>,
}

impl EventListener for AlacrittyListener {
    fn send_event(&self, event: Event) {
        self.events.borrow_mut().push(event);
    }
}

fn collapse_runs(cells: Vec<(String, TerminalStyle)>) -> TerminalRow {
    let mut runs: Vec<TerminalRun> = Vec::new();
    for (text, style) in cells {
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
    TerminalRow { runs }
}

fn terminal_color(color: Color) -> TerminalColor {
    match color {
        Color::Spec(rgb) => TerminalColor::Rgb {
            red: rgb.r,
            green: rgb.g,
            blue: rgb.b,
        },
        Color::Indexed(index) => TerminalColor::Indexed(index),
        Color::Named(
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground,
        ) => TerminalColor::Foreground,
        Color::Named(NamedColor::Background) => TerminalColor::Background,
        Color::Named(NamedColor::Cursor) => TerminalColor::Cursor,
        Color::Named(named) => TerminalColor::Named(match named {
            NamedColor::Black | NamedColor::DimBlack => 0,
            NamedColor::Red | NamedColor::DimRed => 1,
            NamedColor::Green | NamedColor::DimGreen => 2,
            NamedColor::Yellow | NamedColor::DimYellow => 3,
            NamedColor::Blue | NamedColor::DimBlue => 4,
            NamedColor::Magenta | NamedColor::DimMagenta => 5,
            NamedColor::Cyan | NamedColor::DimCyan => 6,
            NamedColor::White | NamedColor::DimWhite => 7,
            NamedColor::BrightBlack => 8,
            NamedColor::BrightRed => 9,
            NamedColor::BrightGreen => 10,
            NamedColor::BrightYellow => 11,
            NamedColor::BrightBlue => 12,
            NamedColor::BrightMagenta => 13,
            NamedColor::BrightCyan => 14,
            NamedColor::BrightWhite => 15,
            _ => 7,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row_text(row: &TerminalRow) -> String {
        row.runs.iter().map(|run| run.text.as_str()).collect()
    }
    #[test]
    fn parses_styles_and_cursor() {
        let mut engine = AlacrittyEngine::new(20, 3, 100);
        engine.advance(b"hello\r\n\x1b[31mred\x1b[0m");
        let snapshot = engine.snapshot().unwrap();
        assert!(row_text(&snapshot.rows[0]).starts_with("hello"));
        assert_eq!(
            snapshot.rows[1]
                .runs
                .iter()
                .find(|run| run.text.starts_with("red"))
                .unwrap()
                .style
                .foreground,
            TerminalColor::Named(1)
        );
        assert!(snapshot.cursor.visible);
    }

    #[test]
    fn terminal_queries_are_returned_to_the_pty() {
        let mut engine = AlacrittyEngine::new(20, 3, 100);
        let replies = engine.advance(b"\x1b[6n");
        assert_eq!(replies, [b"\x1b[1;1R".to_vec()]);
    }

    #[test]
    fn snapshot_tracks_application_cursor_mode() {
        let mut engine = AlacrittyEngine::new(20, 3, 100);
        engine.advance(b"\x1b[?1h");
        assert!(engine.snapshot().unwrap().application_cursor_keys);
        engine.advance(b"\x1b[?1l");
        assert!(!engine.snapshot().unwrap().application_cursor_keys);
    }

    #[test]
    fn text_area_queries_use_the_latest_grid_and_cell_size() {
        let mut engine = AlacrittyEngine::new(20, 3, 100);
        engine.resize(80, 24, 8, 16).unwrap();
        let replies = engine.advance(b"\x1b[14t");
        assert_eq!(replies, [b"\x1b[4;384;640t".to_vec()]);
    }
}
