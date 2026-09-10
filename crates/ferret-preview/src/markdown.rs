//! Markdown resource policy (F-31, PV-D10).
//!
//! A preview pane renders files the user did not write. The single most
//! likely quiet failure is a README with `![](https://tracker.example/x.png)`
//! confirming to a third party that this machine opened the file — GPUI
//! happily fetches it. So every image URL is decided structurally, from the
//! mdast, before the renderer ever sees the source:
//!
//! * Remote (`http(s)`, protocol-relative, any other scheme) → alt text.
//!   Document resources never initiate network access.
//! * Absolute paths, UNC/device paths, traversal (`..`, encoded or split
//!   across separators), and symlink/junction escapes → alt text. Canonical
//!   containment against the document directory is required, not just
//!   lexical normalisation.
//! * Safe relative local paths → recorded for the phase-07 image pipeline
//!   and rendered as alt text until then. Remote documents deny them.
//!
//! Reference images resolve through definitions; HTML `img` blocks are
//! detected as blocks (never by regex over prose) and replaced whole. Raw
//! HTML without images passes through untouched.
//!
//! Two backstops make offset bugs unshippable: every rewritten span is
//! verified to start with `![` (or contain an img tag for HTML blocks), and
//! the sanitized source is re-parsed asserting no image use remains at all.
//! Any structural anomaly falls back to the bounded source view — never to a
//! half-sanitized document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::config::{MARKDOWN_MAX_BLOCKS, MARKDOWN_MAX_DEPTH, MARKDOWN_MAX_IMAGES};

/// How one image URL was decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Safe relative local path, verified contained. Recorded for the image
    /// pipeline; alt text until it renders.
    Local(PathBuf),
    /// `http(s)` or protocol-relative: never fetched, always alt text.
    Remote,
    /// Past the per-document image cap: alt text with a count.
    OverCount,
    /// Blocked with a reason: absolute, traversal, escape, scheme, or
    /// unresolvable. Alt text, counted for the footer.
    Unsafe(UnsafeReason),
}

/// Why an image URL was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsafeReason {
    /// Absolute local path, UNC path, or Windows device path.
    Absolute,
    /// A `..` component, plain or percent-encoded, on either separator.
    Traversal,
    /// Resolved outside the document directory (symlink/junction escape).
    Escape,
    /// A non-http(s) scheme (`data:`, `file:`, `javascript:`, …).
    Scheme,
    /// The location cannot be resolved against the document directory.
    Unresolvable,
}

/// A safe-relative image awaiting the phase-07 renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalImage {
    pub path: PathBuf,
    pub alt: String,
}

/// Sanitized source plus everything the footer and phase 07 need.
#[derive(Debug)]
pub struct Sanitized {
    /// Render this. Contains no image use at all: every image node became
    /// alt text or `[image]`, and HTML img blocks became `[image]`.
    pub source: String,
    /// Safe-relative images, in document order, for the image pipeline.
    pub local: Vec<LocalImage>,
    pub blocked_remote: usize,
    pub blocked_unsafe: usize,
    pub over_count: usize,
}

/// Sanitization refused; the caller shows bounded source with a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SanitizeError {
    /// The source does not parse.
    Parse,
    /// A rewritten span failed verification (not an image where expected).
    Span,
    /// Structural limits exceeded (blocks, depth).
    Limits,
}

/// One image use found in the tree: alt text, raw URL, source span.
struct ImageUse {
    alt: String,
    url: String,
    span: (usize, usize),
    is_html_block: bool,
}

