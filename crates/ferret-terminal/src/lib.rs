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

/// An owned view of the visible terminal grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_column: usize,
    pub generation: u64,
}
