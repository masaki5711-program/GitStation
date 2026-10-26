//! Regenerates `Git/docs/errors/` from the registry.
//!
//! Usage (from `Git/`): `cargo run -p forge-errors --bin gen-catalog`
//! With `--check`, exits 1 if the committed catalog differs (no writes).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs};

use forge_errors::{catalog_files, ErrorCode, ForgeError};

fn main() -> ExitCode {
    let check = env::args().any(|a| a == "--check");
    let dir = catalog_dir();
    match run(&dir, check) {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gen-catalog: {e}");
            ExitCode::FAILURE
        }
    }
}

fn catalog_dir() -> PathBuf {
    // crates/forge-errors -> ../../docs/errors
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("errors")
}

fn run(dir: &Path, check: bool) -> Result<String, ForgeError> {
    let io = |e: std::io::Error, path: &Path| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "gen-catalog")
            .with_context("path", path.display())
            .with_source(e)
    };
    let files = catalog_files();
    if check {
        let stale: Vec<String> = files
            .iter()
            .filter(|(name, body)| {
                fs::read_to_string(dir.join(name)).ok().as_deref() != Some(body.as_str())
            })
            .map(|(name, _)| name.clone())
            .collect();
        return if stale.is_empty() {
            Ok(format!("catalog up to date ({} files)", files.len()))
        } else {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "gen-catalog --check")
                .with_context("stale", stale.join(",")))
        };
    }
    fs::create_dir_all(dir).map_err(|e| io(e, dir))?;
    // Remove pages for codes that no longer exist so the catalog cannot drift.
    if let Ok(read) = fs::read_dir(dir) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("FG-") && !files.iter().any(|(f, _)| *f == name) {
                fs::remove_file(entry.path()).map_err(|e| io(e, &entry.path()))?;
            }
        }
    }
    for (name, body) in &files {
        let path = dir.join(name);
        fs::write(&path, body).map_err(|e| io(e, &path))?;
    }
    Ok(format!("wrote {} files to {}", files.len(), dir.display()))
}
