//! Measures the numbers the base's performance budget is written against.
//!
//! **Time to first row** is the one that decides whether the app feels alive;
//! it is the entire justification for streaming listings, and a regression here
//! means someone collected into a `Vec` somewhere.
//!
//! **Time to a fully populated listing** is the one that decides whether the
//! rows you are looking at are finished. Every finding in `docs/audit.html` §4
//! leaves the first two numbers untouched and shows up only here.
//!
//! Each fixture is measured [`BENCH_RUNS`] times and reported at the median,
//! because a single sample is noise and noise forces loose budgets.

use crate::config::{
    BENCH_FIXTURES, BENCH_RUNS, FIRST_ROW_BUDGET, FLAT_POPULATED_BUDGET, FLAT_TOTAL_BUDGET,
    PREVIEW_SWEEP_BUDGET,
};
use anyhow::{Context, Result};
use ferret_vfs::config::METADATA_BATCH_SIZE;
use ferret_vfs::{Entry, ListingDetail, LocalFs, Vfs, VfsPath};
use futures::StreamExt;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Measurement {
    pub label: String,
    pub entries: usize,
    pub errors: usize,
    pub first: Duration,
    pub total: Duration,
    /// First row to the last row that can draw a size and a date.
    pub populated: Duration,
    pub metadata_in_listing: bool,
}

pub fn run(root: &Path) -> Result<Vec<Measurement>> {
    anyhow::ensure!(
        root.exists(),
        "{} does not exist — run `cargo xtask fixture` first",
        root.display()
    );

    let fs = LocalFs::new();
    let mut results = Vec::new();

    for name in BENCH_FIXTURES {
        let dir = root.join(name);
        if !dir.exists() {
            continue;
        }
        let mut runs = Vec::with_capacity(BENCH_RUNS);
        for _ in 0..BENCH_RUNS {
            runs.push(
                measure(&fs, name, &dir)
                    .with_context(|| format!("benchmarking {}", dir.display()))?,
            );
        }
        results.push(median(runs));
    }

    Ok(results)
}

/// The middle run by total time, so one stalled sample cannot set the figure.
fn median(mut runs: Vec<Measurement>) -> Measurement {
    runs.sort_by_key(|m| m.total);
    runs.swap_remove(runs.len() / 2)
}

fn measure(fs: &LocalFs, label: &str, dir: &Path) -> Result<Measurement> {
    let path = VfsPath::local(dir);
    let capabilities = fs.capabilities();

    smol::block_on(async {
        let started = Instant::now();
        let mut first = None;
        let mut errors = 0usize;
        let mut listed: Vec<Entry> = Vec::new();

        // Full: the budget is written against what the file pane actually
        // asks for, which is every field.
        let mut stream = fs.read_dir(&path, ListingDetail::Full);
        while let Some(item) = stream.next().await {
            if first.is_none() {
                first = Some(started.elapsed());
            }
            match item {
                Ok(batch) => listed.extend(batch),
                Err(_) => errors += 1,
            }
        }
        let total = started.elapsed();

        // What the file pane does next, in the same shape: whatever the walk
        // did not fill is chased in `METADATA_BATCH_SIZE` groups. Where the
        // backend fills metadata during the walk there is nothing to chase,
        // and `populated` is simply `total`.
        let pending: Vec<VfsPath> = listed
            .iter()
            .filter(|e| e.metadata.is_none())
            .map(|e| e.path.clone())
            .collect();
        for chunk in pending.chunks(METADATA_BATCH_SIZE) {
            fs.metadata_batch(chunk.to_vec()).await;
        }

        Ok(Measurement {
            label: label.to_string(),
            entries: listed.len(),
            errors,
            first: first.unwrap_or_default(),
            total,
            populated: started.elapsed(),
            metadata_in_listing: capabilities.metadata_in_listing,
        })
    })
}

