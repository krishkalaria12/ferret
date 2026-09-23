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
//! Layout follows the repository conventions: every tunable lives in
//! [`config`]; fallible leaves (`image`, `svg`, `pdf`) define their own
//! error types beside the code, and the infallible pipeline (`build`)
//! reports through [`Preview`]/[`Unsupported`] rather than a crate-wide
//! error enum.

pub mod classify;
pub mod config;
pub mod image;
pub mod languages;
pub mod markdown;
pub mod pdf;
pub mod svg;
pub mod table;
pub mod text;
pub mod thumbs;

pub use classify::Kind;
pub use image::{Bitmap, DecodedImage, Frame, ImageError};
pub use pdf::{PdfUnavailable, RenderedPage, is_encrypted, page_count, render_page};
pub use svg::{SvgError, looks_like_svg};
pub use table::{ParsedTable, Row, sort_sample};
pub use text::{Bom, DecodedText, LineEndings, reencode};
pub use thumbs::{ThumbCache, fingerprint, is_tileable_extension, thumb_key};

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
    /// No provider in this build: the OS thumbnail service declined (or is
    /// absent, as on Windows pending its supervised helper) and no bundled
    /// renderer covers the kind. Metadata and *Open with…* instead — a
    /// finished state, not a failure state.
    NoRenderer,
    /// The file declares encryption (PDF `/Encrypt`). v1 opens nothing and
    /// retries nothing in the background: a password is required, so the
    /// pane says so and offers *Open with…* instead.
    Protected,
}

/// The value the background pipeline produces and `Render` only reads.
///
/// Carries source identity where the renderer needs it (Markdown's document
/// directory, which is what makes relative image links resolvable — and,
/// per PV-D10, resolvable only downward from there). Kinds no in-process
/// decoder covers reach [`Preview::Rendered`] through the OS thumbnail
/// service where one answers, else [`Preview::Unsupported`].
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

    /// F-35. Header row detected, delimiter sniffed, rows bounded. The pane
    /// renders the sample in a virtualised grid; sorting is sample-only.
    Table {
        table: ParsedTable,
        truncated: Truncation,
    },

    /// F-33. Already decoded, already downscaled, in the BGRA layout
    /// `RenderImage` wants. `natural` is the size before scaling, so the
    /// footer can be honest about it. `frames_capped` carries the animation
    /// fallback ("first frame shown") into the footer.
    Image {
        image: DecodedImage,
        truncated: Truncation,
    },

    /// F-34 and everything the OS knows how to draw. `by` records which
    /// path produced it, because "Quick Look" and "pdfium" fail differently.
    /// `pages` is `Some` once a paged backend (Pdfium) knows the document;
    /// a native thumbnail is one bitmap with no page model.
    Rendered {
        bitmap: Bitmap,
        kind: Kind,
        pages: Option<Pages>,
        by: RenderedBy,
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

/// Which renderer produced a [`Preview::Rendered`] bitmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderedBy {
    /// The OS thumbnail service (PV-D07): best-effort, capability-dependent,
    /// one bitmap, no page model.
    NativeThumbnail,
    /// The bundled Pdfium backend (PV-D08): paged, isolated, guaranteed on
    /// official builds.
    Pdfium,
}

