//! Deciding what a file is.
//!
//! Extension first because it is free and usually right; content second
//! because it is the only thing that is *reliably* right. A `.txt` that is
//! actually a 900 MB core dump must not reach the text renderer, and a
//! `Makefile` with no extension at all must.
//!
//! Order: exact filename and extension table, then magic bytes (which
//! *override* the name for images and archives), then the binary heuristic,
//! then plain text. The size gate is applied last, per kind, by the pipeline
//! before any full read is attempted.

use smol_str::SmolStr;

/// What a file is, as far as the preview pipeline needs to care.
///
/// A `Kind` decides the budget and the provider. Whether the provider can
/// actually render it is a later question — an image on a build without a
/// decoder is still an image, and says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Rendered Markdown (falls back to bounded source where the rendered
    /// view is unavailable or over budget).
    Markdown,
    /// Source or text with a registry language name — never an extension.
    /// `Plain` is a language, not an absence of one.
    Code {
        language: SmolStr,
    },
    Csv,
    Tsv,
    Image,
    /// Vector images: rasterised in-process by [`crate::svg`], bounded by
    /// [`crate::config::SVG_BUDGET`] rather than [`crate::config::IMAGE_BUDGET`].
    Svg,
    Pdf,
    /// Anything else: the hex floor, not an apology.
    Binary,
}

impl Kind {
    /// The header label: the resolved language or format name — "Python",
    /// "PNG image", "PDF document" — never the extension.
    pub fn display_name(&self) -> String {
        match self {
            Kind::Markdown => "Markdown".into(),
            Kind::Code { language } => super::languages::display_name(language).to_string(),
            Kind::Csv => "CSV table".into(),
            Kind::Tsv => "TSV table".into(),
            Kind::Image => "Image".into(),
            Kind::Svg => "SVG image".into(),
            Kind::Pdf => "PDF document".into(),
            Kind::Binary => "Binary".into(),
        }
    }

    /// Whether this kind is text the editor can show.
    pub fn is_text(self_kind: &Kind) -> bool {
        matches!(
            self_kind,
            Kind::Markdown | Kind::Code { .. } | Kind::Csv | Kind::Tsv
        )
    }
}

/// Classify one file from its name, its head bytes and its size.
///
/// `sniff` is the first [`crate::config::SNIFF_BYTES`] (or fewer, for a
/// short file); `size` is the full length when the listing already knows it.
/// Pure and total: no I/O, no allocation past the language name, safe to call
/// per keystroke while a sweep settles.
///
/// The order is the point. Exact filenames win outright (`Makefile` is
/// `Makefile` whatever it contains). Magic bytes override *extensions* for
/// images and archives — a `.txt` that is actually a PNG is a PNG — but the
/// binary heuristic overrides extensions the other way: a `.txt` with a NUL
/// at byte 4000 is a core dump, not text. Content is the only thing that is
/// reliably right, so content gets the last word over the name in both
/// directions.
pub fn classify(file_name: &str, sniff: &[u8], size: Option<u64>) -> Kind {
    // 1. Exact filenames before everything.
    if let Some(language) = super::languages::filename_language(file_name) {
        return language_kind(language);
    }

    // 2. Magic bytes: images, archives and documents.
    if let Some(kind) = magic_kind(sniff) {
        return kind;
    }

    // 3. Extension table (tentative) and format extensions. A raster file
    // with its real extension but unrecognised magic (HEIC, RAW) is still an
    // image — it reaches the OS-thumbnail path, never the text view. SVG is
    // checked before the language table: `svg` resolves to `xml` there, and
    // a vector image must rasterise rather than preview as source now that
    // the bounded rasteriser exists.
    let mut kind = is_svg_name(file_name)
        .then_some(Kind::Svg)
        .or_else(|| super::languages::language_for(file_name).map(language_kind))
        .or_else(|| format_extension_kind(file_name));
    if kind.is_none() {
        // The extension table already maps `txt` to `plain`; anything else
        // unknown starts as plain text pending the content checks below.
        kind = Some(Kind::Code {
            language: SmolStr::new("plain"),
        });
    }

    // 4. Binary test. BOMs first: UTF-16/32 naturally contain NULs, and
    // testing NULs before BOMs calls every UTF-16 file binary.
    if has_bom(sniff) {
        return Kind::Code {
            language: SmolStr::new("plain"),
        };
    }
    if sniff.contains(&0) {
        return Kind::Binary;
    }

    // 5. Size gate for the text decision. Content sniffing cannot see past
    // its window: a 4 GB log starts with perfectly good UTF-8. Past 64 text
    // budgets the head is not worth decoding as text at all.
    if size.is_some_and(|len| len > super::config::TEXT_BUDGET as u64 * 64) {
        return Kind::Binary;
    }

    kind.unwrap_or(Kind::Binary)
}

