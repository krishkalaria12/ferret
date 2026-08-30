//! Engine-neutral terminal state boundary (D-23).

mod alacritty;
#[cfg(feature = "ghostty")]
mod ghostty;

#[cfg(not(feature = "ghostty"))]
use crate::TerminalError;
use crate::{ActiveTerminalBackend, Result, TerminalBackend, TerminalSnapshot};

pub(super) trait TerminalEngine {
    fn advance(&mut self, bytes: &[u8]);
    fn resize(&mut self, columns: u16, rows: u16, cell_width: u32, cell_height: u32) -> Result<()>;
    fn snapshot(&mut self) -> Result<TerminalSnapshot>;
}

pub(super) fn select_backend(requested: TerminalBackend) -> Result<ActiveTerminalBackend> {
    match requested {
        TerminalBackend::Alacritty => Ok(ActiveTerminalBackend::Alacritty),
        TerminalBackend::Auto => {
            #[cfg(feature = "ghostty")]
            return Ok(ActiveTerminalBackend::Ghostty);
            #[cfg(not(feature = "ghostty"))]
            Ok(ActiveTerminalBackend::Alacritty)
        }
        TerminalBackend::Ghostty => {
            #[cfg(feature = "ghostty")]
            return Ok(ActiveTerminalBackend::Ghostty);
            #[cfg(not(feature = "ghostty"))]
            Err(TerminalError::InitializeEngine {
                backend: "Ghostty",
                message: "this Ferret build does not include the `ghostty` feature".into(),
            })
        }
    }
}

pub(super) fn create(
    backend: ActiveTerminalBackend,
    columns: u16,
    rows: u16,
    scrollback_lines: usize,
) -> Result<Box<dyn TerminalEngine>> {
    match backend {
        ActiveTerminalBackend::Alacritty => Ok(Box::new(alacritty::AlacrittyEngine::new(
            columns,
            rows,
            scrollback_lines,
        ))),
        ActiveTerminalBackend::Ghostty => {
            #[cfg(feature = "ghostty")]
            return ghostty::GhosttyEngine::new(columns, rows, scrollback_lines)
                .map(|engine| Box::new(engine) as Box<dyn TerminalEngine>);
            #[cfg(not(feature = "ghostty"))]
            Err(TerminalError::InitializeEngine {
                backend: "Ghostty",
                message: "this Ferret build does not include the `ghostty` feature".into(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_alacritty_is_always_available() {
        assert_eq!(
            select_backend(TerminalBackend::Alacritty).unwrap(),
            ActiveTerminalBackend::Alacritty
        );
    }

    #[cfg(not(feature = "ghostty"))]
    #[test]
    fn auto_falls_back_when_ghostty_is_not_compiled() {
        assert_eq!(
            select_backend(TerminalBackend::Auto).unwrap(),
            ActiveTerminalBackend::Alacritty
        );
        assert!(select_backend(TerminalBackend::Ghostty).is_err());
    }
}