/// Decide every image in `source` against `doc_dir`.
///
/// `doc_dir` is the canonical document directory for local files, `None`
/// for remote documents (which deny local resources outright). Pure over
/// the source except for containment canonicalization, which the caller runs
/// off-thread with everything else.
pub fn sanitize(source: &str, doc_dir: Option<&Path>) -> Result<Sanitized, SanitizeError> {
    let root = parse(source)?;
    let (blocks, depth) = measure(&root);
    if blocks > MARKDOWN_MAX_BLOCKS || depth > MARKDOWN_MAX_DEPTH {
        return Err(SanitizeError::Limits);
    }

    let canonical_base = doc_dir.and_then(|dir| std::fs::canonicalize(dir).ok());
    let uses = collect_images(&root)?;
    let mut out = Sanitized {
        source: String::new(),
        local: Vec::new(),
        blocked_remote: 0,
        blocked_unsafe: 0,
        over_count: 0,
    };

    // Replacements back-to-front so earlier spans stay valid. mdast spans
    // must not overlap; anything else is a structural anomaly worth
    // refusing rather than rendering half-sanitized.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for (index, use_) in uses.iter().enumerate() {
        let verdict = if index >= MARKDOWN_MAX_IMAGES {
            Verdict::OverCount
        } else {
            decide(&use_.url, canonical_base.as_deref())
        };
        match &verdict {
            Verdict::Local(path) => {
                out.local.push(LocalImage {
                    path: path.clone(),
                    alt: use_.alt.clone(),
                });
            }
            Verdict::Remote => out.blocked_remote += 1,
            Verdict::OverCount => out.over_count += 1,
            Verdict::Unsafe(_) => out.blocked_unsafe += 1,
        }
        // Every image becomes alt text in phase 06 — including safe-local
        // ones, until the image pipeline renders them in phase 07.
        edits.push(placeholder(source, use_)?);
    }
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    let mut sanitized = source.to_string();
    let mut floor = usize::MAX;
    for (start, end, replacement) in edits {
        if end > floor || start >= end {
            return Err(SanitizeError::Span);
        }
        floor = start;
        sanitized.replace_range(start..end, &replacement);
    }

    // The backstop: re-parse and prove no image use survived. An
    // offset-semantics bug lands here instead of in a user's network log.
    // (`collect_images` failing is itself a refusal, not a pass.)
    if !collect_images(&parse(&sanitized)?)?.is_empty() {
        return Err(SanitizeError::Span);
    }

    out.source = sanitized;
    Ok(out)
}

fn parse(source: &str) -> Result<markdown::mdast::Node, SanitizeError> {
    markdown::to_mdast(source, &markdown::ParseOptions::gfm()).map_err(|_| SanitizeError::Parse)
}

fn kids(node: &markdown::mdast::Node) -> &[markdown::mdast::Node] {
    node.children().map(Vec::as_slice).unwrap_or(&[])
}

/// Phrasing content: excluded from the block count and block depth.
///
/// Enumerated explicitly — an unknown future variant counts as a block,
/// which over-counts (safe) rather than under-counts.
fn is_inline(node: &markdown::mdast::Node) -> bool {
    use markdown::mdast::Node;
    matches!(
        node,
        Node::Text(_)
            | Node::Emphasis(_)
            | Node::Strong(_)
            | Node::Delete(_)
            | Node::InlineCode(_)
            | Node::InlineMath(_)
            | Node::Link(_)
            | Node::LinkReference(_)
            | Node::Image(_)
            | Node::ImageReference(_)
            | Node::FootnoteReference(_)
            | Node::Break(_)
            | Node::MdxTextExpression(_)
            | Node::MdxJsxTextElement(_)
    )
}

/// Count block-level nodes and measure block nesting depth.
fn measure(root: &markdown::mdast::Node) -> (usize, usize) {
    fn walk(node: &markdown::mdast::Node, block_depth: usize, counts: &mut (usize, usize)) {
        let depth = if is_inline(node) {
            block_depth
        } else {
            counts.0 += 1;
            block_depth + 1
        };
        counts.1 = counts.1.max(depth);
        for child in kids(node) {
            walk(child, depth, counts);
        }
    }
    let mut counts = (0, 0);
    walk(root, 0, &mut counts);
    counts
}

/// Collect image uses: inline images, reference images (resolved through
/// definitions), and HTML blocks containing an `img` tag.
///
/// A use whose node carries no position refuses the whole document: an
/// image the policy cannot locate is an image the policy cannot neutralize.
fn collect_images(root: &markdown::mdast::Node) -> Result<Vec<ImageUse>, SanitizeError> {
    let mut definitions = HashMap::new();
    collect_definitions(root, &mut definitions);
    let mut uses = Vec::new();
    collect_uses(root, &definitions, &mut uses)?;
    Ok(uses)
}

