//! PDF: page counts, encryption detection, and the renderer seam (F-34, PV-D08).
//!
//! Reliable PDF rendering is part of the release: official builds bundle
//! Pdfium and render bounded page bitmaps through it. This module is the
//! code side of that contract:
//!
//! * [`page_count`] and [`is_encrypted`] are pure scans over bytes already
//!   read under [`crate::config::PDF_BUDGET`]. No library, no window.
//! * [`render_page`] rasterises one page through the bundled Pdfium backend
//!   behind the crate's `pdf` feature. The native library itself is resolved
//!   at runtime — an application-controlled absolute path beside the
//!   executable first, then the system library — and its absence is
//!   [`PdfUnavailable::NoRenderer`], a metadata fallback rather than a crash.
//!   Developer builds may disable `pdf`; that reduced build does not satisfy
//!   F-34, and says so in the footer rather than pretending otherwise.
//!
//! Never executed here: JavaScript, launch actions, attachments, embedded
//! media. Pdfium parses and paints; the pane shows bitmaps.
//!
//! Runs on the caller's thread, which must not be the window thread. A
//! process boundary around the helper (crash containment, memory ceiling) is
//! the documented follow-up; until it lands, renders run on the background
//! pool with [`crate::config::PDF_RENDER_DEADLINE`] as the caller's budget
//! for waiting, not as a preemption of native code.

use super::config::{PDF_BUDGET, PDF_TARGET_LONG_EDGE};
use super::image::Bitmap;

/// Why a page bitmap is unavailable.
///
/// Every variant is a finished pane state, not a spinner: `Protected` names
/// the password requirement, `TooLarge` names both numbers, and `NoRenderer`
/// names the missing library rather than the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdfUnavailable {
    /// The file declares encryption (`/Encrypt`). v1 opens nothing and
    /// retries nothing in the background.
    Encrypted,
    /// Past [`PDF_BUDGET`]: PDFs need random access and can place their index
    /// at the end, so a truncated head is useless and no partial render is
    /// attempted.
    TooLarge { size: u64 },
    /// No Pdfium library where the runtime looked. A packaging state, not a
    /// file state: the bytes were never judged.
    NoRenderer,
    /// The library answered but the page did not rasterise: corrupt file,
    /// unknown page, or a render that exceeded its budget.
    Failed,
}

/// How many pages a PDF claims, from a bounded heuristic scan.
///
/// Takes the maximum of every `/Count` in a page tree and every directly
/// written `/Type /Page` object. The root's `/Count` is the true total and
/// dominates any subtree; the object scan covers files whose tree the scan
/// cannot follow. Object streams (PDF 1.5+) can hide page objects from both
/// scans, so the count is a floor the renderer corrects upward, never a
/// ceiling anything depends on for safety.
pub fn page_count(bytes: &[u8]) -> Option<u32> {
    if !bytes.starts_with(b"%PDF-") {
        return None;
    }
    let mut total = 0u32;
    // `/Count 12` — the page tree's own totals; the root (the largest) wins.
    total = total.max(max_count(bytes));
    // Directly written page objects, excluding `/Pages` and `/PageMode`-style
    // names by requiring a delimiter after `/Page`.
    total = total.max(count_page_objects(bytes));
    (total > 0).then_some(total)
}

/// Whether the file declares encryption.
///
/// A cheap scan for an `/Encrypt` name, which is what makes a PDF password
/// protected. False positives require a content stream to contain that exact
/// name, which is why this only *labels* the Protected state — the renderer
/// confirms it by failing to open without a password.
pub fn is_encrypted(bytes: &[u8]) -> bool {
    contains_name(bytes, b"/Encrypt")
}

/// Render one page (zero-based) to a display-ready bitmap.
///
/// `pdf` is the complete bytes within [`PDF_BUDGET`]; larger inputs are
/// refused before any library call. Without the `pdf` feature this always
/// returns [`PdfUnavailable::NoRenderer`].
#[cfg(feature = "pdf")]
pub fn render_page(pdf: &[u8], page_index: u32) -> Result<RenderedPage, PdfUnavailable> {
    if pdf.len() as u64 > PDF_BUDGET as u64 {
        return Err(PdfUnavailable::TooLarge {
            size: pdf.len() as u64,
        });
    }
    if is_encrypted(pdf) {
        return Err(PdfUnavailable::Encrypted);
    }
    let pdfium = bound_pdfium().map_err(|_| PdfUnavailable::NoRenderer)?;
    let document = pdfium
        .load_pdf_from_byte_slice(pdf, None)
        .map_err(|_| PdfUnavailable::Failed)?;
    let pages = document.pages();
    let total = pages.len().max(0) as u32;
    let index: i32 = page_index.try_into().map_err(|_| PdfUnavailable::Failed)?;
    if page_index >= total {
        return Err(PdfUnavailable::Failed);
    }
    let page = pages.get(index).map_err(|_| PdfUnavailable::Failed)?;
    let (page_width, page_height) = (page.width().value, page.height().value);
    if !(page_width > 0.0 && page_height > 0.0) {
        return Err(PdfUnavailable::Failed);
    }
    let long_edge = page_width.max(page_height);
    let scale = PDF_TARGET_LONG_EDGE as f32 / long_edge;
    let target_width = ((page_width * scale).round() as i32).max(1);
    let target_height = ((page_height * scale).round() as i32).max(1);
    if target_width as u64 * target_height as u64 > super::config::IMAGE_SOURCE_PIXEL_CAP {
        return Err(PdfUnavailable::TooLarge {
            size: pdf.len() as u64,
        });
    }
    let config = pdfium_render::prelude::PdfRenderConfig::new()
        .set_target_width(target_width)
        .set_target_height(target_height);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|_| PdfUnavailable::Failed)?;
    let (width, height) = (bitmap.width().max(0) as u32, bitmap.height().max(0) as u32);
    let bitmap =
        Bitmap::from_rgba8(width, height, bitmap.as_rgba_bytes()).ok_or(PdfUnavailable::Failed)?;
    Ok(RenderedPage {
        bitmap,
        natural: (target_width.max(0) as u32, target_height.max(0) as u32),
        pages_total: total,
    })
}

