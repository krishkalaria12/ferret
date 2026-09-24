//! Budgeted text: BOM, encoding, line endings and the line cap.
//!
//! Everything here is pure over bytes already read under budget. Lossy
//! display (U+FFFD) is always labelled and never editable — the edit gate
//! checks [`DecodedText::lossless`] before the button even exists.

use super::config::{TEXT_LINE_CAP, TEXT_MAX_LINE_BYTES};

/// A byte-order mark, when the file opens with one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bom {
    #[default]
    None,
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

impl Bom {
    /// The header label note, when there is one worth showing.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Bom::None => None,
            Bom::Utf8 => Some("UTF-8 BOM"),
            Bom::Utf16Le => Some("UTF-16 LE"),
            Bom::Utf16Be => Some("UTF-16 BE"),
            Bom::Utf32Le => Some("UTF-32 LE"),
            Bom::Utf32Be => Some("UTF-32 BE"),
        }
    }
}

/// The line endings in force, for the header (load-bearing the moment
/// editing is involved) and for losslessly restoring them on save.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineEndings {
    #[default]
    None,
    Lf,
    Crlf,
    Mixed,
}

impl LineEndings {
    pub fn label(self) -> Option<&'static str> {
        match self {
            LineEndings::None => None,
            LineEndings::Lf => Some("LF"),
            LineEndings::Crlf => Some("CRLF"),
            LineEndings::Mixed => Some("mixed endings"),
        }
    }
}

/// Text decoded within budget, with everything a later save must restore.
pub struct DecodedText {
    /// Ready for the editor: line-capped, overlong lines cut.
    pub text: String,
    pub bom: Bom,
    pub endings: LineEndings,
    /// Whether a trailing newline was present (a save must restore it).
    pub trailing_newline: bool,
    /// True when U+FFFD substitutions happened. Labelled read-only only;
    /// unknown invalid text defaults to the hex floor instead.
    pub lossy: bool,
    /// The line cap or line-length cap bit.
    pub truncated_lines: bool,
}

impl DecodedText {
    /// Whether an edit mode may offer to save this buffer.
    ///
    /// Uniform LF or CRLF, a complete lossless decode. Mixed endings, lossy
    /// text and truncated buffers stay read-only until their fidelity
    /// semantics are explicitly implemented.
    pub fn lossless(&self) -> bool {
        !self.lossy && !self.truncated_lines && !matches!(self.endings, LineEndings::Mixed)
    }

    /// Whether a save may be attempted at all.
    ///
    /// Everything [`DecodedText::lossless`] requires, plus a re-encodable
    /// encoding: UTF-32 stays read-only in v1 because its four-byte units
    /// make the editor's character offsets lie about byte offsets, and that
    /// lie is exactly what a save must never act on.
    pub fn saveable(&self) -> bool {
        self.lossless() && !matches!(self.bom, Bom::Utf32Le | Bom::Utf32Be)
    }
}

/// Decode budgeted bytes into displayable text.
///
/// BOM-marked text is decoded strictly by its encoding; otherwise UTF-8 is
/// validated, allowing an incomplete trailing code point only at the read
/// boundary (those bytes belong to the unread tail, not to corruption).
/// Returns `None` for undecodable input, which the pipeline sends to the hex
/// floor instead of showing mojibake.
pub fn decode(bytes: &[u8]) -> Option<DecodedText> {
    let (bom, body) = strip_bom(bytes);
    let (raw, lossy) = match bom {
        Bom::None | Bom::Utf8 => decode_utf8(body),
        Bom::Utf16Le => Some((decode_utf16(body, true)?, false)),
        Bom::Utf16Be => Some((decode_utf16(body, false)?, false)),
        Bom::Utf32Le | Bom::Utf32Be => Some((decode_utf32(body, bom)?, false)),
    }?;
    // A NUL past decoding is binary whatever the BOM claimed.
    if raw.contains('\0') {
        return None;
    }
    let endings = detect_endings(&raw);
    let trailing_newline = raw.ends_with('\n');
    let (text, truncated_lines) = cap_lines(&raw);
    Some(DecodedText {
        text: text.into_owned(),
        bom,
        endings,
        trailing_newline,
        lossy,
        truncated_lines,
    })
}

fn strip_bom(bytes: &[u8]) -> (Bom, &[u8]) {
    if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (Bom::Utf8, body)
    } else if let Some(body) = bytes.strip_prefix(&[0, 0, 0xFE, 0xFF]) {
        (Bom::Utf32Be, body)
    } else if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE, 0, 0]) {
        (Bom::Utf32Le, body)
    } else if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (Bom::Utf16Le, body)
    } else if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (Bom::Utf16Be, body)
    } else {
        (Bom::None, bytes)
    }
}