fn collect_definitions(node: &markdown::mdast::Node, definitions: &mut HashMap<String, String>) {
    use markdown::mdast::Node;
    if let Node::Definition(definition) = node {
        definitions.insert(
            normalize_label(&definition.identifier),
            definition.url.clone(),
        );
    }
    for child in kids(node) {
        collect_definitions(child, definitions);
    }
}

fn collect_uses(
    node: &markdown::mdast::Node,
    definitions: &HashMap<String, String>,
    uses: &mut Vec<ImageUse>,
) -> Result<(), SanitizeError> {
    use markdown::mdast::Node;
    match node {
        Node::Image(image) => {
            let Some(span) = position_span(&image.position) else {
                return Err(SanitizeError::Span);
            };
            uses.push(ImageUse {
                alt: image.alt.clone(),
                url: image.url.clone(),
                span,
                is_html_block: false,
            });
        }
        Node::ImageReference(reference) => {
            let key = match reference.reference_kind {
                markdown::mdast::ReferenceKind::Full => normalize_label(&reference.identifier),
                // Shortcut and collapsed references infer the identifier
                // from their content; the label carries the parsed text.
                _ => normalize_label(reference.label.as_deref().unwrap_or(&reference.alt)),
            };
            if let Some(url) = definitions.get(&key) {
                let Some(span) = position_span(&reference.position) else {
                    return Err(SanitizeError::Span);
                };
                uses.push(ImageUse {
                    alt: reference.alt.clone(),
                    url: url.clone(),
                    span,
                    is_html_block: false,
                });
            }
            // A reference without a definition renders as literal text and
            // loads nothing — no policy needed.
        }
        Node::Html(html) => {
            if contains_img_tag(&html.value) {
                let Some(span) = position_span(&html.position) else {
                    return Err(SanitizeError::Span);
                };
                uses.push(ImageUse {
                    alt: String::new(),
                    url: String::new(),
                    span,
                    is_html_block: true,
                });
            }
        }
        _ => {}
    }
    for child in kids(node) {
        collect_uses(child, definitions, uses)?;
    }
    Ok(())
}

/// CommonMark label normalization: collapse whitespace, trim, case-fold.
fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn position_span(position: &Option<markdown::unist::Position>) -> Option<(usize, usize)> {
    let position = position.as_ref()?;
    Some((position.start.offset, position.end.offset))
}

/// Whether an HTML block contains an `img` tag.
///
/// A small scanner over a structurally-identified HTML node — not a regex
/// over prose. Matches `<img`, `< img`, any case; the URL itself is never
/// extracted here (the whole block is replaced).
fn contains_img_tag(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'<' {
            let mut cursor = index + 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if value[cursor..]
                .get(..3)
                .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
                && value[cursor + 3..]
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_ascii_whitespace() || next == '/' || next == '>')
            {
                return true;
            }
        }
        index += 1;
    }
    false
}

/// Decide one image URL. Pure except for containment canonicalization.
///
/// Order matters: scheme (colon before any slash) before protocol-relative,
/// before absolute, before traversal. `a/b:c` is a relative path with a
/// colon, not a scheme; `//host/x` is remote, not absolute.
fn decide(url: &str, canonical_base: Option<&Path>) -> Verdict {
    if let Some(scheme) = url_scheme(url) {
        if scheme == "http" || scheme == "https" {
            return Verdict::Remote;
        }
        return Verdict::Unsafe(UnsafeReason::Scheme);
    }
    if url.starts_with("//") {
        return Verdict::Remote;
    }
    // Strip query and fragment for path analysis.
    let path_text = url.split(['?', '#']).next().unwrap_or_default();
    if path_text.is_empty() {
        return Verdict::Unsafe(UnsafeReason::Scheme);
    }
    if is_absolute_path(path_text) {
        return Verdict::Unsafe(UnsafeReason::Absolute);
    }
    if has_traversal(path_text) {
        return Verdict::Unsafe(UnsafeReason::Traversal);
    }
    let Some(base) = canonical_base else {
        // Remote documents deny local resources outright.
        return Verdict::Remote;
    };
    // Canonical containment, not just lexical normalisation: symlinks and
    // junctions are resolved and must stay under the document directory.
    // The suffix tracks back down when only an ancestor exists (a missing
    // leaf cannot be canonicalized, but its parent can).
    let candidate = base.join(path_text);
    let mut probe = candidate.as_path();
    let mut suffix = PathBuf::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(probe) {
            return if canonical.starts_with(base) {
                Verdict::Local(canonical.join(&suffix))
            } else {
                Verdict::Unsafe(UnsafeReason::Escape)
            };
        }
        if probe == base {
            // The base itself was canonicalized to reach this function; if
            // it fails now the location is unverifiable.
            return Verdict::Unsafe(UnsafeReason::Unresolvable);
        }
        let Some(parent) = probe.parent().filter(|parent| *parent != probe) else {
            return Verdict::Unsafe(UnsafeReason::Unresolvable);
        };
        if let Some(name) = probe.file_name() {
            suffix = Path::new(name).join(&suffix);
        }
        probe = parent;
    }
}

