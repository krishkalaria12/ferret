//! The preview pane's half of the allocations-per-frame budget
//! (`docs/preview-plan.html` §11, `docs/audit.html` §11).
//!
//! # What it measures, precisely
//!
//! Not a rendered frame — same documented limit as
//! [`crate::folder::alloc_budget`]. What this drives is **the questions a
//! frame asks of a computed preview**: the variant tag, the stored header
//! and footer strings, the language, the table's already-parsed rows, the
//! image's already-decoded dimensions. One clone of an already-computed
//! footer per pane per frame is the entire allowance (mirroring
//! `render_pane`, which clones the stored footer); anything that re-derives
//! the language, re-scans the text, or rebuilds a bitmap fails here.
//!
//! # The sharp assertion
//!
//! Two legs hold the same ten variants with identical totals and identical
//! truncation, differing only in payload size (13 bytes vs 2 MiB of text,
//! 12 bytes vs 64 KiB of hex, a short vs a long table). The per-frame reads
//! must allocate **identically** across legs: that equality is what proves
//! the render path is O(the pane), never O(the file). Payloads that are
//! bounded by construction — the downscaled bitmap, the refused TIFF — are
//! identical in both legs, which is the point being proven about them.
//!
//! Run with `just budget`.

use super::{LoadOutcome, preview_outcome};
use ferret_preview::{Kind, Preview, Truncation, Unsupported};
use ferret_vfs::{Entry, EntryKind, Metadata, VfsPath};
use std::time::SystemTime;

/// Ten files of ten kinds — the arrow-through-kinds shape from §11.
const FILES: usize = 10;

/// Frames to average over.
const FRAMES: usize = 20;

/// Heap allocations one preview may cost per frame: the header title, which
/// is re-derived per frame and filename-bounded (measured: exactly 1). The
/// stored detail, footer and page-label handles are `SharedString` clones
/// (refcount bumps, no heap blocks) and everything else is a read of a
/// reference, a `Copy`, or an inline `SmolStr`. Anything O(the file) lands
/// orders of magnitude above this, which is the point — a tripwire, not a
/// stopwatch.
const ALLOCATIONS_PER_PREVIEW: u64 = 1;

fn entry(name: &str, len: u64) -> Entry {
    Entry::new(
        VfsPath::local(format!("/preview/{name}")),
        name,
        EntryKind::File,
    )
    .with_metadata(Some(Metadata {
        len,
        modified: Some(SystemTime::UNIX_EPOCH),
        hidden: false,
        name_hidden: false,
        platform_hidden: false,
    }))
}

