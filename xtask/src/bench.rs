//! Measures the four numbers the base's performance budget is written against.
//!
//! **Time to first row** is the one that decides whether the app feels alive;
//! it is the entire justification for streaming listings, and a regression here
//! means someone collected into a `Vec` somewhere.

use crate::config::{BENCH_FIXTURES, FIRST_ROW_BUDGET, FLAT_TOTAL_BUDGET};
use anyhow::{Context, Result};
use ferret_vfs::{LocalFs, Vfs, VfsPath};
use futures::StreamExt;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Measurement {
    pub label: String,
    pub entries: usize,
    pub errors: usize,
    pub first: Duration,
    pub total: Duration,
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
        results.push(
            measure(&fs, name, &dir).with_context(|| format!("benchmarking {}", dir.display()))?,
        );
    }

    Ok(results)
}

fn measure(fs: &LocalFs, label: &str, dir: &Path) -> Result<Measurement> {
    let path = VfsPath::local(dir);
    let capabilities = fs.capabilities();

    smol::block_on(async {
        let started = Instant::now();
        let mut first = None;
        let mut entries = 0usize;
        let mut errors = 0usize;

        let mut stream = fs.read_dir(&path);
        while let Some(item) = stream.next().await {
            if first.is_none() {
                first = Some(started.elapsed());
            }
            match item {
                Ok(_) => entries += 1,
                Err(_) => errors += 1,
            }
        }

        Ok(Measurement {
            label: label.to_string(),
            entries,
            errors,
            first: first.unwrap_or_default(),
            total: started.elapsed(),
            metadata_in_listing: capabilities.metadata_in_listing,
        })
    })
}

pub fn report(results: &[Measurement]) -> bool {
    println!(
        "\n{:<12} {:>9} {:>7} {:>14} {:>14} {:>12}",
        "FIXTURE", "ENTRIES", "ERRORS", "FIRST ROW", "FULL LISTING", "PER ENTRY"
    );
    println!("{}", "-".repeat(74));

    let mut ok = true;
    for m in results {
        let per_entry = if m.entries > 0 {
            m.total.as_secs_f64() * 1e6 / m.entries as f64
        } else {
            0.0
        };
        println!(
            "{:<12} {:>9} {:>7} {:>12.1?} {:>12.1?} {:>9.2} µs",
            m.label, m.entries, m.errors, m.first, m.total, per_entry
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
    }

    if let Some(m) = results.first() {
        println!(
            "\nmetadata_in_listing = {} on this platform{}",
            m.metadata_in_listing,
            if m.metadata_in_listing {
                " (size and dates arrive with the names)"
            } else {
                " (size and dates need a second pass)"
            }
        );
    }

    println!(
        "\nNote: these measure the listing only. Scroll frame time is measured in the app, \
         not here — see docs/ui-base.html §10."
    );

    ok
}
