//! Builds the performance and correctness fixture tree.
//!
//! One tree that hits every cliff a file manager falls off. The point is that
//! "it works on my repo" proves nothing — every number in the performance
//! budget is measured against `flat/`, and every awkward-name and broken-link
//! bug is reproducible here rather than only on a user's disk.

use crate::config::{DEEP_LEVELS, EXTENSIONS, FIXTURE_MAX_THREADS, FLAT_ENTRIES, SPARSE_LEN};
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Report {
    pub root: PathBuf,
    pub created: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn build(root: &Path, force: bool) -> Result<Report> {
    if root.exists() {
        if !force {
            anyhow::bail!(
                "{} already exists — pass --force to rebuild it",
                root.display()
            );
        }
        eprintln!("removing existing {}", root.display());
        fs::remove_dir_all(root).with_context(|| format!("removing {}", root.display()))?;
    }
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;

    let mut report = Report {
        root: root.to_path_buf(),
        created: Vec::new(),
        skipped: Vec::new(),
    };

    flat(root, &mut report)?;
    deep(root, &mut report)?;
    awkward_names(root, &mut report)?;
    links(root, &mut report)?;
    denied(root, &mut report)?;
    sparse(root, &mut report)?;
    types(root, &mut report)?;
    realistic(root, &mut report)?;

    Ok(report)
}

/// `flat/` — the scroll target. 200,000 entries in one directory.
fn flat(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("flat");
    fs::create_dir(&dir)?;

    // Creating 200k files is the slow part of the whole fixture, so it is split
    // across threads. Names are zero-padded so lexical and natural order agree
    // and a sort bug cannot hide behind them.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(FIXTURE_MAX_THREADS);
    let per = FLAT_ENTRIES.div_ceil(threads);

    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for t in 0..threads {
            let dir = dir.clone();
            handles.push(scope.spawn(move || -> Result<()> {
                let start = t * per;
                let end = ((t + 1) * per).min(FLAT_ENTRIES);
                for i in start..end {
                    let ext = EXTENSIONS[i % EXTENSIONS.len()];
                    let path = dir.join(format!("entry-{i:06}.{ext}"));
                    // Non-empty and varying, so the Size column has something
                    // to sort and right-align.
                    let body = vec![b'x'; i % 997];
                    fs::write(&path, &body)
                        .with_context(|| format!("writing {}", path.display()))?;
                }
                Ok(())
            }));
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("fixture thread panicked"))??;
        }
        Ok(())
    })?;

    report
        .created
        .push(format!("flat/ — {FLAT_ENTRIES} entries in one directory"));
    Ok(())
}

/// `deep/` — 512 nested levels. Catches recursion limits and, on Windows, the
/// legacy 260-character path cap.
fn deep(root: &Path, report: &mut Report) -> Result<()> {
    let mut path = root.join("deep");
    fs::create_dir(&path)?;
    let mut reached = 0;
    for level in 0..DEEP_LEVELS {
        path = path.join(format!("l{level:03}"));
        // Stops when the platform's total path limit is reached — `PATH_MAX`
        // (1024) on macOS, or 260 on Windows without long-path support. Record
        // where rather than aborting: the partial tree is exactly the awkward
        // case worth testing, and how deep it got is itself a useful fact.
        if fs::create_dir(&path).is_err() {
            break;
        }
        reached = level + 1;
    }
    let _ = fs::write(path.join("bottom.txt"), b"bottom");

    if reached == DEEP_LEVELS {
        report.created.push(format!("deep/ — {reached} levels"));
    } else {
        report.created.push(format!(
            "deep/ — {reached} levels (the platform's path-length limit stopped it short of \
             {DEEP_LEVELS}; that boundary is the thing to test)"
        ));
    }
    Ok(())
}

/// `names/` — everything that breaks naive string handling.
fn awkward_names(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("names");
    fs::create_dir(&dir)?;

    let mut made = 0;
    let attempt = |name: String, made: &mut i32| {
        if fs::write(dir.join(&name), b"x").is_ok() {
            *made += 1;
        }
    };

    attempt("a".repeat(255), &mut made); // maximum component length
    attempt("with spaces in it.txt".into(), &mut made);
    attempt("  leading-and-trailing  .txt".into(), &mut made);
    attempt(".hidden-dotfile".into(), &mut made);
    attempt("..double-dot".into(), &mut made);
    attempt("no-extension".into(), &mut made);
    attempt("archive.tar.gz".into(), &mut made);
    attempt("UPPER.PNG".into(), &mut made);
    attempt("emoji-🦦-otter.txt".into(), &mut made);
    attempt("中文文件名.txt".into(), &mut made);
    attempt("عربى.txt".into(), &mut made);
    // The same word precomposed (NFC) and decomposed (NFD). macOS normalises
    // to NFD, so these may collapse into one file — that collapse is itself
    // the thing worth seeing.
    attempt("café-nfc.txt".into(), &mut made);
    attempt("cafe\u{0301}-nfd.txt".into(), &mut made);
    attempt("tab\tand\nnewline.txt".into(), &mut made);
    attempt("quote'and\"double.txt".into(), &mut made);
    attempt("semi;colon&amp.txt".into(), &mut made);
    // Reserved on Windows; these simply fail there, which is correct.
    attempt("CON".into(), &mut made);
    attempt("aux.txt".into(), &mut made);

    report.created.push(format!(
        "names/ — {made} awkward names (some refused by the OS, by design)"
    ));
    Ok(())
}

