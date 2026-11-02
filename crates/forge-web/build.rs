//! With the feature `embed-ui` only: refuse to build without a usable
//! `web/dist` (ADR-0022 §2). Never runs npm or touches the network — the CI
//! web job (or `npm run build` in `web/`) makes `dist` first.
//!
//! `dist` must hold `index.html` and flat files (no sub-directory) whose
//! extensions are on the allow-list, plus `<file>.br` / `<file>.gz` next to
//! an allowed file. Anything else would be an asset forge cannot type, so
//! it fails here instead of shipping.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Kept equal to `src/ui.rs` `EXTENSIONS` (a unit test compares them).
const ALLOWED: &[&str] = &["js", "css", "woff2", "svg", "png", "ico", "json", "txt"];

fn main() -> ExitCode {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EMBED_UI");
    if std::env::var_os("CARGO_FEATURE_EMBED_UI").is_none() {
        return ExitCode::SUCCESS;
    }
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let dist = manifest.join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    match check(&dist) {
        Ok(()) => ExitCode::SUCCESS,
        Err(problems) => {
            for p in &problems {
                println!("cargo:warning=embed-ui: {p}");
                eprintln!("embed-ui: {p}");
            }
            eprintln!(
                "embed-ui: build the UI first (cd web && npm ci --ignore-scripts && npm run build), \
                 or build without --features embed-ui"
            );
            ExitCode::FAILURE
        }
    }
}

fn check(dist: &Path) -> Result<(), Vec<String>> {
    let entries = std::fs::read_dir(dist)
        .map_err(|e| vec![format!("{} is not readable: {e}", dist.display())])?;
    let mut names = Vec::new();
    let mut problems = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                problems.push(format!("reading {}: {e}", dist.display()));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(t) if t.is_file() => names.push(name),
            Ok(_) => problems.push(format!("{name}: not a plain file (assets must be flat)")),
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }
    if !names.iter().any(|n| n == "index.html") {
        problems.push(format!("{}/index.html is missing", dist.display()));
    }
    for name in &names {
        if name == "index.html" {
            continue;
        }
        let base = name
            .strip_suffix(".br")
            .or_else(|| name.strip_suffix(".gz"));
        let typed = base.unwrap_or(name);
        let ext = typed.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if !ALLOWED.contains(&ext) {
            problems.push(format!("{name}: extension not allow-listed"));
        } else if base.is_some_and(|b| !names.iter().any(|n| n == b)) {
            problems.push(format!("{name}: compressed file without its original"));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