pub fn report(results: &[Measurement]) -> bool {
    println!(
        "\n{:<12} {:>9} {:>7} {:>12} {:>14} {:>12} {:>11}",
        "FIXTURE", "ENTRIES", "ERRORS", "FIRST ROW", "FULL LISTING", "POPULATED", "PER ENTRY"
    );
    println!("{}", "-".repeat(84));

    let mut ok = true;
    for m in results {
        let per_entry = if m.entries > 0 {
            m.total.as_secs_f64() * 1e6 / m.entries as f64
        } else {
            0.0
        };
        println!(
            "{:<12} {:>9} {:>7} {:>10.1?} {:>12.1?} {:>10.1?} {:>8.2} µs",
            m.label, m.entries, m.errors, m.first, m.total, m.populated, per_entry
        );

        if m.first > FIRST_ROW_BUDGET {
            println!(
                "  ✗ first row took {:.1?}, budget is {:.1?} — is something collecting the \
                 listing before yielding it?",
                m.first, FIRST_ROW_BUDGET
            );
            ok = false;
        }
        if m.label == "flat" && m.total > FLAT_TOTAL_BUDGET {
            println!(
                "  ✗ full listing took {:.1?}, budget is {:.1?}",
                m.total, FLAT_TOTAL_BUDGET
            );
            ok = false;
        }
        if m.label == "flat" && m.populated > FLAT_POPULATED_BUDGET {
            println!(
                "  ✗ rows took {:.1?} to show a size and a date, budget is {:.1?} — is the \
                 metadata tier resolving one path at a time?",
                m.populated, FLAT_POPULATED_BUDGET
            );
            ok = false;
        }
    }

    if let Some(m) = results.first() {
        println!(
            "\nmetadata_in_listing = {} on this platform{}",
            m.metadata_in_listing,
            if m.metadata_in_listing {
                " (size and dates arrive with the names, so POPULATED is FULL LISTING)"
            } else {
                " (size and dates need a second pass)"
            }
        );
    }

    println!(
        "\nMedian of {BENCH_RUNS} runs. These measure the listing only — scroll frame time is \
          measured in the app,\nnot here (see docs/ui-base.html §10), and allocations per frame \
          by the dhat budget in\nferret-ui (see docs/audit.html §11)."
    );

    ok
}

// ---------------------------------------------------------------------------
// Preview sweep — one settled selection per file in `preview/`
// ---------------------------------------------------------------------------

/// What one settled selection cost: the same sniff-then-budgeted-read the
/// pane's pipeline issues, through the real classifier and builders.
pub struct PreviewFile {
    pub name: String,
    pub kind: String,
    pub total: u64,
    pub shown: u64,
    pub elapsed: Duration,
    pub note: String,
}

pub struct PreviewSweep {
    pub files: Vec<PreviewFile>,
    /// VFS read calls issued. A settled selection is a sniff plus one
    /// budgeted read — exactly two; anything more is a re-read the debounce
    /// was supposed to collapse.
    pub reads: usize,
    pub elapsed: Duration,
}

/// Classify, read and build every file in `preview/`, asserting each file
/// lands in its labelled kind and the sweep costs two reads per readable
/// file — the arrow-key contract from `docs/preview-plan.html` §11, minus
/// the debounce, which is a UI concern with its own unit behaviour.
pub fn sweep_preview(root: &Path) -> Result<PreviewSweep> {
    use ferret_preview::{budget_for, config};

    let dir = root.join("preview");
    anyhow::ensure!(
        dir.exists(),
        "{} does not exist — run `cargo xtask fixture --force` first (preview/ is new)",
        dir.display()
    );

    let fs = LocalFs::new();
    let mut names: Vec<String> = Vec::new();
    for item in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let item = item?;
        names.push(item.file_name().to_string_lossy().into_owned());
    }
    names.sort();

    let started = Instant::now();
    let mut files = Vec::with_capacity(names.len());
    let mut reads = 0usize;

    for name in &names {
        let path = VfsPath::local(dir.join(name));
        let file_started = Instant::now();

        // The sniff first: reading megabytes of a gigabyte file to discover
        // it is one is the waste this avoids. A failed sniff is a
        // Gone-adjacent outcome (broken symlink) or Denied, never a retry.
        let sniff = match smol::block_on(fs.read(&path, config::SNIFF_BYTES)) {
            Ok(sniff) => {
                reads += 1;
                sniff
            }
            Err(err) => {
                reads += 1;
                let note = preview_error_note(name, &err);
                files.push(PreviewFile {
                    name: name.clone(),
                    kind: "unreadable".into(),
                    total: 0,
                    shown: 0,
                    elapsed: file_started.elapsed(),
                    note,
                });
                continue;
            }
        };

        let known_size = std::fs::metadata(dir.join(name)).ok().map(|md| md.len());
        let kind = ferret_preview::classify::classify(name, &sniff, known_size);
        let budget = budget_for(&kind, false);
        let bytes = smol::block_on(fs.read(&path, budget))
            .with_context(|| format!("reading {name} under its budget"))?;
        reads += 1;

        let total = known_size.unwrap_or(bytes.len() as u64);
        let base = path.parent().unwrap_or_else(|| VfsPath::local("/"));
        let preview = ferret_preview::build(kind.clone(), bytes, Some(total), base, false);
        describe(
            name,
            &kind,
            &preview,
            total,
            file_started.elapsed(),
            &mut files,
        )?;
    }

    Ok(PreviewSweep {
        files,
        reads,
        elapsed: started.elapsed(),
    })
}

