//! SVG: bounded off-thread rasterisation (F-33).
//!
//! Vector images preview as real images now that this module exists — before
//! it, they previewed as labelled source, which was the documented fallback,
//! not a misclassification. The pipeline stays the same shape as raster
//! images (probe, cap, decode, downscale) with vector-specific guards:
//!
//! 1. Source bytes are capped ([`crate::config::SVG_BUDGET`]) before parsing.
//! 2. External resources are never resolved: `usvg` is configured with no
//!    resource directory, so `href`s to files, fonts or network locations
//!    render as nothing rather than fetching anything.
//! 3. The parsed tree is walked and counted: past
//!    [`crate::config::SVG_MAX_NODES`] nodes or
//!    [`crate::config::SVG_MAX_DEPTH`] nesting the answer is `TooLarge`,
//!    never an attempted render — `viewBox` dimensions alone do not bound
//!    complexity.
//! 4. Rendering happens at the display scale directly (vectors have no
//!    "natural" pixels to downscale from), bounded by
//!    [`crate::config::IMAGE_TARGET_LONG_EDGE`].
//!
//! Runs on the caller's thread, which must not be the window thread.

use super::config::{
    IMAGE_SOURCE_PIXEL_CAP, IMAGE_TARGET_LONG_EDGE, SVG_BUDGET, SVG_MAX_DEPTH, SVG_MAX_NODES,
};
use super::image::{Bitmap, DecodedImage};

/// Why an SVG did not rasterise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvgError {
    /// Past the source budget, the node/depth cap, or the pixel cap: never
    /// an attempted render.
    TooLarge,
    /// Well-formed XML but not an SVG document, or an SVG the parser
    /// rejects. The caller falls back to labelled source, not to hex.
    Unparseable,
}

/// Whether bytes look like an SVG document: optional BOM, whitespace and
/// comments skipped, then `<svg`, a `DOCTYPE svg`, or an XML prolog followed
/// by `<svg` within a bounded prefix.
///
/// Pure and cheap — the pipeline asks this before spending a parse, and the
/// classifier never sees it (classification is by `.svg` extension, so an
/// HTML file with inline SVG keeps previewing as HTML). A bare `<?xml`
/// without `<svg` is *not* enough: any mislabelled XML would otherwise pay a
/// full `usvg` parse before falling back to source.
pub fn looks_like_svg(bytes: &[u8]) -> bool {
    let mut body = bytes;
    if let Some(stripped) = body.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        body = stripped;
    }
    loop {
        body = trim_start_xml_whitespace(body);
        if let Some(rest) = body.strip_prefix(b"<!--") {
            let Some(end) = find_bytes(rest, b"-->") else {
                return false;
            };
            body = &rest[end + 3..];
        } else {
            break;
        }
    }
    if body.starts_with(b"<svg")
        || body.starts_with(b"<SVG")
        || body.starts_with(b"<!DOCTYPE svg")
        || body.starts_with(b"<!DOCTYPE SVG")
    {
        return true;
    }
    // An XML prolog is only a maybe: require `<svg` within the first 4 KiB
    // after it, case-insensitively, so a 2 MiB XML bomb does not pass the
    // sniff on the prolog alone.
    if body.starts_with(b"<?xml") {
        let window = &body[..body.len().min(4096)];
        return find_bytes(window, b"<svg").is_some() || find_bytes(window, b"<SVG").is_some();
    }
    false
}

