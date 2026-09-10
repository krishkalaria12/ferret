//! What to show: classification, budgets, decoding and parsing for the
//! preview pane. GPUI-free on purpose.
//!
//! The crate seam falls between *what to show* (here: sniffing, budgets,
//! decoding, parsing, all testable without opening a window) and *how to
//! draw it* (`ferret-ui/src/preview/`). Only `ferret-ui` may depend on
//! `gpui-component`, and every renderer worth reusing is inside it.
//!
//! Two properties of [`Preview`] are load-bearing. It is `Send` and owns
//! everything it points at, so it crosses from a background task to the view
//! without borrowing anything that might have been dropped. And it carries no
//! GPUI type, which is what keeps this crate's tests running without a
//! window.
//!
//! Layout follows the repository conventions: the error type and [`Result`]
//! alias live in `errors.rs`, every tunable lives in [`config`].

pub mod classify;
pub mod config;
pub mod languages;
pub mod markdown;
pub mod table;
pub mod text;

mod errors;

pub use classify::Kind;
pub use errors::{PreviewError, Result};
pub use table::{ParsedTable, Row};

use ferret_vfs::VfsPath;
use smallvec::SmallVec;
use smol_str::SmolStr;

/// How much of the file the preview shows, in bytes seen and bytes total.
///
/// `shown == total` means the whole file. Anything else gets a footer line
/// naming both numbers — never a spinner that never ends, never a blank
/// pane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Truncation {
    pub shown: u64,
    pub total: u64,
}

impl Truncation {
    pub const NONE: Self = Self { shown: 0, total: 0 };

    pub fn whole(total: u64) -> Self {
        Self {
            shown: total,
            total,
        }
    }

    pub fn is_truncated(self) -> bool {
        self.shown < self.total
    }
}

/// Why a recognised file has no rendered body (yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// Past even the head budget for a kind that cannot be partially
    /// rendered (image, PDF). Never an attempted decode.
    TooLarge { size: u64, budget: usize },
    /// No provider in this build: image and PDF rendering, and the OS
    /// thumbnail service, land in later steps. Metadata and *Open with…*
    /// instead — a finished state, not a failure state.
    NoRenderer,
}

/// The value the background pipeline produces and `Render` only reads.
///
/// Carries source identity where the renderer needs it (Markdown's document
/// directory, which is what makes relative image links resolvable — and,
/// per PV-D10, resolvable only downward from there). Image and paged-PDF
/// bitmap variants arrive with their decoders in later steps; until then
/// those kinds map to [`Preview::Unsupported`].
#[derive(Clone, Debug)]
pub enum Preview {
    /// F-31, source form. Rendered Markdown arrives with the document view;
    /// until then the source previews as labelled text.
    Markdown {
        source: String,
        base: VfsPath,
        truncated: Truncation,
    },

    /// F-32. `language` is a registry name ("rust", "nix"), never an
    /// extension. `Plain` is a language, not an absence of one.
    Code {
        text: String,
        language: SmolStr,
        truncated: Truncation,
    },

    /// F-35. Header row detected, delimiter sniffed, rows bounded. The grid
    /// renderer arrives later; until then the sample previews as text.
    Table {
        table: ParsedTable,
        truncated: Truncation,
    },

    /// The floor. Hex plus printable ASCII for the head of the file.
    Binary {
        head: Vec<u8>,
        size: u64,
    },

    Directory {
        entries: Option<u64>,
    },
    Multiple {
        count: usize,
        bytes: u64,
        kinds: SmallVec<[(SmolStr, usize); 4]>,
    },
    Unsupported {
        kind: Kind,
        reason: Unsupported,
    },
}

/// The byte budget for a kind, scaled down on remote backends.
///
/// Every read is capped; every cap has a footer line when it bites. `remote`
/// is whether `capabilities_for(path)` reports a non-local backend — never
/// `capabilities()`, which is a property of the wrong thing.
pub fn budget_for(kind: &Kind, remote: bool) -> usize {
    let budget = match kind {
        Kind::Markdown => config::MARKDOWN_BUDGET,
        Kind::Code { .. } => config::TEXT_BUDGET,
        Kind::Csv | Kind::Tsv => config::TABLE_BUDGET,
        Kind::Image => config::IMAGE_BUDGET,
        Kind::Pdf => config::PDF_BUDGET,
        Kind::Binary => config::HEX_BUDGET,
    };
    if remote {
        config::scale_for_remote(budget)
    } else {
        budget
    }
}

