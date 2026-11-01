#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-db's `*_decided` commands take any `decide` closure, so a caller
//! could pass `|_| Ok(())` and skip the in-transaction decision. Only
//! forge-hosting calls them (with `Authorized::reconfirm`); forge-db's own
//! tests exercise them. Any other caller in the workspace fails here (M1
//! phase 8b MR7a review, MEDIUM-3). forge-db cannot require a forge-auth
//! type (it sits below forge-auth), so this is the guard.

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
fn only_forge_hosting_calls_the_decided_commands() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    // The commands' names, assembled so this file does not match itself.
    let needles: Vec<String> = [
        "create",
        "update",
        "change_visibility",
        // M1 phase 8b MR8: members and protected refs
        "add_member",
        "change_member",
        "remove_member",
        "change",
        "delete",
        // D5 MR2: moves, renames and tombstone releases
        "move",
        "rename",
        "release",
    ]
    .iter()
    .map(|c| format!("{c}{}{}", "_decided", "("))
    .collect();
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let dir = entry.expect("entry").path();
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_owned();
        if name == "forge-db" || name == "forge-hosting" {
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
        "callers outside forge-hosting: {offenders:?}"
    );
}