/// Rasterise bounded SVG bytes into display-ready frames.
pub fn render(bytes: &[u8]) -> Result<DecodedImage, SvgError> {
    if bytes.len() > SVG_BUDGET {
        return Err(SvgError::TooLarge);
    }
    if !looks_like_svg(bytes) {
        return Err(SvgError::Unparseable);
    }
    // Entity expansion happens inside `Tree::from_data`, before the node cap
    // below runs: a 10 KiB billion-laughs payload could OOM the parse. Reject
    // internal subsets up front — legitimate icons almost never define
    // entities, and the cost of refusing one is a source fallback.
    if has_internal_subset(bytes) {
        return Err(SvgError::Unparseable);
    }
    // No resource directory: external files, fonts and network locations
    // referenced from the document resolve to nothing. `usvg` 0.46 renders
    // `<text>` through an empty font database by default (no system font
    // scan per file): text elements come out blank rather than slow, which
    // is the safe direction — a source fallback would be noisier than an
    // honest gap, and a per-file font scan would be a perf cliff.
    let options = usvg::Options {
        resources_dir: None,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|_| SvgError::Unparseable)?;
    check_complexity(&tree)?;
    let size = tree.size();
    if !(size.width() > 0.0 && size.height() > 0.0) {
        return Err(SvgError::Unparseable);
    }
    let natural = (size.width().ceil() as u32, size.height().ceil() as u32);
    if natural.0 as u64 * natural.1 as u64 > IMAGE_SOURCE_PIXEL_CAP {
        return Err(SvgError::TooLarge);
    }
    // Render at display scale directly: vectors gain nothing from rendering
    // huge and downscaling, and the output stays bounded by construction.
    // The output cap reuses `IMAGE_SOURCE_PIXEL_CAP` (16 MP) while the raster
    // path never re-checks post-scale: for SVG the check matters because a
    // 4096-long-edge square scales to 16.7 MP, just past the source cap, and
    // refusing it is the conservative answer. One semantic ("no bitmap past
    // 16 MP anywhere") shared by both paths.
    let long_edge = natural.0.max(natural.1).max(1);
    let scale = if long_edge > IMAGE_TARGET_LONG_EDGE {
        IMAGE_TARGET_LONG_EDGE as f32 / long_edge as f32
    } else {
        1.0
    };
    let out_width = ((natural.0 as f32 * scale).round() as u32).max(1);
    let out_height = ((natural.1 as f32 * scale).round() as u32).max(1);
    if out_width as u64 * out_height as u64 > IMAGE_SOURCE_PIXEL_CAP {
        return Err(SvgError::TooLarge);
    }
    let mut pixmap = tiny_skia::Pixmap::new(out_width, out_height).ok_or(SvgError::TooLarge)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let bitmap = Bitmap::from_premultiplied_rgba8(out_width, out_height, pixmap.take())
        .ok_or(SvgError::Unparseable)?;
    Ok(DecodedImage {
        natural,
        format_name: "SVG",
        frames: vec![super::image::Frame {
            bitmap,
            delay_ms: 0,
        }],
        frames_capped: false,
    })
}

/// Walk the tree counting nodes and depth before rasterising.
///
/// Filters and masks can make one node arbitrarily expensive, which no node
/// count captures — this is the documented residual risk, and the reason the
/// output pixels are capped independently above.
fn check_complexity(tree: &usvg::Tree) -> Result<(), SvgError> {
    fn walk(group: &usvg::Group, depth: usize, count: &mut usize) -> Result<(), SvgError> {
        if depth > SVG_MAX_DEPTH {
            return Err(SvgError::TooLarge);
        }
        for child in group.children() {
            *count += 1;
            if *count > SVG_MAX_NODES {
                return Err(SvgError::TooLarge);
            }
            if let usvg::Node::Group(nested) = child {
                walk(nested, depth + 1, count)?;
            }
        }
        Ok(())
    }

    let mut count = 0;
    walk(tree.root(), 0, &mut count)
}

