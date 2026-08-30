//! Engine-neutral terminal state boundary (D-23).

mod alacritty;
#[cfg(feature = "ghostty")]
mod ghostty;

#[cfg(not(feature = "ghostty"))]
use crate::TerminalError;
use crate::{ActiveTerminalBackend, Result, TerminalBackend, TerminalSnapshot};

pub(super) trait TerminalEngine {
    /// Feed PTY output and return any bytes the terminal protocol requires us
    /// to write back to the PTY.
    fn advance(&mut self, bytes: &[u8]) -> Vec<Vec<u8>>;
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

    #[cfg(feature = "ghostty")]
    #[test]
    fn backends_agree_on_core_vt_corpus() {
        let corpus: &[&[u8]] = &[
            b"plain text\r\nsecond line",
            "wide: 界 emoji: 🙂\r\n".as_bytes(),
            b"\x1b[31;1mred bold\x1b[0m \x1b[38;2;12;34;56mtruecolor\x1b[0m",
            b"abc\x1b[2DZ\x1b[?1h",
            b"\x1b[?1049halternate\x1b[?1049lprimary",
            b"\x1b[?1049hstill in vim",
            b"\x1b[?2004hbracketed",
        ];
        for bytes in corpus {
            let mut alacritty = alacritty::AlacrittyEngine::new(40, 4, 100);
            let mut ghostty = ghostty::GhosttyEngine::new(40, 4, 100).unwrap();
            alacritty.advance(bytes);
            ghostty.advance(bytes);
            let alacritty = alacritty.snapshot().unwrap();
            let ghostty = ghostty.snapshot().unwrap();
            assert_eq!(
                visible_text(&alacritty),
                visible_text(&ghostty),
                "input: {bytes:?}"
            );
            assert_eq!(alacritty.cursor, ghostty.cursor, "input: {bytes:?}");
            assert_eq!(
                alacritty.application_cursor_keys, ghostty.application_cursor_keys,
                "input: {bytes:?}"
            );
            // Directory sync reads both of these before it writes into
            // someone's shell, so the two engines have to agree on them.
            assert_eq!(
                alacritty.alternate_screen, ghostty.alternate_screen,
                "input: {bytes:?}"
            );
            assert_eq!(
                alacritty.bracketed_paste, ghostty.bracketed_paste,
                "input: {bytes:?}"
            );
        }
    }

    #[cfg(feature = "ghostty")]
    fn visible_text(snapshot: &TerminalSnapshot) -> Vec<String> {
        snapshot
            .rows
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }
}