/// Strict UTF-8, except for a trailing incomplete sequence.
///
/// A prefix read may cut a multibyte character in half; those bytes are
/// dropped (they belong to the tail past the budget), not replaced. Anything
/// else invalid is genuinely undecodable and refuses the text view.
fn decode_utf8(bytes: &[u8]) -> Option<(String, bool)> {
    match str::from_utf8(bytes) {
        Ok(text) => Some((text.to_string(), false)),
        Err(err) => {
            let up_to = err.valid_up_to();
            // `error_len() == None` means the input ended mid-character: a
            // boundary cut, safe to drop. A real error length means corrupt
            // bytes mid-buffer, which is a refusal, not a substitution —
            // lossy display must be explicitly labelled, never silent.
            if err.error_len().is_none() {
                Some((str::from_utf8(&bytes[..up_to]).ok()?.to_string(), false))
            } else {
                None
            }
        }
    }
}

fn decode_utf16(bytes: &[u8], little: bool) -> Option<String> {
    // A budget cut may end mid-code-unit: drop the trailing byte (it belongs
    // to the tail past the budget), mirroring the UTF-8 boundary handling.
    let bytes = if bytes.len() % 2 == 1 {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    // Strict: a lone surrogate is corruption, not text. Decoded streaming
    // from the byte pairs — no intermediate `Vec<u16>` doubling the input.
    let mut out = String::with_capacity(bytes.len() / 2);
    let (pairs, _) = bytes.as_chunks::<2>();
    let mut pairs = pairs.iter().map(|pair| {
        if little {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    while let Some(unit) = pairs.next() {
        if (0xD800..0xDC00).contains(&unit) {
            let low = pairs.next()?;
            if !(0xDC00..0xE000).contains(&low) {
                return None;
            }
            let code = 0x10000 + ((u32::from(unit - 0xD800) << 10) | u32::from(low - 0xDC00));
            out.push(char::from_u32(code)?);
        } else if (0xDC00..0xE000).contains(&unit) {
            return None;
        } else {
            out.push(char::from_u32(u32::from(unit))?);
        }
    }
    Some(out)
}

fn decode_utf32(bytes: &[u8], bom: Bom) -> Option<String> {
    // Same boundary rule as UTF-16: a truncated tail is dropped, not refused.
    let rem = bytes.len() % 4;
    let bytes = if rem == 0 {
        bytes
    } else {
        &bytes[..bytes.len() - rem]
    };
    let mut out = String::with_capacity(bytes.len() / 4);
    let (quads, _) = bytes.as_chunks::<4>();
    for quad in quads {
        let code = match bom {
            Bom::Utf32Le => u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]]),
            _ => u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]]),
        };
        // NUL and surrogates are not text content.
        out.push(char::from_u32(code)?);
    }
    Some(out)
}

fn detect_endings(text: &str) -> LineEndings {
    let bytes = text.as_bytes();
    // Every `\n` not preceded by `\r` is a bare LF; every `\r\n` is a CRLF.
    // (A lone `\r` counts against uniformity rather than for either side.)
    let bare = bytes
        .windows(2)
        .filter(|pair| pair[1] == b'\n' && pair[0] != b'\r')
        .count()
        + usize::from(text.starts_with('\n'));
    let crlf = bytes.windows(2).filter(|pair| pair == b"\r\n").count();
    match (bare > 0, crlf > 0) {
        (false, false) => LineEndings::None,
        (true, false) => LineEndings::Lf,
        (false, true) => LineEndings::Crlf,
        (true, true) => LineEndings::Mixed,
    }
}

/// Enforce the line cap and the per-line byte cap.
///
/// Returns the capped text and whether anything was cut. Cutting is explicit:
/// the footer names it, so a preview never silently hides the broken row
/// someone is looking for. Returns a borrow when nothing is cut, so the
/// common small-file case allocates nothing.
fn cap_lines(text: &str) -> (std::borrow::Cow<'_, str>, bool) {
    // Fast path: count newlines and scan line lengths without building.
    let mut lines = 0usize;
    let mut needs_cut = false;
    for line in text.split_inclusive('\n') {
        lines += 1;
        if lines > TEXT_LINE_CAP || line.len() > TEXT_MAX_LINE_BYTES {
            needs_cut = true;
            break;
        }
    }
    if !needs_cut {
        return (std::borrow::Cow::Borrowed(text), false);
    }
    let mut out = String::new();
    let mut truncated = false;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index >= TEXT_LINE_CAP {
            truncated = true;
            break;
        }
        if line.len() > TEXT_MAX_LINE_BYTES {
            // Cut on a character boundary, keep the newline shape so row
            // numbers below still line up with the file.
            let mut end = TEXT_MAX_LINE_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            out.push_str(&line[..end]);
            out.push('…');
            if line.ends_with('\n') {
                out.push('\n');
            }
            truncated = true;
        } else {
            out.push_str(line);
        }
    }
    // A file with no `\n` at all still passes through the loop once above,
    // so only the unterminated-tail case needs handling: none — the loop
    // already emitted it.
    (std::borrow::Cow::Owned(out), truncated)
}

