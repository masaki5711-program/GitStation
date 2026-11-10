#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-db's pipeline commands (cancel, retry, manual run) take any
//! `decide` closure, so a caller could pass `|_| Ok(())` and skip the
//! decision made again inside the transaction. Only forge-pipelines calls
//! them (with `Authorized::reconfirm` on the locked facts); forge-db
//! defines and tests them. The same shape as forge-review's and
//! forge-hosting's `decided_callers.rs`.

use std::path::{Path, PathBuf};

/// The commands' names, assembled so this file does not match itself.
fn pipeline_commands() -> Vec<String> {
    ["cancel_pipeline", "retry_pipeline", "run_manual"]
        .iter()
        .map(|c| format!("{c}{}{}", "_decided", "("))
        .collect()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_forge_pipelines_calls_the_pipeline_commands() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let needles = pipeline_commands();
    let mut offenders = Vec::new();
    let mut callers = 0;
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let dir = entry.expect("entry").path();
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read");
            if !needles.iter().any(|n| text.contains(n.as_str())) {
                continue;
            }
            match name.as_str() {
                "forge-db" => {}
                "forge-pipelines" => callers += 1,
                _ => offenders.push(file.display().to_string()),
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "callers outside forge-pipelines: {offenders:?}"
    );
    assert!(
        callers > 0,
        "forge-pipelines calls the commands (the scan works)"
    );
}
