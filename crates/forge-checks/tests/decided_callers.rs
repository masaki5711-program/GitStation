#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-db's check commands take any `decide` closure, so a caller could
//! pass `|_| Ok(())` and skip the in-transaction decision. Only
//! forge-checks calls them (with `Authorized::reconfirm`); forge-db's own
//! tests exercise them (M3 C1, the same guard as forge-hosting's and
//! forge-review's `decided_callers.rs`). M3 X2 adds forge's own write,
//! `record_forge_check_run`, which takes no decision at all.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_forge_checks_calls_the_check_commands() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    // Assembled so this file does not match itself.
    let mut needles: Vec<String> = [
        "report_check_run",
        "change_check_namespace_grant",
        // Plan 13 Q13: the document lint's migration setting.
        "set_doc_lint_migration",
        "clear_doc_lint_migration",
    ]
    .iter()
    .map(|c| format!("{c}{}{}", "_decided", "("))
    .collect();
    // M3 X2: forge's own runs (no decision at all: forge's worker) are
    // written through forge-checks' `SpecDrift` only.
    needles.push(format!("{}{}", "record_forge_check_run", "("));
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let dir = entry.expect("entry").path();
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        if name == "forge-db" || name == "forge-checks" {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&dir, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read");
            if needles.iter().any(|n| text.contains(n.as_str())) {
                offenders.push(file.display().to_string());
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "callers outside forge-checks: {offenders:?}"
    );
}

/// The names never match the needles of forge-hosting's scan (so neither
/// scan needs an exception for the other crate).
#[test]
fn the_check_command_names_avoid_the_hosting_needles() {
    let hosting = [
        "create",
        "update",
        "change_visibility",
        "add_member",
        "change_member",
        "remove_member",
        "change",
        "delete",
        "move",
        "rename",
        "release",
        "restore",
        "put_merge_rule",
        "remove_merge_rule",
    ];
    for name in [
        "report_check_run",
        "change_check_namespace_grant",
        "set_doc_lint_migration",
        "clear_doc_lint_migration",
    ] {
        let call = format!("{name}_decided(");
        for needle in hosting {
            assert!(
                !call.contains(&format!("{needle}_decided(")),
                "{name} matches {needle}"
            );
        }
    }
}