/// Page position inside a paged document. Carried once the backend knows
/// the document; the pane shows "Page 1 of N" from this, not from pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pages {
    pub current: u32,
    pub total: u32,
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
        Kind::Svg => config::SVG_BUDGET,
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
/// `remote` must match the `remote` passed to [`budget_for`] for the read:
/// the `TooLarge` footer cites the effective budget, and a remote read capped
/// at a quarter of the local budget must not claim the local one.
/// Undecodable text falls back to the hex floor; undecodable hex is
/// impossible, so this only fails on programmer error (an empty kind map).
pub fn build(
    kind: Kind,
    bytes: Vec<u8>,
    total: Option<u64>,
    base: VfsPath,
    remote: bool,
) -> Preview {
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
        Kind::Image => {
            let shown = bytes.len() as u64;
            match image::decode(&bytes) {
                Ok(image) => Preview::Image {
                    image,
                    truncated: Truncation { shown, total },
                },
                // A vector image wearing a raster extension (or the reverse):
                // the bounded rasteriser gets one chance before the fallbacks.
                Err(image::ImageError::Undecodable) if svg::looks_like_svg(&bytes) => {
                    svg_preview(bytes, total, remote)
                }
                // A corrupt image must still show its size: the hex floor.
                Err(image::ImageError::Undecodable) => Preview::Binary {
                    head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
                    size: total,
                },
                // A valid image with no in-process decoder (HEIC, AVIF):
                // metadata and Open.
                Err(image::ImageError::UnsupportedFormat) => Preview::Unsupported {
                    kind,
                    reason: Unsupported::NoRenderer,
                },
                // Past even the head budget for a kind that cannot be
                // partially rendered: metadata, never an attempted decode.
                Err(image::ImageError::TooLarge { .. }) => {
                    let budget = budget_for(&kind, remote);
                    Preview::Unsupported {
                        kind,
                        reason: Unsupported::TooLarge {
                            size: total,
                            budget,
                        },
                    }
                }
            }
        }
        Kind::Svg => svg_preview(bytes, total, remote),
        Kind::Pdf => {
            let budget = budget_for(&kind, remote);
            if total > budget as u64 {
                Preview::Unsupported {
                    kind,
                    reason: Unsupported::TooLarge {
                        size: total,
                        budget,
                    },
                }
            } else if pdf::is_encrypted(&bytes) {
                // Labelled here so every path that declines the render —
                // the pipeline, the overlay, a future helper — agrees the
                // file needs a password. v1 opens nothing and retries nothing.
                Preview::Unsupported {
                    kind,
                    reason: Unsupported::Protected,
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

/// Build the [`Preview`] for SVG bytes: rasterise bounded, fall back to
/// labelled XML source for files that claim the extension without parsing,
/// and to the hex floor only for bytes that are neither.
fn svg_preview(bytes: Vec<u8>, total: u64, remote: bool) -> Preview {
    let shown = bytes.len() as u64;
    match svg::render(&bytes) {
        Ok(image) => Preview::Image {
            image,
            truncated: Truncation { shown, total },
        },
        Err(svg::SvgError::TooLarge) => {
            let budget = budget_for(&Kind::Svg, remote);
            Preview::Unsupported {
                kind: Kind::Svg,
                reason: Unsupported::TooLarge {
                    size: total,
                    budget,
                },
            }
        }
        Err(svg::SvgError::Unparseable) => match text::decode(&bytes) {
            Some(decoded) => Preview::Code {
                text: decoded.text,
                language: SmolStr::new("xml"),
                truncated: Truncation { shown, total },
            },
            None => Preview::Binary {
                head: bytes[..bytes.len().min(config::HEX_BUDGET)].to_vec(),
                size: total,
            },
        },
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
        build(kind, capped, size, VfsPath::local("/preview"), false)
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
    fn images_decode_to_bitmaps() {
        let encoded = {
            let img = ::image::DynamicImage::ImageRgba8(::image::RgbaImage::from_pixel(
                2,
                2,
                ::image::Rgba([1, 2, 3, 255]),
            ));
            let mut bytes = Vec::new();
            img.write_to(
                &mut std::io::Cursor::new(&mut bytes),
                ::image::ImageFormat::Png,
            )
            .unwrap();
            bytes
        };
        let Preview::Image { image, .. } = build_named("photo.png", &encoded, Some(100)) else {
            panic!("a real PNG must decode");
        };
        assert_eq!(image.natural, (2, 2));
    }

    #[test]
    fn corrupt_images_fall_back_to_hex_with_their_size() {
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
        let Preview::Binary { size, .. } = build_named("photo.png", &png, Some(12)) else {
            panic!("a corrupt PNG must fall back to binary");
        };
        assert_eq!(size, 12);
    }

    #[test]
    fn heic_builds_unsupported_metadata_not_hex() {
        // Valid photo, no in-process decoder: metadata and Open — never a
        // hex dump of a good file.
        let mut bytes = b"....ftypheic".to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        let Preview::Unsupported { kind, reason } =
            build_named("photo.heic", &bytes, Some(bytes.len() as u64))
        else {
            panic!("HEIC must build unsupported metadata");
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

    #[test]
    fn svg_builds_a_bitmap_labelled_svg() {
        let source = br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="green"/></svg>"#;
        let Preview::Image { image, .. } =
            build_named("icon.svg", source, Some(source.len() as u64))
        else {
            panic!("a real SVG must rasterise");
        };
        assert_eq!(image.format_name, "SVG");
        assert_eq!(image.natural, (32, 32));
    }

    #[test]
    fn ordinary_pdfs_build_unsupported_until_the_backend_answers() {
        // `build` never calls the renderer: an in-budget PDF is metadata
        // with a renderer-shaped hole, which the pipeline fills (Pdfium)
        // or leaves (native thumbnail, then this message).
        let bytes = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
        let Preview::Unsupported { kind, reason } =
            build_named("report.pdf", &bytes, Some(bytes.len() as u64))
        else {
            panic!("a PDF must not preview as text, code or hex");
        };
        assert_eq!(kind, Kind::Pdf);
        assert_eq!(reason, Unsupported::NoRenderer);
    }

    #[test]
    fn encrypted_pdfs_label_protected_not_unrenderable() {
        // The password requirement is detected from bytes alone, on every
        // path that declines the render — never a spinner, never retries.
        let mut bytes = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec();
        bytes.extend_from_slice(b"trailer\n<< /Size 2 /Encrypt 5 0 R >>");
        let Preview::Unsupported { kind, reason } =
            build_named("secret.pdf", &bytes, Some(bytes.len() as u64))
        else {
            panic!("an encrypted PDF must not preview as anything else");
        };
        assert_eq!(kind, Kind::Pdf);
        assert_eq!(reason, Unsupported::Protected);
    }

    #[test]
    fn garbage_wearing_svg_falls_back_to_labelled_source() {
        let Preview::Code { language, .. } =
            build_named("icon.svg", b"definitely not xml", Some(18))
        else {
            panic!("an unparseable .svg must preview as source, not hex");
        };
        assert_eq!(language.as_str(), "xml");
    }
}