/// The URI scheme, when a colon precedes any slash, query or fragment.
///
/// Shared with link-click handling: links classify exactly like image URLs.
pub fn url_scheme(url: &str) -> Option<String> {
    let prefix = url.split(['/', '?', '#']).next().unwrap_or_default();
    let (scheme, _) = prefix.split_once(':')?;
    if scheme.len() > 1
        && scheme.starts_with(|first: char| first.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, '+' | '-' | '.'))
    {
        Some(scheme.to_ascii_lowercase())
    } else {
        None
    }
}

/// Absolute on either platform: `/x`, `C:\x`, `C:/x`, `\\host\x`.
/// Checked unconditionally — a UNC path is rejected on macOS too.
///
/// Shared with link-click handling.
pub fn is_absolute_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('\\') {
        return true;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return true;
    }
    false
}

/// A `..` component on either separator, plain or percent-encoded.
/// Single-decoded once: double-encoding (`%252e`) decodes to a literal
/// `%2e`, which the filesystem reads literally too.
///
/// Shared with link-click handling: one traversal definition, one place.
pub fn has_traversal(path: &str) -> bool {
    let decoded = decode_percent(path);
    decoded
        .split(['/', '\\'])
        .any(|component| component == "..")
}

/// Percent-decode `%XX` triplets once, ASCII-only. Anything malformed stays
/// literal, exactly as the filesystem would read it.
fn decode_percent(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The placeholder replacing one image use.
///
/// Verified against the source span before replacing: an Image span must
/// open with `![`, an HTML block must contain an img tag. Anything else is
/// a structural anomaly and refuses the whole document. Alt text keeps
/// `![` sequences escaped so the re-parse backstop never sees a new image.
fn placeholder(source: &str, use_: &ImageUse) -> Result<(usize, usize, String), SanitizeError> {
    let (start, end) = use_.span;
    let span = source.get(start..end).ok_or(SanitizeError::Span)?;
    if use_.is_html_block {
        if !contains_img_tag(span) {
            return Err(SanitizeError::Span);
        }
    } else if !span.starts_with("![") {
        return Err(SanitizeError::Span);
    }
    let replacement = if use_.alt.trim().is_empty() {
        "[image]".to_string()
    } else {
        escape_alt(&use_.alt)
    };
    Ok((start, end, replacement))
}

/// Escape alt prose so it can never form link or image syntax with its new
/// neighbours: a trailing `!` meeting a following `[legit]`, or an
/// `![x](y)` inside the alt itself, must stay literal text. Backslash
/// escapes render identically, so the reading experience is unchanged —
/// while the re-parse backstop below would refuse the document otherwise.
fn escape_alt(alt: &str) -> String {
    alt.replace('\\', "\\\\")
        .replace('!', "\\!")
        .replace('[', "\\[")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A document directory: `doc/` with `img/a.png`, plus an outside file
    /// and a symlink pointing out, for the escape tests.
    struct Fixture {
        root: PathBuf,
        doc: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ferret-md-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let doc = root.join("doc");
            fs::create_dir_all(doc.join("img")).unwrap();
            fs::write(doc.join("img/a.png"), b"fake-png").unwrap();
            fs::write(doc.join("notes.md"), b"# hi\n").unwrap();
            fs::write(root.join("secret.txt"), b"secret").unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.join("secret.txt"), doc.join("link-out")).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(root.join("doc"), root.join("link-in")).unwrap();
            Self { root, doc }
        }

        fn doc(&self) -> &Path {
            &self.doc
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    /// No http(s), scheme or protocol-relative URL may survive sanitizing —
    /// this is the zero-network proof. A live request-counting server would
    /// only test GPUI, not the policy; the policy is what must hold.
    fn assert_no_loadable_urls(source: &str) {
        let root = parse(source).expect("sanitized source parses");
        let mut urls = Vec::new();
        fn walk(node: &markdown::mdast::Node, urls: &mut Vec<String>) {
            use markdown::mdast::Node;
            match node {
                Node::Image(image) => urls.push(format!("image:{}", image.url)),
                Node::ImageReference(reference) => urls.push(format!("ref:{}", reference.alt)),
                Node::Html(html) if contains_img_tag(&html.value) => {
                    urls.push("html-img".to_string());
                }
                _ => {}
            }
            for child in kids(node) {
                walk(child, urls);
            }
        }
        walk(&root, &mut urls);
        assert!(urls.is_empty(), "loadable images survived: {urls:?}");
    }

    #[test]
    fn remote_images_become_alt_text() {
        let fixture = Fixture::new();
        let out = sanitize(
            "# T\n\n![tracker](https://tracker.example/x.png)\n\n![logo](http://cdn.example/a.png)\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.blocked_remote, 2);
        assert!(out.local.is_empty());
        assert!(out.source.contains("tracker"));
        assert!(!out.source.contains("tracker.example"));
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn protocol_relative_and_data_urls_are_blocked() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![a](//cdn.example/x.png)\n\n![b](data:image/png;base64,AAAA)\n\n![c](javascript:alert(1))\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.blocked_remote, 1, "protocol-relative counts as remote");
        assert_eq!(out.blocked_unsafe, 2, "data: and javascript: are refused");
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn absolute_paths_and_traversal_are_blocked() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![a](/etc/passwd)\n\n![b](../../../secret.txt)\n\n![c](img%2f..%2f..%2fsecret.txt)\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.blocked_unsafe, 3);
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn windows_paths_are_blocked_on_every_platform() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![a](C:\\Windows\\x.png)\n\n![b](\\\\\\\\host\\\\share\\\\x.png)\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.blocked_unsafe, 2);
    }

    #[test]
    fn safe_relative_images_are_kept_for_the_pipeline() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![diagram](img/a.png)\n\n![sub](./img/a.png)\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.local.len(), 2);
        assert_eq!(out.blocked_remote, 0);
        assert_eq!(out.blocked_unsafe, 0);
        // Phase 06 renders alt text; the paths are recorded for phase 07.
        assert!(out.source.contains("diagram"));
        assert_no_loadable_urls(&out.source);
        assert!(out.local[0].path.ends_with("doc/img/a.png"));
    }

    #[test]
    fn symlink_escapes_are_caught_by_canonical_containment() {
        let fixture = Fixture::new();
        // `link-out` points at root/secret.txt: lexically inside, really outside.
        let out = sanitize("![s](link-out)\n", Some(fixture.doc())).unwrap();
        assert_eq!(out.blocked_unsafe, 1);
        assert!(out.local.is_empty());
    }

    #[test]
    fn links_through_a_symlinked_directory_stay_inside() {
        let fixture = Fixture::new();
        // root/link-in -> root/doc: resolving through it stays contained.
        let via = fixture.root.join("link-in");
        let out = sanitize("![a](img/a.png)\n", Some(&via)).unwrap();
        assert_eq!(out.local.len(), 1, "contained through a symlink");
    }

    #[test]
    fn remote_documents_deny_local_resources() {
        let out = sanitize("![a](img/a.png)\n", None).unwrap();
        assert_eq!(out.blocked_remote, 1);
        assert!(out.local.is_empty());
    }

    #[test]
    fn reference_images_resolve_through_definitions() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![safe][good]\n\n![evil][bad]\n\n[good]: img/a.png\n[bad]: https://evil.example/x.png\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.local.len(), 1);
        assert_eq!(out.blocked_remote, 1);
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn collapsed_and_shortcut_references_resolve() {
        let fixture = Fixture::new();
        let out = sanitize(
            "![img/a.png][]\n\n[img/a.png]: img/a.png\n",
            Some(fixture.doc()),
        )
        .unwrap();
        // Collapsed: label is the content; definition maps it to a local file.
        assert_eq!(out.local.len() + out.blocked_remote + out.blocked_unsafe, 1);
    }

    #[test]
    fn dangling_references_render_as_literal_text() {
        let fixture = Fixture::new();
        let source = "![ghost][missing]\n";
        let out = sanitize(source, Some(fixture.doc())).unwrap();
        assert_eq!(out.source, source, "no definition, nothing to neutralize");
    }

    #[test]
    fn html_img_blocks_become_placeholders() {
        let fixture = Fixture::new();
        let out = sanitize(
            "# T\n\n<div align=\"center\">\n<img src=\"https://evil.example/x.png\" alt=\"badge\">\n</div>\n",
            Some(fixture.doc()),
        )
        .unwrap();
        assert_eq!(out.blocked_remote + out.blocked_unsafe, 1);
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn html_without_images_passes_through() {
        let fixture = Fixture::new();
        let source = "<div>\n\nSome *text*.\n\n</div>\n";
        let out = sanitize(source, Some(fixture.doc())).unwrap();
        assert_eq!(out.source, source);
    }

    #[test]
    fn images_past_the_cap_become_alt_text() {
        let fixture = Fixture::new();
        let mut source = String::new();
        for index in 0..super::super::config::MARKDOWN_MAX_IMAGES + 3 {
            source.push_str(&format!("![i{index}](img/a.png)\n\n"));
        }
        let out = sanitize(&source, Some(fixture.doc())).unwrap();
        assert_eq!(out.local.len(), super::super::config::MARKDOWN_MAX_IMAGES);
        assert_eq!(out.over_count, 3);
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn deep_nesting_falls_back_to_source() {
        let fixture = Fixture::new();
        let mut source = String::from("# T\n\n");
        for _ in 0..super::super::config::MARKDOWN_MAX_DEPTH + 5 {
            source.push_str("> ");
        }
        source.push_str("deep\n");
        assert_eq!(
            sanitize(&source, Some(fixture.doc())).unwrap_err(),
            SanitizeError::Limits
        );
    }

    #[test]
    fn multibyte_positions_rewrite_exactly() {
        let fixture = Fixture::new();
        // Multibyte alt, multibyte prose, multibyte URL-adjacent text: the
        // span backstop would refuse the document if offsets drifted.
        let source = "# héllo wörld\n\n![déjà vu](https://evil.example/x.png)\n\nTëxt après.\n";
        let out = sanitize(source, Some(fixture.doc())).unwrap();
        assert!(out.source.contains("déjà vu"));
        assert!(out.source.contains("Tëxt après."));
        assert!(!out.source.contains("evil.example"));
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn hostile_alt_text_cannot_form_new_syntax() {
        let fixture = Fixture::new();
        // Alt containing image syntax, and alt ending in `!` before a `[`.
        let source = "![see ![x](https://a.example/)](https://evil.example/1.png)\n\n![wow!](https://evil.example/2.png)[legit]\n\n[legit]: https://ok.example/\n";
        let out = sanitize(source, Some(fixture.doc())).unwrap();
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn empty_alt_becomes_a_bracketed_placeholder() {
        let fixture = Fixture::new();
        let out = sanitize("![](https://evil.example/x.png)\n", Some(fixture.doc())).unwrap();
        assert!(out.source.contains("[image]"));
        assert_no_loadable_urls(&out.source);
    }

    #[test]
    fn query_and_fragment_do_not_hide_traversal() {
        let fixture = Fixture::new();
        let out = sanitize("![a](../secret.txt?x=1#frag)\n", Some(fixture.doc())).unwrap();
        assert_eq!(out.blocked_unsafe, 1);
    }

    #[test]
    fn plain_documents_pass_through_untouched() {
        let fixture = Fixture::new();
        let source = "# Title\n\nSome **bold** and `code`.\n\n- a\n- b\n";
        let out = sanitize(source, Some(fixture.doc())).unwrap();
        assert_eq!(out.source, source);
        assert!(out.local.is_empty());
    }
}