/// The same ten variants in both legs: identical totals and truncation, and
/// only the payload sizes differ. `capped` switches the text, table and hex
/// payloads from tiny to budget-sized.
fn ten_outcomes(capped: bool) -> Vec<LoadOutcome> {
    let text = if capped {
        "a".repeat(ferret_preview::config::TEXT_BUDGET)
    } else {
        "fn main() {}\n".to_string()
    };
    let markdown = if capped {
        "# t\n\n".repeat(ferret_preview::config::MARKDOWN_BUDGET / 5)
    } else {
        "# hi\n".to_string()
    };
    let csv = if capped {
        let mut big = "name,age\n".to_string();
        for i in 0..20_000 {
            big.push_str(&format!("name-{i:06},36\n"));
        }
        big
    } else {
        "name,age\nada,36\n".to_string()
    };
    let hex = if capped {
        vec![0xABu8; ferret_preview::config::HEX_BUDGET]
    } else {
        vec![0xABu8; 12]
    };

    // Identical in both legs: the truncation — and therefore the footer —
    // never depends on the payload size, only on shown-vs-total.
    let truncated = Truncation {
        shown: 1_000,
        total: 2_000_000,
    };
    let base = VfsPath::local("/preview");

    let png = ferret_preview::image::decode(&png_bytes()).expect("the budget PNG decodes");
    let table = ferret_preview::table::parse(&csv, b',');
    // Bounded by construction: the downscaled bitmap handed to the view is
    // the same bytes in both legs, which is the property under test.
    let bitmap = ferret_preview::Bitmap {
        width: 64,
        height: 64,
        bgra: vec![0u8; 64 * 64 * 4],
        has_transparency: false,
    };

    let previews: Vec<(&str, Preview)> = vec![
        (
            "main.rs",
            Preview::Code {
                text,
                language: "rust".into(),
                truncated,
            },
        ),
        (
            "README.md",
            Preview::Markdown {
                source: markdown,
                base: base.clone(),
                truncated,
            },
        ),
        ("data.csv", Preview::Table { table, truncated }),
        (
            "photo.png",
            Preview::Image {
                image: png,
                truncated,
            },
        ),
        (
            "report.pdf",
            Preview::Rendered {
                bitmap,
                kind: Kind::Pdf,
                pages: Some(ferret_preview::Pages {
                    current: 1,
                    total: 3,
                }),
                by: ferret_preview::RenderedBy::Pdfium,
            },
        ),
        (
            "core.bin",
            Preview::Binary {
                head: hex,
                size: 2_000_000,
            },
        ),
        ("docs", Preview::Directory { entries: Some(42) }),
        (
            "many",
            Preview::Multiple {
                count: 12,
                bytes: 240_000_000,
                kinds: smallvec::SmallVec::new(),
            },
        ),
        (
            "song.mp3",
            Preview::Unsupported {
                kind: Kind::Binary,
                reason: Unsupported::NoRenderer,
            },
        ),
        (
            "secret.pdf",
            Preview::Unsupported {
                kind: Kind::Pdf,
                reason: Unsupported::Protected,
            },
        ),
    ];

    previews
        .into_iter()
        .map(|(name, preview)| {
            let entry = entry(name, 2_000_000);
            preview_outcome(&entry, preview, 2_000_000, false, None)
        })
        .collect()
}

/// One frame's worth of what the pane asks of each computed preview.
///
/// Mirrors `render_pane`, `render_header` and `render_pdf_bar` exactly:
/// the header title is re-derived per frame (`display_name`, filename-
/// bounded), the stored detail/footer/page-label handles are cloned
/// (`SharedString`: refcount bumps, no heap blocks), and everything else is
/// read by reference. The results fold into a return value so nothing can
/// be optimised away.
///
/// What this proves is the gate: payload sizes (2 MiB of text vs 13 bytes)
/// must not change the allocation count. Anything O(the file) — a text
/// clone, a re-derived language, a re-scan — lands orders of magnitude
/// above the allowance, which is the point: a tripwire, not a stopwatch.
fn read_frame(outcomes: &[LoadOutcome]) -> usize {
    let mut sink = 0usize;
    for outcome in outcomes {
        // The header title, re-derived per frame like `render_header` does.
        // Filename-bounded (≤57 chars inline, one heap block past that),
        // never file-bounded.
        let title = match outcome {
            LoadOutcome::Text { entry, .. }
            | LoadOutcome::Markdown { entry, .. }
            | LoadOutcome::Table { entry, .. }
            | LoadOutcome::Image { entry, .. }
            | LoadOutcome::Pdf { entry, .. }
            | LoadOutcome::Unsupported { entry, .. } => {
                super::PreviewPanel::display_name(entry.name())
            }
            LoadOutcome::Error { name, .. } => super::PreviewPanel::display_name(name),
        };
        sink += title.len();
        match outcome {
            LoadOutcome::Text {
                detail,
                text,
                language,
                wrap,
                footer,
                ..
            } => {
                sink += detail.len();
                sink += footer.clone().map_or(0, |f| f.len());
                sink += text.len();
                sink += language.len();
                sink += *wrap as usize;
            }
            LoadOutcome::Markdown {
                detail,
                original,
                sanitized,
                footer,
                ..
            } => {
                sink += detail.len();
                sink += footer.clone().map_or(0, |f| f.len());
                sink += original.len() + sanitized.len();
            }
            LoadOutcome::Table {
                detail,
                table,
                footer,
                ..
            } => {
                sink += detail.len();
                sink += footer.clone().map_or(0, |f| f.len());
                sink += table.rows.len();
                sink += table.header.as_ref().map_or(0, Vec::len);
                if let Some(row) = table.rows.first() {
                    sink += row.len();
                    if let Some(cell) = row.first() {
                        sink += cell.len();
                    }
                }
            }
            LoadOutcome::Image {
                detail,
                image,
                footer,
                ..
            } => {
                sink += detail.len();
                sink += footer.clone().map_or(0, |f| f.len());
                sink += (image.natural.0 + image.natural.1) as usize;
                sink += image.frames.len();
                if let Some(frame) = image.frames.first() {
                    sink += (frame.bitmap.width + frame.bitmap.height) as usize;
                }
            }
            LoadOutcome::Pdf {
                detail, pdf, page, ..
            } => {
                // The bar reads the preformatted page label (a handle clone,
                // like the footer above): page turns reformat at apply time,
                // never per frame.
                sink += detail.len();
                sink += pdf.len();
                sink += page.pages_total as usize;
                sink += (page.bitmap.width + page.bitmap.height) as usize;
            }
            LoadOutcome::Unsupported {
                detail, message, ..
            } => {
                sink += detail.len() + message.len();
            }
            LoadOutcome::Error { name, message } => {
                sink += name.len() + message.len();
            }
        }
    }
    sink
}

