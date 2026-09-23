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
    preview(root, &mut report)?;

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

/// `preview/` — one file per provider and per hazard (`docs/preview-plan.html`
/// §verification). Arrow-keying through this one directory reaches every pane
/// state in §4, which makes it the fastest manual regression check as well as
/// the bench sweep's input.
///
/// Raster fixtures are hand-built bytes, not encoder output: the point is a
/// deterministic, dependency-free fixture, and the bench sweep asserts each
/// file classifies (and decodes, where decodable) as labelled.
pub(crate) fn preview(root: &Path, report: &mut Report) -> Result<()> {
    let dir = root.join("preview");
    fs::create_dir(&dir)?;

    // F-31. Tables, fences, a relative image and a remote image. The relative
    // one must resolve; the remote one must stay alt text (PV-D10).
    fs::write(
        dir.join("README.md"),
        b"# Preview fixture\n\nA table:\n\n| name | kind |\n| ---- | ---- |\n| `main.rs` | code |\n| `photo.png` | image |\n\nA fence:\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\nLocal: ![tile](img.png)\n\nRemote: ![tracker](https://tracker.example/pixel.png)\n\nEscape: ![secret](../../../../.ssh/id_rsa)\n",
    )?;
    fs::write(dir.join("img.png"), png_1x1())?;

    // F-32. A dozen languages, three from the extended set (nix, hcl,
    // dockerfile) — representative source with comments and keywords, so a
    // highlight regression is visible, not just a passing parse.
    let sources: &[(&str, &[u8])] = &[
        ("main.rs", b"// entry point\nfn main() {\n    let greeting = \"hi\";\n    println!(\"{greeting}\");\n}\n"),
        ("app.py", b"# entry point\ndef greet(name: str) -> str:\n    return f\"hi {name}\"\n\nif __name__ == \"__main__\":\n    print(greet(\"ferret\"))\n"),
        ("app.ts", b"// entry point\ninterface Greeter {\n  greet(name: string): string;\n}\n\nexport const greeter: Greeter = {\n  greet: (name) => `hi ${name}`,\n};\n"),
        ("main.go", b"// entry point\npackage main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hi\")\n}\n"),
        ("config.yaml", b"# service config\nserver:\n  host: localhost\n  port: 8080\nlogging:\n  level: debug\n"),
        ("style.css", b"/* pane */\n.preview {\n  display: flex;\n  color: #182331;\n}\n"),
        ("run.sh", b"#!/bin/sh\n# build helper\nset -eu\ncargo xtask fixture --force\n"),
        ("query.sql", b"-- recent files\nSELECT name, len FROM entries\nWHERE len > 1024\nORDER BY name;\n"),
        ("service.nix", b"# extended: nix\n{ stdenv }: stdenv.mkDerivation {\n  name = \"hi\";\n  src = ./.;\n}\n"),
        ("main.tf", b"# extended: hcl\nresource \"aws_instance\" \"web\" {\n  ami = \"ami-123\"\n}\n"),
        ("Dockerfile", b"# extended: dockerfile\nFROM ubuntu:22.04\nRUN apt-get update\nENV FOO=bar\n"),
        ("data.xml", b"<?xml version=\"1.0\"?>\n<!-- extended: xml -->\n<root attr=\"v\"><a/></root>\n"),
    ];
    for (name, body) in sources {
        fs::write(dir.join(name), body)?;
    }

    // F-33. A decodable still, a two-frame animation, a vector image, a
    // header-only 9504x6336 TIFF (the probe must refuse it before any
    // allocation), and a HEIC (recognised, no in-process decoder).
    fs::write(dir.join("photo.png"), png_1x1())?;
    fs::write(dir.join("anim.gif"), gif_2frame_1x1())?;
    fs::write(
        dir.join("icon.svg"),
        br#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="green"/></svg>"#,
    )?;
    fs::write(dir.join("huge.tiff"), tiff_header_only(9504, 6336))?;
    let mut heic = b"....ftypheic".to_vec();
    heic.extend_from_slice(&[0u8; 64]);
    fs::write(dir.join("photo.heic"), heic)?;

    // F-34 and the OS-thumbnail customers: a minimal PDF, an MP4 and a docx.
    // The PDF here exercises classification and page-count/encryption scans,
    // not page rendering — that needs a real document, not a header.
    fs::write(
        dir.join("report.pdf"),
        b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n",
    )?;
    let mut mp4 = b"....ftypisom".to_vec();
    mp4.extend_from_slice(&[0u8; 64]);
    fs::write(dir.join("clip.mp4"), mp4)?;
    let mut docx = b"PK\x03\x04".to_vec();
    docx.extend_from_slice(&[0u8; 64]);
    fs::write(dir.join("doc.docx"), docx)?;

    // F-35. Comma, semicolon-with-quoted-newlines (the European case the
    // frequency heuristic gets wrong), and tab.
    fs::write(
        dir.join("data.csv"),
        b"name,age,city\nada,36,london\ngrace,85,new york\n",
    )?;
    fs::write(
        dir.join("export.csv"),
        b"name;age;notes\nada;36;\"loves\nlogic\"\" deeply\"\"\"\ngrace;85;\"compiler, pioneer\"\n",
    )?;
    fs::write(dir.join("data.tsv"), b"name\tage\tcity\nada\t36\tlondon\n")?;

    // Text hazards: UTF-16 with BOM, CRLF without a trailing newline, and a
    // NUL at byte 4000 wearing a .txt extension (content overrules the name).
    let mut utf16 = vec![0xFF, 0xFE];
    for b in b"hi ferret" {
        utf16.push(*b);
        utf16.push(0);
    }
    fs::write(dir.join("utf16.txt"), utf16)?;
    fs::write(dir.join("crlf.txt"), b"first\r\nsecond\r\nthird-no-newline")?;
    let mut nul = vec![b'#'; 4096];
    nul[4000] = 0;
    fs::write(dir.join("nul.txt"), nul)?;

    // The 4 GB log: sparse, so it costs almost nothing while still making
    // any code that reads a file whole fall over. Reads as zeros, so it
    // classifies binary — the point is the two capped reads, never 4 GB.
    let log = fs::File::create(dir.join("big.log"))?;
    log.set_len(SPARSE_LEN)?;
    drop(log);

    // A broken symlink: Gone-adjacent with the dangling target named, never
    // a silent empty pane. Refused platforms (Windows without Developer
    // Mode) get the directory without it, like `links/`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let _ = symlink(dir.join("nowhere-at-all"), dir.join("broken.txt"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::symlink_file;
        let _ = symlink_file(dir.join("nowhere-at-all"), dir.join("broken.txt"));
    }

    // No read permission: the Denied state. Running as root still reads it,
    // which the sweep tolerates — the file is for the manual check first.
    fs::write(dir.join("noperm.txt"), b"unreadable")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("noperm.txt"), fs::Permissions::from_mode(0o000))?;
    }

    report.created.push(
        "preview/ — one file per provider and per hazard (README, 12 sources, \
         PNG/GIF/SVG/TIFF/HEIC, PDF/MP4/docx, 3 tables, UTF-16/CRLF/NUL, a 4 GB \
         sparse log, a broken symlink, an unreadable file)"
            .into(),
    );
    Ok(())
}

