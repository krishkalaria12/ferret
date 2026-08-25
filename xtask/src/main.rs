//! Ferret development tasks.
//!
//! ```text
//! cargo xtask fixture [--force] [--root <dir>]   build the performance fixture
//! cargo xtask bench   [--root <dir>]             measure listing performance
//! cargo xtask clean   [--root <dir>]             remove the fixture
//! ```

mod bench;
mod fixture;

use anyhow::Result;
use std::path::PathBuf;
use std::process::ExitCode;

/// Outside the repository on purpose: 200,000 files inside a Cargo workspace
/// makes every `cargo` invocation and every editor file-watcher crawl.
fn default_root() -> PathBuf {
    std::env::temp_dir().join("ferret-fixture")
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".into());

    let mut root = default_root();
    let mut force = false;
    let mut rest = args.peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--force" | "-f" => force = true,
            "--root" => {
                root = rest
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--root needs a path"))?;
            }
            other => anyhow::bail!("unrecognised argument: {other}"),
        }
    }

    match command.as_str() {
        "fixture" => {
            eprintln!(
                "building the fixture in {} — {} files in flat/ alone, so this takes a minute",
                root.display(),
                fixture::FLAT_ENTRIES
            );
            let report = fixture::build(&root, force)?;
            println!("\nfixture ready at {}", report.root.display());
            for line in &report.created {
                println!("  · {line}");
            }
            for line in &report.skipped {
                println!("  ! {line}");
            }
            println!("\nnext: cargo xtask bench");
            Ok(true)
        }
        "bench" => {
            let results = bench::run(&root)?;
            Ok(bench::report(&results))
        }
        "clean" => {
            if root.exists() {
                // The permission-denied fixture is mode 000, so make it
                // traversable again or the removal fails.
                restore_permissions(&root);
                std::fs::remove_dir_all(&root)?;
                println!("removed {}", root.display());
            } else {
                println!("{} does not exist", root.display());
            }
            Ok(true)
        }
        "help" | "--help" | "-h" => {
            println!("{}", HELP);
            Ok(true)
        }
        other => {
            eprintln!("unknown command: {other}\n\n{HELP}");
            Ok(false)
        }
    }
}

#[cfg(unix)]
fn restore_permissions(root: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let denied = root.join("denied");
    if denied.exists() {
        let _ = std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700));
    }
}

#[cfg(not(unix))]
fn restore_permissions(_root: &std::path::Path) {}

const HELP: &str = "\
Ferret development tasks

  cargo xtask fixture [--force] [--root <dir>]
      Build the performance and correctness fixture tree.
      Defaults to a directory under the system temp dir, deliberately outside
      the repository so Cargo and editor file-watchers do not crawl it.

  cargo xtask bench [--root <dir>]
      Measure time-to-first-row and full-listing time against the fixture.
      Exits non-zero if a budget is exceeded.

  cargo xtask clean [--root <dir>]
      Remove the fixture.
";