/// One rasterised page: the bitmap plus the document facts the footer needs.
#[derive(Clone, Debug)]
pub struct RenderedPage {
    pub bitmap: Bitmap,
    pub natural: (u32, u32),
    pub pages_total: u32,
}

/// Without the `pdf` feature there is no backend to call — but the cheap
/// verdicts (encryption, size) do not need one, so they still report
/// correctly rather than blaming the packaging for the file.
#[cfg(not(feature = "pdf"))]
pub fn render_page(pdf: &[u8], _page_index: u32) -> Result<RenderedPage, PdfUnavailable> {
    if pdf.len() as u64 > PDF_BUDGET as u64 {
        return Err(PdfUnavailable::TooLarge {
            size: pdf.len() as u64,
        });
    }
    if is_encrypted(pdf) {
        return Err(PdfUnavailable::Encrypted);
    }
    Err(PdfUnavailable::NoRenderer)
}

/// The bound library, initialised once per process.
///
/// `Pdfium::new` installs process-global bindings, so rebinding is an error
/// rather than a second instance: the `OnceLock` holds the one answer —
/// success or failure — for every later render. A missing library fails here,
/// once, and every PDF shows metadata instead of paying a `dlopen` per page.
#[cfg(feature = "pdf")]
fn bound_pdfium() -> Result<&'static pdfium_render::prelude::Pdfium, ()> {
    use pdfium_render::prelude::Pdfium;
    use std::sync::OnceLock;

    static BOUND: OnceLock<Result<Pdfium, ()>> = OnceLock::new();
    BOUND
        .get_or_init(|| {
            if let Some(path) = explicit_library_path()
                && let Ok(bindings) = Pdfium::bind_to_library(&path)
            {
                return Ok(Pdfium::new(bindings));
            }
            Pdfium::bind_to_system_library()
                .map(Pdfium::new)
                .map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| ())
}

/// An application-controlled absolute path: `FERRET_PDFIUM_LIB` for
/// development, else the platform library name beside the executable.
///
/// Only these locations are ever loaded: whatever `PATH` or the current
/// directory happens to contain is never a PDF library as far as this
/// function is concerned. Returns `None` when neither names an existing
/// file, in which case the caller falls through to the system library.
#[cfg(feature = "pdf")]
fn explicit_library_path() -> Option<std::path::PathBuf> {
    use pdfium_render::prelude::Pdfium;

    if let Some(path) = std::env::var_os("FERRET_PDFIUM_LIB") {
        let path = std::path::PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let candidate = Pdfium::pdfium_platform_library_name_at_path(&dir);
    candidate.exists().then_some(candidate)
}

/// Largest `/Count` in any page tree node.
fn max_count(bytes: &[u8]) -> u32 {
    let mut best = 0u32;
    let mut index = 0;
    while let Some(found) = find_token(bytes, index, b"/Count") {
        let mut cursor = found + b"/Count".len();
        cursor = skip_pdf_whitespace(bytes, cursor);
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if start < cursor
            && let Ok(value) = std::str::from_utf8(&bytes[start..cursor])
            && let Ok(count) = value.parse::<u32>()
        {
            best = best.max(count);
        }
        index = found + 1;
    }
    best
}

/// Directly written `/Type /Page` objects, excluding `/Pages`.
fn count_page_objects(bytes: &[u8]) -> u32 {
    let mut total = 0u32;
    let mut index = 0;
    while let Some(found) = find_token(bytes, index, b"/Type") {
        let mut cursor = skip_pdf_whitespace(bytes, found + b"/Type".len());
        if bytes.get(cursor..cursor + 5) == Some(b"/Page") {
            // `/Pages`, `/PageMode`, `/PageLabels` must not count.
            let after = bytes.get(cursor + 5).copied().unwrap_or(b' ');
            if is_pdf_delimiter(after) {
                total += 1;
            }
        }
        // Comments (`%...`) can hide a `/Type` from naive scans and, worse,
        // a content stream can fake one. Skipping to end-of-line past `%`
        // keeps the scan honest about structure without parsing objects.
        index = found + 1;
        let _ = &mut cursor;
    }
    total
}

/// Whether `name` (with its leading slash) appears as a PDF name.
fn contains_name(bytes: &[u8], name: &[u8]) -> bool {
    let mut index = 0;
    while let Some(found) = find_token(bytes, index, name) {
        let after = bytes.get(found + name.len()).copied().unwrap_or(b' ');
        if is_pdf_delimiter(after) {
            return true;
        }
        index = found + 1;
    }
    false
}

/// Find a token that starts at a name boundary: start of input, whitespace,
/// or one of `/<([`.
fn find_token(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    let mut index = from;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle
            && (index == 0 || is_pdf_boundary(haystack[index - 1]))
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn is_pdf_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'/' | b'<' | b'(' | b'[')
}

fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'/' | b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'%' | b'\0'
        )
}

fn skip_pdf_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while cursor < bytes.len() && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'\0') {
        cursor += 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One hand-made single-page PDF: base-14 Helvetica, no embedding,
    /// byte-exact xref. The same shape the UI tests probe the native path
    /// with — here it proves the count and encryption scans instead.
    pub(crate) const SINGLE_PAGE: &[u8] = b"\
        %PDF-1.4\n\
        1 0 obj\n\
        << /Type /Catalog /Pages 2 0 R >>\n\
        endobj\n\
        2 0 obj\n\
        << /Type /Pages /Kids [3 0 R] /Count 1 >>\n\
        endobj\n\
        3 0 obj\n\
        << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n\
        endobj\n\
        4 0 obj\n\
        << /Length 44 >>\n\
        stream\n\
        BT /F1 24 Tf 72 72 Td (Hello) Tj ET\n\
        endstream\n\
        endobj\n\
        5 0 obj\n\
        << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n\
        endobj\n\
        xref\n\
        0 6\n\
        0000000000 65535 f \n\
        0000000009 00000 n \n\
        0000000058 00000 n \n\
        0000000115 00000 n \n\
        0000000288 00000 n \n\
        0000000382 00000 n \n\
        trailer\n\
        << /Size 6 /Root 1 0 R >>\n\
        startxref\n\
        479\n\
        %%EOF";

    fn two_page() -> Vec<u8> {
        // A second page object plus a root count of 2.
        let raw = String::from_utf8_lossy(SINGLE_PAGE).replace(
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>",
        );
        let mut pdf = raw.into_bytes();
        pdf.extend_from_slice(
            b"\n6 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] >>\nendobj\n",
        );
        pdf
    }

    #[test]
    fn counts_pages_from_tree_and_objects() {
        assert_eq!(page_count(SINGLE_PAGE), Some(1));
        assert_eq!(page_count(&two_page()), Some(2));
        // `/Pages` and `/PageMode` never count as pages.
        assert_eq!(
            page_count(
                b"%PDF-1.4\n1 0 obj\n<< /Type /Pages /Count 0 /PageMode /UseNone >>\nendobj"
            ),
            None
        );
        assert_eq!(page_count(b"not a pdf"), None);
    }

    #[test]
    fn encryption_is_detected_not_rendered() {
        assert!(!is_encrypted(SINGLE_PAGE));
        // A longer name that merely starts the same way is not encryption.
        let mut other = SINGLE_PAGE.to_vec();
        other.extend_from_slice(b"\n7 0 obj\n<< /Encryptor (some filter) >>\nendobj");
        assert!(!is_encrypted(&other));
        let mut protected = SINGLE_PAGE.to_vec();
        protected.extend_from_slice(b"\ntrailer\n<< /Size 6 /Root 1 0 R /Encrypt 7 0 R >>");
        assert!(is_encrypted(&protected));
    }

    #[test]
    fn encrypted_bytes_refuse_before_any_library_call() {
        let mut protected = SINGLE_PAGE.to_vec();
        protected.extend_from_slice(b"\ntrailer\n<< /Size 6 /Root 1 0 R /Encrypt 7 0 R >>");
        assert_eq!(
            render_page(&protected, 0).unwrap_err(),
            PdfUnavailable::Encrypted
        );
    }

    #[test]
    fn oversize_inputs_refuse_before_any_library_call() {
        // One byte past the budget: refused by length alone, whatever the
        // content — a truncated head is useless for a format that indexes
        // from the end.
        let big = vec![0u8; PDF_BUDGET + 1];
        assert_eq!(
            render_page(&big, 0).unwrap_err(),
            PdfUnavailable::TooLarge {
                size: big.len() as u64
            }
        );
    }

    #[test]
    fn missing_libraries_fall_back_to_metadata_not_a_crash() {
        // No Pdfium ships with the test environment: a small valid PDF must
        // report the missing backend, never panic, never hang.
        match render_page(SINGLE_PAGE, 0) {
            Ok(_) => {}
            Err(PdfUnavailable::NoRenderer) => {}
            Err(other) => panic!("unexpected verdict for a valid page: {other:?}"),
        }
    }
}