/// Rebuild file bytes from edited editor text plus the fidelity snapshot.
///
/// The editor works in LF with no BOM; the file may be CRLF with one. `text`
/// is the editor's current value, `original` the [`DecodedText`] it was
/// loaded from. Line endings, the byte-order mark and the presence or absence
/// of a trailing newline are all restored from the snapshot, so a CRLF file
/// stays CRLF even though the editor never saw a `\r`.
///
/// Callers must have checked [`DecodedText::saveable`] first: mixed endings
/// have no single ending to restore, and UTF-32 is refused above, not
/// mangled here.
pub fn reencode(original: &DecodedText, text: &str) -> Vec<u8> {
    // The editor normalises to LF; strip any stray `\r` first so a pasted
    // CRLF fragment cannot smuggle a second ending style into the file.
    let logical: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let joined = match original.endings {
        LineEndings::Crlf => logical.join("\r\n"),
        _ => logical.join("\n"),
    };
    // Restore the trailing-newline state: a file that ended without one must
    // still end without one after an edit, and vice versa.
    let mut body = joined;
    if original.trailing_newline && !body.ends_with('\n') {
        let ending = matches!(original.endings, LineEndings::Crlf)
            .then_some("\r\n")
            .unwrap_or("\n");
        body.push_str(ending);
    } else if !original.trailing_newline && body.ends_with('\n') {
        body.pop();
        if matches!(original.endings, LineEndings::Crlf) && body.ends_with('\r') {
            body.pop();
        }
    }
    match original.bom {
        Bom::None => body.into_bytes(),
        Bom::Utf8 => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(body.as_bytes());
            out
        }
        Bom::Utf16Le | Bom::Utf16Be => {
            let little = matches!(original.bom, Bom::Utf16Le);
            let mut out = if little {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            for unit in body.encode_utf16() {
                let pair = if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                out.extend_from_slice(&pair);
            }
            out
        }
        // Refused by `saveable`; encode UTF-8 rather than corrupt. Callers
        // must gate on `saveable()` first — this arm exists so a missed
        // check degrades visibly (encoding change) rather than panicking,
        // and `debug_assert` catches it in tests.
        Bom::Utf32Le | Bom::Utf32Be => {
            debug_assert!(false, "reencode called on non-saveable UTF-32");
            body.into_bytes()
        }
    }
}