fn trim_start_xml_whitespace(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if matches!(first, b' ' | b'\t' | b'\n' | b'\r') {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether the source declares a `DOCTYPE` with an internal subset or any
/// entity: the billion-laughs shape. Case-insensitive on the keyword; the
/// match is a cheap byte scan, not a parse.
fn has_internal_subset(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SVG_BUDGET)];
    // ASCII-case-insensitive search for `<!doctype` then `[` or `<!entity`.
    for (i, window) in head.windows(9).enumerate() {
        if window[0] == b'<'
            && window[1] == b'!'
            && window[2].eq_ignore_ascii_case(&b'd')
            && window[3].eq_ignore_ascii_case(&b'o')
            && window[4].eq_ignore_ascii_case(&b'c')
            && window[5].eq_ignore_ascii_case(&b't')
            && window[6].eq_ignore_ascii_case(&b'y')
            && window[7].eq_ignore_ascii_case(&b'p')
            && window[8].eq_ignore_ascii_case(&b'e')
        {
            // From here to `>` containing `[` means an internal subset.
            let rest = &head[i..];
            if let Some(end) = find_bytes(rest, b">")
                && rest[..end].contains(&b'[')
            {
                return true;
            }
        }
        if window[0] == b'<'
            && window[1] == b'!'
            && window[2].eq_ignore_ascii_case(&b'e')
            && window[3].eq_ignore_ascii_case(&b'n')
            && window[4].eq_ignore_ascii_case(&b't')
            && window[5].eq_ignore_ascii_case(&b'i')
            && window[6].eq_ignore_ascii_case(&b't')
            && window[7].eq_ignore_ascii_case(&b'y')
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = br#"<?xml version="1.0"?>
<svg xmlns="http://www.w3.org/2000/svg" width="120" height="80">
  <rect x="10" y="10" width="100" height="60" fill="red"/>
</svg>"#;

    #[test]
    fn sniffing_tells_svg_from_html_and_png() {
        assert!(looks_like_svg(SIMPLE));
        assert!(looks_like_svg(b"  <!-- c -->\n<svg width=\"4\"></svg>"));
        assert!(looks_like_svg(b"\xEF\xBB\xBF<svg></svg>"));
        assert!(!looks_like_svg(
            b"<!DOCTYPE html><html><body><svg></svg></body></html>"
        ));
        assert!(!looks_like_svg(b"\x89PNG\r\n\x1a\n...."));
        assert!(!looks_like_svg(b"plain text"));
    }

    #[test]
    fn a_small_icon_rasterises_at_natural_size() {
        let decoded = render(SIMPLE).unwrap();
        assert_eq!(decoded.natural, (120, 80));
        assert_eq!(decoded.format_name, "SVG");
        let bitmap = &decoded.frames[0].bitmap;
        assert!(bitmap.is_well_formed());
        assert_eq!((bitmap.width, bitmap.height), (120, 80));
        // The red rect covers the middle: some pixel is opaque and reddish.
        let (pixels, _) = bitmap.bgra.as_chunks::<4>();
        assert!(
            pixels.iter().any(|pixel| pixel[3] == 255 && pixel[2] > 200),
            "the rect must have painted"
        );
    }

    #[test]
    fn a_huge_viewbox_renders_bounded_but_reports_honestly() {
        let big = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20000\" height=\"100\"><rect width=\"20000\" height=\"100\" fill=\"blue\"/></svg>".to_string();
        let decoded = render(big.as_bytes()).unwrap();
        assert_eq!(decoded.natural, (20000, 100));
        let bitmap = &decoded.frames[0].bitmap;
        assert_eq!(bitmap.width, IMAGE_TARGET_LONG_EDGE);
        assert!(bitmap.is_well_formed());
    }

    #[test]
    fn external_references_resolve_to_nothing_not_to_the_network() {
        // An `href` to a file and one to https: neither may be fetched, and
        // neither may fail the render — they paint as nothing.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="40" height="40"><image xlink:href="https://tracker.example/x.png" width="40" height="40"/><image href="/etc/passwd" width="40" height="40"/></svg>"#;
        let decoded = render(svg).unwrap();
        assert!(decoded.frames[0].bitmap.is_well_formed());
    }

    #[test]
    fn garbage_is_unparseable_and_oversize_is_too_large() {
        assert_eq!(
            render(b"<svg><unclosed").unwrap_err(),
            SvgError::Unparseable
        );
        assert_eq!(render(b"just text").unwrap_err(), SvgError::Unparseable);
        let huge = vec![b'a'; SVG_BUDGET + 1];
        assert_eq!(render(&huge).unwrap_err(), SvgError::TooLarge);
    }

    #[test]
    fn a_path_bomb_is_refused_before_rasterising() {
        let mut svg =
            String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\">");
        for _ in 0..SVG_MAX_NODES + 10 {
            svg.push_str("<path d=\"M0 0h1\"/>");
        }
        svg.push_str("</svg>");
        assert_eq!(render(svg.as_bytes()).unwrap_err(), SvgError::TooLarge);
    }
}
