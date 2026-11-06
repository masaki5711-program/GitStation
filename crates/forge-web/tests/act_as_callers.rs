#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Acting for a person is built in one place (M3 S2, ADR-0025 §7.3): only
//! forge-web's authentication (`src/act_as.rs`) builds an act-as principal
//! — forge-auth defines it (and its trial for the viewer flags and its
//! tests use it) — so a worker, the auto-merge, a bot or a queued job can
//! never act for a person. Rust cannot hide a constructor across crates,
//! so this scan is the guard (the same shape as forge-integ's
//! `decided_callers.rs`).

use std::path::{Path, PathBuf};

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

/// Every file under `crates/` whose code (not its comments) contains
/// `needle`, as `crate/relative/path`.
fn mentions(needle: &str) -> Vec<String> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
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
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            if code.contains(needle) {
                let rel = file
                    .strip_prefix(&dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                found.push(format!("{name}/{rel}"));
            }
        }
    }
    found.sort();
    found
}

/// Assembled so this file does not match itself.
fn needle(parts: &[&str]) -> String {
    parts.concat()
}

#[test]
fn only_forge_web_builds_act_as_principals() {
    let found = mentions(&needle(&["Principal::", "act_as("]));
    let allowed = [
        // The constructor's own crate: its trial for the viewer flags
        // and its tests.
        "forge-auth/src/viewer.rs",
        "forge-auth/src/act_as_tests.rs",
        "forge-auth/src/matrix_rows_act_as.rs",
        // The one production caller: the authentication layer.
        "forge-web/src/act_as.rs",
    ];
    let offenders: Vec<_> = found
        .iter()
        .filter(|f| !allowed.contains(&f.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "only forge-web's authentication builds an act-as principal: {offenders:?}"
    );
    assert!(found.iter().any(|f| f == "forge-web/src/act_as.rs"));
}

#[test]
fn only_forge_auth_builds_the_act_as_credential() {
    let found = mentions(&needle(&["Credential::", "ActAs(Box"]));
    assert_eq!(
        found,
        vec!["forge-auth/src/model.rs".to_owned()],
        "{found:?}"
    );
}
