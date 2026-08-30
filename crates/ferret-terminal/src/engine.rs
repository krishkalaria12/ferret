//! The only module allowed to name the terminal emulation dependency (D-23).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, CursorShape as AlacrittyCursorShape, NamedColor};

use crate::{
    CursorShape, TerminalColor, TerminalCursor, TerminalRow, TerminalRun, TerminalSnapshot,
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

pub(crate) struct Engine {
    parser: alacritty_terminal::vte::ansi::Processor,
    term: Term<VoidListener>,
    generation: u64,
    columns: usize,
    rows: usize,
}

impl Engine {
    pub(crate) fn new(columns: usize, rows: usize, scrollback_lines: usize) -> Self {
        let size = Size { columns, rows };
        let config = Config {
            scrolling_history: scrollback_lines,
            ..Config::default()
        };
        Self {
            parser: alacritty_terminal::vte::ansi::Processor::new(),
            term: Term::new(config, &size, VoidListener),
            generation: 0,
            columns,
            rows,
        }
    }

    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn resize(&mut self, columns: usize, rows: usize) {
        self.columns = columns;
        self.rows = rows;
        self.term.resize(Size { columns, rows });
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn snapshot(&self) -> TerminalSnapshot {
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
        let rows = cells
            .into_iter()
            .map(|cells| {
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
            })
            .collect();
        let Line(cursor_row) = content.cursor.point.line;
        let Column(cursor_column) = content.cursor.point.column;
        TerminalSnapshot {
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
            generation: self.generation,
        }
    }
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
            // Semantic variants were handled above.
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
    fn parses_text_and_ansi_without_leaking_escape_bytes() {
        let mut engine = Engine::new(20, 3, 100);
        engine.advance(b"hello\r\n\x1b[31mred\x1b[0m");
        let snapshot = engine.snapshot();
        assert!(row_text(&snapshot.rows[0]).starts_with("hello"));
        let red = snapshot.rows[1]
            .runs
            .iter()
            .find(|run| run.text.starts_with("red"))
            .expect("red run");
        assert_eq!(red.style.foreground, TerminalColor::Named(1));
    }

    #[test]
    fn preserves_truecolor_cursor_movement_and_private_use_glyphs() {
        let mut engine = Engine::new(20, 3, 100);
        engine.advance("ab\u{e0b0}\x1b[2D\x1b[38;2;12;34;56mZ".as_bytes());
        let snapshot = engine.snapshot();
        let text = row_text(&snapshot.rows[0]);
        assert!(text.starts_with("aZ\u{e0b0}"), "{text:?}");
        let styled = snapshot.rows[0]
            .runs
            .iter()
            .find(|run| run.text.starts_with('Z'))
            .expect("truecolor run");
        assert_eq!(
            styled.style.foreground,
            TerminalColor::Rgb {
                red: 12,
                green: 34,
                blue: 56,
            }
        );
        assert_eq!(snapshot.cursor.column, 2);
        assert!(snapshot.cursor.visible);
    }
}
