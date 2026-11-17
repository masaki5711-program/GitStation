#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! GitStation's own repository must be pushable to a GitStation instance:
//! every tracked file passes the push secret scan (FG-GIT-0022) — a
//! synthetic fixture that trips a rule carries `forge:allow-secret`.
//!
//! Runs the real `secret_scan::scan` (the push check's entry point, which
//! skips binary blobs the same way) over every file `git ls-files` lists.

use std::path::PathBuf;

use forge_git::secret_scan::{self, SecretScan};

#[test]
fn every_tracked_file_passes_the_push_secret_scan() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z", "--"])
        .output()
        .unwrap_or_else(|e| panic!("cannot run git ls-files: {e}"));
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut scanned = 0usize;
    let mut hits = Vec::new();
    for raw in out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let rel = std::str::from_utf8(raw).expect("tracked path is UTF-8");
        let path = root.join(rel);
        let content = match std::fs::read(&path) {
            Ok(c) => c,
            // Deleted in the working tree (not yet committed): nothing to push.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot read {rel}: {e}"),
        };
        scanned += 1;
        if let SecretScan::Found(rule) = secret_scan::scan(&content) {
            hits.push(format!("{rel} ({rule})"));
        }
    }

    assert!(scanned > 100, "ls-files listed only {scanned} files");
    assert!(
        hits.is_empty(),
        "tracked files trip the push secret scan without `forge:allow-secret` \
         (a push of this repository is refused with FG-GIT-0022):\n  {}",
        hits.join("\n  ")
    );
}