/// A valid 1x1 RGBA PNG (one red pixel), built by hand: signature, IHDR, a
/// zlib stream holding one stored deflate block, IEND.
fn png_1x1() -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let ihdr = vec![
        0, 0, 0, 1, // width
        0, 0, 0, 1, // height
        8, // bit depth
        6, // color type: RGBA
        0, 0, 0, // compression, filter, interlace
    ];
    chunk(&mut out, b"IHDR", &ihdr);
    // Raw scanline: filter byte 0, then RGBA red.
    let raw = [0u8, 0xFF, 0, 0, 0xFF];
    let mut zlib = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF];
    zlib.extend_from_slice(&raw);
    let adler = adler32(&raw);
    zlib.extend_from_slice(&adler.to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    // One running CRC over the type and the data — finalised once, not once
    // per piece. Finalising between the two (the obvious mistake) writes a
    // bad chunk the decoder refuses, and the PNG falls to the hex floor.
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

/// A valid two-frame 1x1 GIF: header, one global color table, then two
/// identical single-pixel frame blocks. Never shown in icon-view tiles —
/// forty running animations is the failure this exercises.
fn gif_2frame_1x1() -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    // Logical screen: 1x1, GCT flag with 2 colors, black background.
    out.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00]);
    // Global color table: black, white.
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF]);
    // Two frames, each: graphics control, 1x1 image descriptor, one-pixel
    // LZW payload, block terminator.
    for _ in 0..2 {
        out.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]);
        out.extend_from_slice(&[0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
        out.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00]);
    }
    out.push(0x3B);
    out
}

/// A header-only little-endian TIFF declaring `width` x `height` with no
/// pixel data behind it. The dimension probe reads the IFD without decoding,
/// so this is exactly the adversarial shape: a 60-megapixel claim in ~100
/// bytes that must end in `TooLarge`, never in an allocation.
fn tiff_header_only(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    // Six entries: width, height, bits-per-sample, compression,
    // photometric, strip offsets.
    let entries: &[(u16, u16, u32, u32)] = &[
        (256, 4, 1, width),
        (257, 4, 1, height),
        (258, 3, 1, 8),
        (259, 3, 1, 1),
        (262, 3, 1, 1),
        (273, 4, 1, 200),
    ];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, kind, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        // SHORT values pack inline in the first two bytes.
        if *kind == 3 {
            out.extend_from_slice(&(*value as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(&[0, 0, 0, 0]);
    out
}
