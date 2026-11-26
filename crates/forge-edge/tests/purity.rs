#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-edge stays pure (plan 21 E1, ADR-0041, CLAUDE.md contract 9): it
//! reads bytes and values and returns values, so it may depend only on
//! forge-errors, the pure forge-agent and computation crates — never on a
//! database, an async runtime, an HTTP client or the file system's crates —
//! and its source never reaches for the file system, the network, the
//! process environment or the clock.

use std::path::Path;

/// Every dependency forge-edge may have (normal, build and target).
const ALLOWED: &[&str] = &[
    "forge-errors",
    "forge-agent",
    "serde",
    "serde_json",
    "toml",
    "sha2",
    "jiff",
];

/// Calls that would make the crate impure. `Timestamp::now` reads the
/// clock; `TimeZone::system` / `try_system` read the environment and files.
const FORBIDDEN: &[&str] = &[
    "std::fs",
    "std::net",
    "std::env",
    "std::process",
    "std::time::SystemTime",
    "Instant::now",
    "Timestamp::now",
    "Zoned::now",
    "TimeZone::system",
    "TimeZone::try_system",
    "tokio",
    "reqwest",
    "sqlx",
];

fn names(table: Option<&toml::Value>) -> Vec<String> {
    table
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .map(|(name, spec)| {
                    spec.get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(name)
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn purity() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).expect("Cargo.toml");
    let doc: toml::Value = text.parse().expect("manifest parses");
    let mut deps = names(doc.get("dependencies"));
    deps.extend(names(doc.get("build-dependencies")));
    if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            deps.extend(names(target.get("dependencies")));
        }
    }
    let offenders: Vec<&String> = deps
        .iter()
        .filter(|d| !ALLOWED.contains(&d.as_str()))
        .collect();
    assert!(
        offenders.is_empty(),
        "forge-edge must stay pure: {offenders:?}"
    );

    // jiff without its system time zone or zoneinfo features (I/O); since
    // E1b it only parses the manifest's instant.
    let jiff = doc
        .get("dependencies")
        .and_then(|d| d.get("jiff"))
        .expect("jiff dependency");
    assert_eq!(
        jiff.get("workspace").and_then(toml::Value::as_bool),
        Some(true)
    );
    let ws = std::fs::read_to_string(dir.join("../../Cargo.toml")).expect("workspace Cargo.toml");
    let line = ws
        .lines()
        .find(|l| l.starts_with("jiff ="))
        .expect("workspace jiff");
    assert!(line.contains("default-features = false"), "{line}");
    for io_feature in ["tz-system", "tzdb-zoneinfo", "tzdb-bundle-platform"] {
        assert!(!line.contains(io_feature), "{line}");
    }
    // Plan 21 E1b: no tz database either (only the instant is parsed).
    for tzdb in ["tzdb-bundle-always", "tz-fat", "static"] {
        assert!(!line.contains(&format!("\"{tzdb}\"")), "{line}");
    }

    // The source never reaches for I/O or the clock (tests.rs excluded:
    // it is test code).
    for entry in std::fs::read_dir(dir.join("src")).expect("src") {
        let path = entry.expect("entry").path();
        if path.file_name().and_then(|n| n.to_str()) == Some("tests.rs") {
            continue;
        }
        let src = std::fs::read_to_string(&path).expect("source");
        for f in FORBIDDEN {
            assert!(
                !src.contains(f),
                "{} uses {f}: forge-edge must stay pure",
                path.display()
            );
        }
    }
}
