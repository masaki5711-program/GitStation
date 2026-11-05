#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The feed's decision cannot be skipped (M3 I1c, ADR-0016 §7 "VisibleSet
//! を作れるのは forge-auth だけ … feed::read を呼んでよいのは履歴の口だけ"):
//!
//! - `VisibleSet::from_authorized_decisions` (forge-core) is called only by
//!   forge-auth (`prove_visible`); forge-core defines and tests it.
//! - forge-db's page read (`FeedSnapshot::read_feed_page`) is called only
//!   by forge-integ; forge-db defines and tests it.
//! - forge-integ's `Feed::read_events` / `Feed::head_cursor` are called
//!   only by forge-web's feed routes (`src/routes/integration_feed.rs`).
//!
//! Rust cannot hide a constructor across crates, so these scans are the
//! guard (the same shape as forge-hosting's and forge-checks'
//! `decided_callers.rs`; an agent-model gate may mirror them).

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

/// Every file under `crates/` that mentions one of `needles`, as
/// (crate directory name, path relative to the crate).
fn mentions(needles: &[String]) -> Vec<(String, String)> {
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
            // Code only: a doc comment naming a function calls nothing.
            let code: String = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join(
                    "
",
                );
            if needles.iter().any(|n| code.contains(n.as_str())) {
                let rel = file
                    .strip_prefix(&dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                found.push((name.clone(), rel));
            }
        }
    }
    found
}

/// Assembled so this file does not match itself.
fn needle(parts: &[&str]) -> String {
    parts.concat()
}

#[test]
fn only_forge_auth_builds_a_visible_set() {
    let found = mentions(&[needle(&["from_authorized", "_decisions("])]);
    let offenders: Vec<_> = found
        .iter()
        .filter(|(krate, _)| krate != "forge-auth" && krate != "forge-core")
        .collect();
    assert!(
        offenders.is_empty(),
        "VisibleSet built outside forge-auth: {offenders:?}"
    );
    assert!(
        found
            .iter()
            .any(|(k, f)| k == "forge-auth" && f == "src/feed_ctx.rs"),
        "prove_visible builds it: {found:?}"
    );
}

#[test]
fn only_forge_integ_reads_feed_pages() {
    // M3 I1d: the development status's reads of the same snapshot too.
    let found = mentions(&[
        needle(&[".read_feed", "_page("]),
        needle(&[".dev_status", "_of("]),
        needle(&[".work_items", "_after("]),
        needle(&[".bound_work", "_items("]),
    ]);
    let offenders: Vec<_> = found
        .iter()
        .filter(|(krate, _)| krate != "forge-integ" && krate != "forge-db")
        .collect();
    assert!(
        offenders.is_empty(),
        "feed pages read outside forge-integ: {offenders:?}"
    );
    assert!(found.iter().any(|(k, _)| k == "forge-integ"));
}

#[test]
fn only_the_feed_routes_read_the_feed() {
    let found = mentions(&[
        needle(&[".read", "_events("]),
        needle(&[".head", "_cursor("]),
    ]);
    let offenders: Vec<_> = found
        .iter()
        .filter(|(krate, file)| {
            !(krate == "forge-integ"
                || (krate == "forge-web" && file == "src/routes/integration_feed.rs"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "the feed read outside its routes: {offenders:?}"
    );
    assert!(
        found
            .iter()
            .any(|(k, f)| k == "forge-web" && f == "src/routes/integration_feed.rs"),
        "the feed routes call it: {found:?}"
    );
}

/// Review I1c: `forge_core::FeedMaterial` cannot be sealed across crates,
/// so the decision on it is: only forge-integ (with material read from the
/// forge-db snapshot) calls `prove_visible`; forge-auth defines and tests
/// it, forge-db's feed tests decide with it (a dev-dependency).
#[test]
fn only_forge_integ_proves_what_the_feed_sees() {
    // The bare name (re-review I1c): an import or a path call counts too.
    let found = mentions(&[needle(&["prove", "_visible"])]);
    let offenders: Vec<_> = found
        .iter()
        .filter(|(krate, file)| {
            !(krate == "forge-integ"
                || krate == "forge-auth"
                || (krate == "forge-db" && file == "src/feed/tests.rs"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "prove_visible called outside forge-integ: {offenders:?}"
    );
    assert!(found
        .iter()
        .any(|(k, f)| k == "forge-integ" && f == "src/lib.rs"));
}

/// M3 I1d: the development status reads the feed's snapshot with its
/// decision; only forge-web's development-status routes call it.
#[test]
fn only_the_dev_status_routes_read_the_dev_status() {
    let found = mentions(&[
        needle(&[".work_item_dev", "_status("]),
        needle(&[".dev_status", "_page("]),
    ]);
    let offenders: Vec<_> = found
        .iter()
        .filter(|(krate, file)| {
            !(krate == "forge-integ"
                || (krate == "forge-web" && file == "src/routes/integration_dev_status.rs"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "the development status read outside its routes: {offenders:?}"
    );
    assert!(
        found
            .iter()
            .any(|(k, f)| k == "forge-web" && f == "src/routes/integration_dev_status.rs"),
        "the dev-status routes call it: {found:?}"
    );
}