/// Minimal 1x1 PNG for the image leg (signature, IHDR, one stored deflate
/// block, IEND). CRCs are correct — a corrupt PNG would fall to the hex
/// floor and stop exercising the image arm.
fn png_bytes() -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let ihdr = vec![0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
    chunk(&mut out, b"IHDR", &ihdr);
    let raw = [0u8, 0xFF, 0, 0, 0xFF];
    let mut zlib = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF];
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32_update(crc32_update(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF;
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[gpui::test]
async fn preview_allocation_budget(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| cx.set_global(crate::state::load_state()));

    let small = ten_outcomes(false);
    let capped = ten_outcomes(true);
    assert_eq!(small.len(), FILES);
    assert_eq!(capped.len(), FILES);

    let profiler = dhat::Profiler::builder().testing().build();

    // Warm up both legs: the first pass resolves anything lazily
    // initialised, so the measured frames see steady state only.
    std::hint::black_box(read_frame(&small));
    std::hint::black_box(read_frame(&capped));

    let before_small = dhat::HeapStats::get();
    for _ in 0..FRAMES {
        std::hint::black_box(read_frame(&small));
    }
    let after_small = dhat::HeapStats::get();

    let before_capped = dhat::HeapStats::get();
    for _ in 0..FRAMES {
        std::hint::black_box(read_frame(&capped));
    }
    let after_capped = dhat::HeapStats::get();
    drop(profiler);

    let small_allocs = after_small.total_blocks - before_small.total_blocks;
    let capped_allocs = after_capped.total_blocks - before_capped.total_blocks;
    println!(
        "{small_allocs} allocations over {FRAMES} frames of {FILES} small previews; \
         {capped_allocs} over the capped leg"
    );

    let budget = ALLOCATIONS_PER_PREVIEW * (FRAMES * FILES) as u64;
    assert!(
        small_allocs <= budget,
        "{small_allocs} heap allocations reading {FILES} previews over {FRAMES} frames, budget \
         is {budget}. Something on the render path allocates per preview — a clone that is \
         not a shared handle, a re-derived language, or a re-scan of the text."
    );
    assert_eq!(
        capped_allocs, small_allocs,
        "capped payloads allocated {capped_allocs} blocks where small ones allocated \
         {small_allocs}: the per-frame read scales with the file, not the pane."
    );
}