/// `links/` — working, broken and cyclic symlinks.
fn links(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("links");
    fs::create_dir(&dir)?;
    fs::create_dir(dir.join("real-dir"))?;
    fs::write(dir.join("real-file.txt"), b"target")?;

    if make_links(&dir).is_ok() {
        report
            .created
            .push("links/ — working, broken and cyclic symlinks".into());
    } else {
        report.skipped.push(
            "links/ — symlink creation refused. On Windows this needs Developer Mode or an \
             elevated shell; the directory exists but is empty of links."
                .into(),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn make_links(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;
    symlink(dir.join("real-file.txt"), dir.join("to-file"))?;
    symlink(dir.join("real-dir"), dir.join("to-dir"))?;
    symlink(dir.join("nowhere-at-all"), dir.join("broken"))?;
    // A loop: any walker that follows links without tracking visits will hang.
    symlink(dir.join("cycle-b"), dir.join("cycle-a"))?;
    symlink(dir.join("cycle-a"), dir.join("cycle-b"))?;
    Ok(())
}

#[cfg(windows)]
fn make_links(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    symlink_file(dir.join("real-file.txt"), dir.join("to-file"))?;
    symlink_dir(dir.join("real-dir"), dir.join("to-dir"))?;
    symlink_file(dir.join("nowhere-at-all"), dir.join("broken"))?;
    symlink_dir(dir.join("cycle-b"), dir.join("cycle-a"))?;
    symlink_dir(dir.join("cycle-a"), dir.join("cycle-b"))?;
    Ok(())
}

/// `denied/` — a directory the user cannot read, for the permission-denied state.
fn denied(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("denied");
    fs::create_dir(&dir)?;
    fs::write(dir.join("secret.txt"), b"unreadable")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000))?;
        report
            .created
            .push("denied/ — mode 000; `chmod 700` it before deleting the fixture".into());
    }
    #[cfg(not(unix))]
    {
        report.skipped.push(
            "denied/ — needs an ACL to be genuinely unreadable on Windows; created readable. \
             Deny your own account Read on it by hand to exercise that state."
                .into(),
        );
    }
    Ok(())
}

/// `sparse/` — a file that claims 4 GB. Catches size formatting and any code
/// that would try to read a file whole.
fn sparse(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("sparse");
    fs::create_dir(&dir)?;
    let path = dir.join("huge.bin");
    let file = fs::File::create(&path)?;
    // `set_len` leaves a hole on both APFS and NTFS rather than writing 4 GB.
    file.set_len(SPARSE_LEN)?;
    drop(file);

    let mut small = fs::File::create(dir.join("zero-bytes.bin"))?;
    small.flush()?;

    report
        .created
        .push("sparse/ — a 4 GB sparse file and a zero-byte file".into());
    Ok(())
}

/// `types/` — one file per extension, for icon dispatch.
fn types(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("types");
    fs::create_dir(&dir)?;
    for ext in EXTENSIONS {
        fs::write(dir.join(format!("sample.{ext}")), b"sample")?;
    }
    fs::write(dir.join("Makefile"), b"all:")?;
    fs::write(dir.join("Dockerfile"), b"FROM scratch")?;
    fs::write(dir.join(".gitignore"), b"/target")?;

    report.created.push(format!(
        "types/ — {} extensions plus extension-less well-known names",
        EXTENSIONS.len()
    ));
    Ok(())
}

/// `realistic/` — an ordinary project tree, for eyeballing the UI on something
/// that looks like real work rather than synthetic noise.
fn realistic(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("realistic");
    let layout: &[(&str, &[&str])] = &[
        ("src", &["main.rs", "lib.rs", "config.rs", "error.rs"]),
        ("src/ui", &["mod.rs", "sidebar.rs", "table.rs"]),
        ("tests", &["integration.rs", "fixtures.rs"]),
        ("docs", &["README.md", "CHANGELOG.md", "design.md"]),
        ("assets", &["icon.png", "logo.svg"]),
        (".git", &["HEAD", "config", "index"]),
        ("target/debug", &["app", "app.d"]),
    ];
    for (folder, files) in layout {
        let sub = dir.join(folder);
        fs::create_dir_all(&sub)?;
        for file in *files {
            fs::write(sub.join(file), b"// sample\n")?;
        }
    }
    fs::write(dir.join("Cargo.toml"), b"[package]\nname = \"sample\"\n")?;
    fs::write(dir.join("README.md"), b"# Sample\n")?;

    report
        .created
        .push("realistic/ — an ordinary project tree, including a .git directory".into());
    Ok(())
}