/// Build the [`Preview`] for bytes already read under [`budget_for`].
///
/// `bytes` holds up to the budget; `total` is the full file length when the
/// listing already knows it (otherwise the bytes in hand). `base` is the
/// containing directory, required for Markdown and ignored otherwise.
/// Undecodable text falls back to the hex floor; undecodable hex is
/// impossible, so this only fails on programmer error (an empty kind map).
pub fn build(kind: Kind, bytes: Vec<u8>, total: Option<u64>, base: VfsPath) -> Preview {
    let total = total.unwrap_or(bytes.len() as u64);
    match kind {
        Kind::Markdown => {
            let shown = bytes.len() as u64;
            match text::decode(&bytes) {
                Some(decoded) => Preview::Markdown {
                    source: decoded.text,
                    base,
                    truncated: Truncation { shown, total },
                },
                None => Preview::Binary {
                    head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
                    size: total,
                },
            }
        }
        Kind::Code { language } => {
            let shown = bytes.len() as u64;
            match text::decode(&bytes) {
                Some(decoded) => Preview::Code {
                    text: decoded.text,
                    language,
                    truncated: Truncation { shown, total },
                },
                None => Preview::Binary {
                    head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
                    size: total,
                },
            }
        }
        Kind::Csv | Kind::Tsv => {
            let shown = bytes.len() as u64;
            match text::decode(&bytes) {
                Some(decoded) => {
                    // The delimiter is sniffed from the decoded sample, not
                    // trusted from the extension: European semicolon-delimited
                    // exports are the case the extension gets wrong.
                    let delimiter = table::sniff_delimiter(decoded.text.as_bytes());
                    let table = table::parse(&decoded.text, delimiter);
                    Preview::Table {
                        table,
                        truncated: Truncation { shown, total },
                    }
                }
                None => Preview::Binary {
                    head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
                    size: total,
                },
            }
        }
        Kind::Binary => Preview::Binary {
            head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
            size: total,
        },
        Kind::Image | Kind::Pdf => {
            let budget = budget_for(&kind, false);
            if total > budget as u64 {
                Preview::Unsupported {
                    kind,
                    reason: Unsupported::TooLarge {
                        size: total,
                        budget,
                    },
                }
            } else {
                Preview::Unsupported {
                    kind,
                    reason: Unsupported::NoRenderer,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::classify::classify;
    use super::*;

    fn build_named(name: &str, bytes: &[u8], size: Option<u64>) -> Preview {
        let sniff = &bytes[..bytes.len().min(config::SNIFF_BYTES)];
        let kind = classify(name, sniff, size);
        let budget = budget_for(&kind, false);
        let capped = bytes[..bytes.len().min(budget)].to_vec();
        build(kind, capped, size, VfsPath::local("/preview"))
    }

    #[test]
    fn source_builds_labelled_code() {
        let Preview::Code {
            text,
            language,
            truncated,
        } = build_named("main.rs", b"fn main() {}\n", Some(13))
        else {
            panic!("rust source must build code");
        };
        assert_eq!(text, "fn main() {}\n");
        assert_eq!(language.as_str(), "rust");
        assert!(!truncated.is_truncated());
    }

    #[test]
    fn a_mislabelled_binary_falls_to_the_hex_floor() {
        // The `.txt` says text; the NUL at byte 40 says otherwise, and
        // content gets the last word.
        let mut dump = vec![b'#'; 100];
        dump[40] = 0;
        let Preview::Binary { head, size } = build_named("core.txt", &dump, Some(100)) else {
            panic!("a NUL-bearing .txt must build binary");
        };
        assert_eq!(head.len(), 100);
        assert_eq!(size, 100);
    }

    #[test]
    fn undecodable_text_falls_back_to_hex_with_its_size() {
        let Preview::Binary { size, .. } = build_named("odd.txt", b"ok \xFF\xFE nope", Some(11))
        else {
            panic!("invalid UTF-8 must fall back to binary");
        };
        assert_eq!(size, 11, "a corrupt file must still show its size");
    }

    #[test]
    fn over_budget_text_is_an_explicit_head_not_a_spinner() {
        let big = vec![b'a'; config::TEXT_BUDGET + 100];
        let total = big.len() as u64;
        let Preview::Code { truncated, .. } = build_named("big.log", &big, Some(total)) else {
            panic!("a long log must build truncated code");
        };
        assert!(truncated.is_truncated());
        assert_eq!(truncated.shown, config::TEXT_BUDGET as u64);
        assert_eq!(truncated.total, total);
    }

    #[test]
    fn images_report_no_renderer_rather_than_an_attempted_decode() {
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let Preview::Unsupported { kind, reason } = build_named("photo.png", &png, Some(12)) else {
            panic!("images have no decoder yet");
        };
        assert_eq!(kind, Kind::Image);
        assert_eq!(reason, Unsupported::NoRenderer);
    }

    #[test]
    fn remote_backends_get_quarter_budgets() {
        assert_eq!(
            budget_for(
                &Kind::Code {
                    language: "rust".into()
                },
                true
            ),
            config::TEXT_BUDGET / 4
        );
    }
}
