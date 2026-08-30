//! Terminal emulation and PTY ownership for Ferret.
//!
//! UI code consumes owned [`TerminalSnapshot`] values. The terminal engine is
//! deliberately private so replacing it cannot leak third-party types across
//! the service boundary (D-23).

mod engine;
mod errors;
mod session;

pub use errors::{Result, TerminalError};
pub use session::TerminalSession;

use std::path::PathBuf;

/// Runtime options supplied by the settings crate without coupling the two
/// service crates together.
#[derive(Clone, Debug)]
pub struct TerminalOptions {
    pub shell: Option<PathBuf>,
    pub term: String,
    pub scrollback_lines: usize,
    pub backend: TerminalBackend,
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self {
            shell: None,
            term: "xterm-256color".into(),
            scrollback_lines: 10_000,
            backend: TerminalBackend::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalBackend {
    #[default]
    Auto,
    Ghostty,
    Alacritty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveTerminalBackend {
    Ghostty,
    Alacritty,
}

/// An owned view of the visible terminal grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub rows: Vec<TerminalRow>,
    pub cursor: TerminalCursor,
    pub application_cursor_keys: bool,
    pub generation: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalRow {
    pub runs: Vec<TerminalRun>,
}

/// Consecutive cells with identical presentation. `columns` is separate from
/// the text length because wide glyphs and combining marks occupy a terminal
/// grid differently from UTF-8 bytes or Unicode scalar values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRun {
    pub text: String,
    pub columns: usize,
    pub style: TerminalStyle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalStyle {
    pub foreground: TerminalColor,
    pub background: TerminalColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    pub inverse: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalColor {
    #[default]
    Foreground,
    Background,
    Cursor,
    Named(u8),
    Indexed(u8),
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalCursor {
    pub row: usize,
    pub column: usize,
    pub shape: CursorShape,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
    HollowBlock,
}