/// Format the head of a binary file: 16 bytes per row, offset gutter, hex,
/// printable ASCII column.
pub fn format_hex(head: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(head.len() * 4);
    for (row, chunk) in head.chunks(16).enumerate() {
        let _ = write!(out, "{:08x}  ", row * 16);
        for i in 0..16 {
            if let Some(byte) = chunk.get(i) {
                let _ = write!(out, "{byte:02x} ");
            } else {
                out.push_str("   ");
            }
            if i == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for byte in chunk {
            let glyph = if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            };
            out.push(glyph);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_with_bom_decodes_and_notes_it() {
        let decoded = decode(b"\xEF\xBB\xBFhello\n").expect("BOM text decodes");
        assert_eq!(decoded.text, "hello\n");
        assert_eq!(decoded.bom, Bom::Utf8);
        assert_eq!(decoded.endings, LineEndings::Lf);
        assert!(decoded.trailing_newline);
        assert!(decoded.lossless());
    }

    #[test]
    fn utf16_decodes_and_lone_surrogates_refuse() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "hi\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode(&bytes).expect("UTF-16 decodes");
        assert_eq!(decoded.text, "hi\n");
        assert_eq!(decoded.bom, Bom::Utf16Le);

        let broken = vec![0xFF, 0xFE, 0x00, 0xD8, b'x', 0];
        assert!(decode(&broken).is_none(), "lone surrogate is not text");
    }

    #[test]
    fn a_cut_multibyte_character_at_the_boundary_is_dropped_not_mojibake() {
        // "hé" cut mid-`é`: the tail belongs past the budget.
        let decoded = decode(b"h\xC3").expect("boundary cut is fine");
        assert_eq!(decoded.text, "h");
        assert!(!decoded.lossy);
    }

    #[test]
    fn corrupt_mid_buffer_bytes_refuse_the_text_view() {
        assert!(decode(b"ok \xFF\xFE nope\n").is_none());
    }

    #[test]
    fn line_endings_are_detected_and_reported() {
        assert_eq!(decode(b"a\nb\n").unwrap().endings, LineEndings::Lf);
        assert_eq!(decode(b"a\r\nb\r\n").unwrap().endings, LineEndings::Crlf);
        assert_eq!(decode(b"a\r\nb\n").unwrap().endings, LineEndings::Mixed);
        assert!(!decode(b"a\r\nb\n").unwrap().lossless());
        assert_eq!(decode(b"single").unwrap().endings, LineEndings::None);
    }

    #[test]
    fn the_line_cap_cuts_with_a_flag() {
        let many: String = (0..TEXT_LINE_CAP + 5).map(|_| "x\n").collect();
        let decoded = decode(many.as_bytes()).expect("long text decodes");
        assert!(decoded.truncated_lines);
        assert!(!decoded.lossless(), "a cut buffer is never saveable");
        assert_eq!(decoded.text.lines().count(), TEXT_LINE_CAP);
    }

    #[test]
    fn minified_single_lines_are_cut_before_layout() {
        let long = "x".repeat(TEXT_MAX_LINE_BYTES + 100) + "\n";
        let decoded = decode(long.as_bytes()).expect("long line decodes");
        assert!(decoded.truncated_lines);
        assert!(decoded.text.contains('…'));
    }

    #[test]
    fn hex_formats_offsets_hex_and_ascii() {
        let out = format_hex(b"hello, world!\n");
        assert!(out.starts_with("00000000  "), "{out}");
        assert!(out.contains("68 65 6c 6c 6f"), "{out}");
        assert!(out.contains("hello, world!."), "{out}");
    }

    #[test]
    fn nul_bytes_are_binary_whatever_the_bom_claimed() {
        assert!(decode(b"\xEF\xBB\xBFa\x00b").is_none());
    }

    #[test]
    fn utf32_is_decoded_but_never_saveable() {
        let mut bytes = vec![0xFF, 0xFE, 0, 0];
        for ch in "hi\n".chars() {
            bytes.extend_from_slice(&(ch as u32).to_le_bytes());
        }
        let decoded = decode(&bytes).expect("UTF-32 decodes");
        assert_eq!(decoded.text, "hi\n");
        assert!(decoded.lossless(), "complete and uniform");
        assert!(!decoded.saveable(), "four-byte units lie about offsets");
    }

    #[test]
    fn reencode_restores_endings_newline_and_bom() {
        // LF, trailing newline: the edit changes one line, nothing else.
        let original = decode(b"a\nb\n").unwrap();
        assert_eq!(reencode(&original, "a\nB\n"), b"a\nB\n");

        // CRLF without a trailing newline: both survive the round trip.
        let original = decode(b"a\r\nb").unwrap();
        assert_eq!(original.endings, LineEndings::Crlf);
        assert!(!original.trailing_newline);
        assert_eq!(reencode(&original, "a\r\nB"), b"a\r\nB");
        // A stray CRLF pasted into the editor cannot smuggle endings in.
        assert_eq!(reencode(&original, "a\r\nB\r\n"), b"a\r\nB");

        // A file that ended with a newline keeps it even if the edit drops
        // the last one; one that did not keeps not having it.
        let original = decode(b"x\n").unwrap();
        assert_eq!(reencode(&original, "y"), b"y\n");
        let original = decode(b"x").unwrap();
        assert_eq!(reencode(&original, "y\n"), b"y");

        // UTF-8 BOM round-trips; UTF-16LE re-encodes its units.
        let original = decode(b"\xEF\xBB\xBFhi\n").unwrap();
        assert_eq!(
            reencode(&original, "hi!\n"),
            b"\xEF\xBB\xBFhi!\n".as_slice()
        );
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "hi\n".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        let original = decode(&utf16).unwrap();
        let mut want = vec![0xFF, 0xFE];
        for unit in "yo\n".encode_utf16() {
            want.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(reencode(&original, "yo\n"), want);
    }

    #[test]
    fn an_unedited_buffer_reencodes_byte_identically() {
        for file in [
            b"plain\n".as_slice(),
            b"crlf\r\nfile\r\n".as_slice(),
            b"bare".as_slice(),
        ] {
            let original = decode(file).unwrap();
            assert!(original.saveable());
            assert_eq!(reencode(&original, &original.text), file);
        }
    }
}