/// The pane's own error wording for an unreadable preview file, plus the
/// sweep's assertion for the two deliberately unreadable fixtures.
fn preview_error_note(name: &str, err: &ferret_vfs::VfsError) -> String {
    use ferret_vfs::VfsError;
    match err {
        VfsError::NotFound { .. } => {
            // A dangling symlink is Gone-adjacent with the target named,
            // never a silent empty pane — and only it may fail this way.
            assert_eq!(
                name, "broken.txt",
                "only the broken symlink may fail its sniff, but {name} did"
            );
            format!("gone: {err}")
        }
        VfsError::PermissionDenied { .. } => {
            // Unreadable except as root; either outcome is honest as long
            // as it is reported, not hidden — and only it may fail this way.
            assert_eq!(
                name, "noperm.txt",
                "only the unreadable file may fail with denial, but {name} did"
            );
            format!("denied: {err}")
        }
        _ => format!("unexpected: {err}"),
    }
}

/// Record one built preview and assert it landed in its labelled kind.
fn describe(
    name: &str,
    kind: &ferret_preview::Kind,
    preview: &ferret_preview::Preview,
    total: u64,
    elapsed: Duration,
    files: &mut Vec<PreviewFile>,
) -> Result<()> {
    use ferret_preview::{Preview, Unsupported};

    let (variant, shown, note) = match preview {
        Preview::Markdown { truncated, .. } => ("markdown", truncated.shown, trunc_note(truncated)),
        Preview::Code {
            language,
            truncated,
            ..
        } => (
            "code",
            truncated.shown,
            format!("{language}{}", trunc_note(truncated)),
        ),
        Preview::Table { table, truncated } => (
            "table",
            truncated.shown,
            format!(
                "{} rows delim={:?}{}",
                table.rows.len(),
                table.delimiter as char,
                trunc_note(truncated)
            ),
        ),
        Preview::Image { image, .. } => (
            "image",
            total,
            format!("{} {} frames", image.format_name, image.frames.len()),
        ),
        Preview::Rendered { by, pages, .. } => {
            ("rendered", total, format!("by={by:?} pages={pages:?}"))
        }
        Preview::Binary { head, size } => ("binary", head.len() as u64, format!("size={size}")),
        Preview::Directory { .. } => ("directory", 0, String::new()),
        Preview::Multiple { .. } => ("multiple", 0, String::new()),
        Preview::Unsupported { reason, .. } => (
            "unsupported",
            0,
            match reason {
                Unsupported::TooLarge { size, budget } => {
                    format!("too-large size={size} budget={budget}")
                }
                Unsupported::NoRenderer => "no-renderer".into(),
                Unsupported::Protected => "protected".into(),
            },
        ),
    };

    expected_kind(name, kind, variant, &note)?;
    files.push(PreviewFile {
        name: name.to_string(),
        kind: kind.display_name(),
        total,
        shown,
        elapsed,
        note: format!("{variant} {note}").trim().to_string(),
    });
    Ok(())
}

fn trunc_note(truncated: &ferret_preview::Truncation) -> String {
    if truncated.is_truncated() {
        format!(" truncated {}/{}", truncated.shown, truncated.total)
    } else {
        String::new()
    }
}