/// Whether a file name claims the `.svg` extension, case-insensitively.
///
/// Checked before the language table, where `svg` would otherwise resolve to
/// `xml` and preview as source. A leading dot makes a hidden file, not an
/// extension, the same rule the language table uses.
fn is_svg_name(file_name: &str) -> bool {
    let Some((_, ext)) = file_name.trim_start_matches('.').rsplit_once('.') else {
        return false;
    };
    ext.eq_ignore_ascii_case("svg")
}

/// Image and document formats by extension, for files whose magic is
/// unrecognised (HEIC, RAW) or absent from the sniff window.
fn format_extension_kind(file_name: &str) -> Option<Kind> {
    let (_, ext) = file_name.trim_start_matches('.').rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    // One fold at most, and only for the rare uppercase name.
    let lower;
    let ext = if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        lower = ext.to_ascii_lowercase();
        lower.as_str()
    } else {
        ext
    };
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "tif" | "tiff" | "bmp" | "ico" | "avif"
        | "qoi" | "heic" | "heif" | "psd" | "cr2" | "nef" | "arw" | "dng" | "rw2" | "orf"
        | "raf" => Some(Kind::Image),
        "pdf" => Some(Kind::Pdf),
        _ => None,
    }
}

/// Map a resolved language name to the kind that renders it.
///
/// Note `svg` still resolves to `xml` in the language table for files that
/// reach it without the extension (an extensionless vector document): those
/// preview as labelled source, the honest fallback. Raster formats return no
/// language at all, so they can only arrive here via magic bytes.
fn language_kind(language: &'static str) -> Kind {
    match language {
        "markdown" => Kind::Markdown,
        "csv" => Kind::Csv,
        "tsv" => Kind::Tsv,
        _ => Kind::Code {
            language: SmolStr::new(language),
        },
    }
}

/// Image, archive and document magic, checked against the sniff.
///
/// Deliberately a short list of formats the pipeline does something with.
/// Anything else falls through to the binary heuristic below.
fn magic_kind(sniff: &[u8]) -> Option<Kind> {
    if sniff.starts_with(b"%PDF-") {
        return Some(Kind::Pdf);
    }
    if sniff.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some(Kind::Image);
    }
    if sniff.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Kind::Image);
    }
    if sniff.starts_with(b"GIF87a") || sniff.starts_with(b"GIF89a") {
        return Some(Kind::Image);
    }
    if sniff.len() >= 12 && sniff.starts_with(b"RIFF") && sniff.get(8..12) == Some(b"WEBP") {
        return Some(Kind::Image);
    }
    if sniff.starts_with(b"II*\0") || sniff.starts_with(b"MM\0*") {
        return Some(Kind::Image);
    }
    if sniff.starts_with(b"BM") {
        return Some(Kind::Image);
    }
    if sniff.starts_with(&[0, 0, 1, 0]) {
        return Some(Kind::Image);
    }
    if sniff.starts_with(b"qoif") {
        return Some(Kind::Image);
    }
    if is_avif(sniff) {
        return Some(Kind::Image);
    }
    // Archives and executables: recognised so a mislabelled one still lands
    // on the binary floor rather than in the text view.
    if sniff.starts_with(b"PK\x03\x04")
        || sniff.starts_with(b"PK\x05\x06")
        || sniff.starts_with(b"PK\x07\x08")
    {
        return Some(Kind::Binary);
    }
    if sniff.starts_with(&[0x1F, 0x8B]) {
        return Some(Kind::Binary);
    }
    if sniff.starts_with(&[0x7F, b'E', b'L', b'F']) {
        return Some(Kind::Binary);
    }
    if sniff.starts_with(b"MZ") {
        return Some(Kind::Binary);
    }
    if sniff.starts_with(&[0xFE, 0xED, 0xFA, 0xCE])
        || sniff.starts_with(&[0xFE, 0xED, 0xFA, 0xCF])
        || sniff.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE])
    {
        return Some(Kind::Binary);
    }
    None
}

