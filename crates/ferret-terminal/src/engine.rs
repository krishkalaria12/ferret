//! The only module allowed to name the terminal emulation dependency (D-23).

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};

use crate::TerminalSnapshot;

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
    pub(crate) fn new(columns: usize, rows: usize) -> Self {
        let size = Size { columns, rows };
        Self {
            parser: alacritty_terminal::vte::ansi::Processor::new(),
            term: Term::new(Config::default(), &size, VoidListener),
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
        let mut rows = vec![vec![' '; self.columns]; self.rows];
        for indexed in content.display_iter {
            let Line(line) = indexed.point.line;
            let Column(column) = indexed.point.column;
            if line >= 0 && (line as usize) < rows.len() && column < self.columns {
                rows[line as usize][column] = indexed.cell.c;
            }
        }
        let lines = rows
            .into_iter()
            .map(|line| line.into_iter().collect::<String>().trim_end().to_owned())
            .collect();
        let Line(cursor_row) = content.cursor.point.line;
        let Column(cursor_column) = content.cursor.point.column;
        TerminalSnapshot {
            lines,
            cursor_row: cursor_row.max(0) as usize,
            cursor_column,
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_ansi_without_leaking_escape_bytes() {
        let mut engine = Engine::new(20, 3);
        engine.advance(b"hello\r\n\x1b[31mred\x1b[0m");
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.lines[0], "hello");
        assert_eq!(snapshot.lines[1], "red");
    }
}
