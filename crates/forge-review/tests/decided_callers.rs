#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-db's review commands take any `decide` closure, so a caller could
//! pass `|_| Ok(())` and skip the in-transaction decision. Only forge-review
//! calls them (with `Authorized::reconfirm` on `Authorized<MergeRequestCtx>`);
//! forge-db's own tests exercise them (ADR-0024 §11, design review #9, #33).
//! forge-hosting's `decided_callers.rs` stays as it is, without an
//! exception for this crate: the review commands are named so its needles
//! never match them (checked below).

use std::path::{Path, PathBuf};

/// The review commands' names, assembled so this file does not match itself.
fn review_commands() -> Vec<String> {
    ["open_mr", "edit_mr", "close_mr", "reopen_mr"]
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
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_forge_review_calls_the_review_commands() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let needles = review_commands();
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
                "forge-review" => callers += 1,
                _ => offenders.push(file.display().to_string()),
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "callers outside forge-review: {offenders:?}"
    );
    assert!(
        callers > 0,
        "forge-review calls the commands (the scan works)"
    );
}

/// The needles of forge-hosting's scan (crates/forge-hosting/tests/
/// decided_callers.rs): a review command whose name contained one would
/// fail that test or need an exception there (ADR-0024 §11, #33).
#[test]
fn the_review_commands_never_match_the_hosting_needles() {
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
    ];
    for command in review_commands() {
        for needle in hosting {
            let needle = format!("{needle}{}{}", "_decided", "(");
            assert!(
                !command.contains(&needle),
                "{command} contains the hosting needle {needle}"
            );
        }
    }
}