/// `ftyp` box with a HEIC/AVIF brand. Both are images the in-process decoder
/// does not handle — which is exactly why they must be *recognised* here, so
/// they reach the OS-thumbnail path instead of the text view.
fn is_avif(sniff: &[u8]) -> bool {
    if sniff.len() < 12 || sniff.get(4..8) != Some(b"ftyp") {
        return false;
    }
    let brand = sniff.get(8..12).unwrap_or_default();
    matches!(
        brand,
        b"avif" | b"avis" | b"heic" | b"heix" | b"hevc" | b"hevx"
    )
}

/// Whether the sniff opens with a Unicode byte-order mark.
fn has_bom(sniff: &[u8]) -> bool {
    sniff.starts_with(&[0xEF, 0xBB, 0xBF])
        || sniff.starts_with(&[0xFF, 0xFE])
        || sniff.starts_with(&[0xFE, 0xFF])
        || sniff.starts_with(&[0, 0, 0xFE, 0xFF])
        || sniff.starts_with(&[0xFF, 0xFE, 0, 0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_files_resolve_by_name() {
        assert!(matches!(
            classify("main.rs", b"fn main() {}", Some(12)),
            Kind::Code { .. }
        ));
        assert_eq!(
            classify("Makefile", b"all:\n", Some(5)),
            Kind::Code {
                language: SmolStr::new("make"),
            }
        );
        assert_eq!(classify("README.md", b"# hi\n", Some(5)), Kind::Markdown);
        assert_eq!(classify("data.csv", b"a,b\n", Some(4)), Kind::Csv);
        assert_eq!(classify("data.tsv", b"a\tb\n", Some(4)), Kind::Tsv);
    }

    #[test]
    fn magic_overrides_a_wrong_extension() {
        // A `.txt` that is actually a PNG is a PNG — the case content
        // sniffing exists for.
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(classify("photo.txt", &png, Some(8)), Kind::Image);
        assert_eq!(classify("doc.txt", b"%PDF-1.7", Some(8)), Kind::Pdf);
    }

    #[test]
    fn a_nul_makes_it_binary_unless_a_bom_says_otherwise() {
        // A 900 MB core dump renamed `.txt` must not reach the text renderer.
        let mut dump = vec![b'#'; 100];
        dump[40] = 0;
        assert_eq!(classify("core.txt", &dump, Some(900_000_000)), Kind::Binary);

        // UTF-16 is full of NULs and is text all the same.
        let utf16 = [0xFF, 0xFE, b'h', 0, b'i', 0];
        assert!(matches!(
            classify("notes.txt", &utf16, Some(6)),
            Kind::Code { .. }
        ));
    }

    #[test]
    fn huge_text_without_nuls_is_still_not_text() {
        // The sniff window cannot see the whole file; the known size can.
        let head = vec![b'a'; 8192];
        assert_eq!(
            classify("huge.log", &head, Some(4_000_000_000)),
            Kind::Binary
        );
    }

    #[test]
    fn svg_names_rasterise_before_the_language_table() {
        // The language table maps `svg` to `xml`; the extension check runs
        // first so vector images rasterise instead of previewing as source.
        assert_eq!(classify("icon.svg", b"<svg", Some(5)), Kind::Svg);
        assert_eq!(classify("ICON.SVG", b"<svg", Some(5)), Kind::Svg);
        // ...but only the extension claims it: inline SVG in HTML stays HTML.
        assert!(matches!(
            classify("page.html", b"<svg", Some(5)),
            Kind::Code { .. }
        ));
    }

    #[test]
    fn archives_and_executables_land_on_the_binary_floor() {
        assert_eq!(classify("a.zip", b"PK\x03\x04....", Some(12)), Kind::Binary);
        assert_eq!(
            classify("a", &[0x7F, b'E', b'L', b'F'], Some(4)),
            Kind::Binary
        );
        assert_eq!(classify("a.exe", b"MZ....", Some(6)), Kind::Binary);
    }
}
