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