/// The fixture is self-verifying: every file asserts its labelled kind, so
/// a classifier regression fails the bench instead of scrolling past in a
/// log. `variant` is the built preview's shape; provider-specific spot
/// checks ride in `note` (the language or delimiter the file must carry).
fn expected_kind(name: &str, kind: &ferret_preview::Kind, variant: &str, note: &str) -> Result<()> {
    use ferret_preview::Kind;

    let ok = match (name, kind, variant) {
        ("README.md", Kind::Markdown, "markdown") => true,
        ("img.png" | "photo.png", Kind::Image, "image") => note.starts_with("PNG"),
        ("anim.gif", Kind::Image, "image") => note == "GIF 2 frames",
        ("icon.svg", Kind::Svg, "image") => note.starts_with("SVG"),
        ("huge.tiff", Kind::Image, "unsupported") => note.starts_with("too-large"),
        ("photo.heic", Kind::Image, "unsupported") => note == "no-renderer",
        ("report.pdf", Kind::Pdf, "unsupported") => note == "no-renderer",
        ("clip.mp4" | "doc.docx", Kind::Binary, "binary") => true,
        ("data.csv", Kind::Csv, "table") => note.contains("delim=','"),
        ("export.csv", Kind::Csv, "table") => note.contains("delim=';'"),
        ("data.tsv", Kind::Tsv, "table") => note.contains("delim='\\t'"),
        ("utf16.txt" | "crlf.txt", Kind::Code { .. }, "code") => true,
        ("nul.txt", Kind::Binary, "binary") => true,
        ("big.log", Kind::Binary, "binary") => true,
        ("main.rs", Kind::Code { .. }, "code") => note.starts_with("rust"),
        ("app.py", Kind::Code { .. }, "code") => note.starts_with("python"),
        ("app.ts", Kind::Code { .. }, "code") => note.starts_with("typescript"),
        ("main.go", Kind::Code { .. }, "code") => note.starts_with("go"),
        ("service.nix", Kind::Code { .. }, "code") => note.starts_with("nix"),
        ("main.tf", Kind::Code { .. }, "code") => note.starts_with("hcl"),
        ("Dockerfile", Kind::Code { .. }, "code") => note.starts_with("dockerfile"),
        ("config.yaml", Kind::Code { .. }, "code")
        | ("style.css", Kind::Code { .. }, "code")
        | ("run.sh", Kind::Code { .. }, "code")
        | ("query.sql", Kind::Code { .. }, "code")
        | ("data.xml", Kind::Code { .. }, "code") => true,
        // Unreadable on purpose (non-root): no kind assertion, the read
        // outcome above is the assertion. Readable as root: normal text.
        ("noperm.txt", Kind::Code { .. }, "code") => true,
        _ => false,
    };
    anyhow::ensure!(
        ok,
        "{name} classified as {} and built {variant} ({note}) — the fixture disagrees with the classifier",
        kind.display_name(),
    );
    Ok(())
}

pub fn report_preview_sweep(sweep: &PreviewSweep) -> bool {
    println!(
        "\n{:<14} {:<14} {:>12} {:>12} {:>10}  PREVIEW",
        "FILE", "KIND", "TOTAL", "SHOWN", "TIME"
    );
    println!("{}", "-".repeat(84));

    let mut ok = true;
    let mut readable = 0usize;
    let mut unreadable = 0usize;
    for f in &sweep.files {
        println!(
            "{:<14} {:<14} {:>12} {:>12} {:>10.1?}  {}",
            f.name, f.kind, f.total, f.shown, f.elapsed, f.note
        );
        if f.kind == "unreadable" {
            unreadable += 1;
        } else {
            readable += 1;
        }
    }

    // One sniff plus one budgeted read per readable file; one failed sniff
    // for each file that never opened. A third read anywhere is a re-read
    // the debounce was supposed to collapse.
    let want_reads = 2 * readable + unreadable;
    if sweep.reads != want_reads {
        println!(
            "  ✗ {readable} readable + {unreadable} unreadable files cost {} VFS reads, want \
             {want_reads} — something re-read a file the pipeline already held",
            sweep.reads,
        );
        ok = false;
    }
    if sweep.elapsed > PREVIEW_SWEEP_BUDGET {
        println!(
            "  ✗ the sweep took {:.1?}, budget is {:.1?}",
            sweep.elapsed, PREVIEW_SWEEP_BUDGET
        );
        ok = false;
    }

    println!(
        "\n{} files, {} VFS reads, {:.1?} total (budget {:.1?}). The sweep measures the \
         pipeline only —\nframe time is measured in the app, allocations by the dhat budget.",
        sweep.files.len(),
        sweep.reads,
        sweep.elapsed,
        PREVIEW_SWEEP_BUDGET,
    );
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep over a fresh `preview/` only — seconds, not the minutes a
    /// full fixture costs. Fails on hand-built byte mistakes (a GIF that
    /// decodes to one frame, a TIFF the probe cannot read) as well as on
    /// classifier regressions.
    #[test]
    fn preview_sweep_passes_over_a_fresh_fixture() {
        let root =
            std::env::temp_dir().join(format!("ferret-preview-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut report = crate::fixture::Report {
            root: root.clone(),
            created: Vec::new(),
            skipped: Vec::new(),
        };
        crate::fixture::preview(&root, &mut report).unwrap();
        let sweep = sweep_preview(&root).unwrap();
        assert!(
            report_preview_sweep(&sweep),
            "the sweep must stay within budget over its own fixture"
        );
        // The permission fixture must not outlive the test unreadable:
        // removal only needs the parent, but leave nothing behind regardless.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                root.join("preview").join("noperm.txt"),
                std::fs::Permissions::from_mode(0o600),
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
