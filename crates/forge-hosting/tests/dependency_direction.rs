#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The dependency direction gate (CLAUDE.md §3-9, ADR-0001 and its M1 phase
//! 8b amendment): `forge-web / forge-cli → feature crates (forge-hosting) →
//! forge-auth / forge-git / forge-db → forge-core → forge-errors`. Every
//! workspace crate's normal and build dependencies on other forge crates
//! are checked against the layer table; a crate missing from the table
//! fails too, so a new crate must be placed before it builds green.

use std::collections::BTreeMap;
use std::path::Path;

/// Each forge crate and the forge crates it may depend on.
const ALLOWED: &[(&str, &[&str])] = &[
    ("forge-errors", &[]),
    ("forge-core", &["forge-errors"]),
    ("forge-auth", &["forge-core", "forge-errors"]),
    ("forge-git", &["forge-core", "forge-errors"]),
    ("forge-db", &["forge-core", "forge-errors"]),
    // Feature crates: the layer below only, never each other.
    (
        "forge-hosting",
        &[
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-errors",
        ],
    ),
    (
        "forge-web",
        &[
            "forge-hosting",
            "forge-auth",
            "forge-git",
            "forge-core",
            "forge-errors",
        ],
    ),
    (
        "forge-cli",
        &[
            "forge-web",
            "forge-hosting",
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-errors",
        ],
    ),
];

/// The forge crates `table` names (a `[dependencies]`-like table).
fn forge_deps(table: Option<&toml::Value>) -> Vec<String> {
    table
        .and_then(toml::Value::as_table)
        .map(|t| {
            t.iter()
                .map(|(name, spec)| {
                    // A renamed dependency (`x = { package = "forge-db" }`).
                    spec.get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(name)
                        .to_owned()
                })
                .filter(|n| n.starts_with("forge-"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn every_crate_depends_only_downwards() {
    let allowed: BTreeMap<&str, &[&str]> = ALLOWED.iter().copied().collect();
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut seen = Vec::new();
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let manifest = entry.expect("entry").path().join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let doc: toml::Value = text.parse().expect("manifest parses");
        let name = doc["package"]["name"]
            .as_str()
            .expect("package name")
            .to_owned();
        let Some(ok) = allowed.get(name.as_str()) else {
            offenders.push(format!("{name}: not in the layer table"));
            continue;
        };
        let mut deps = forge_deps(doc.get("dependencies"));
        deps.extend(forge_deps(doc.get("build-dependencies")));
        if let Some(targets) = doc.get("target").and_then(toml::Value::as_table) {
            for target in targets.values() {
                deps.extend(forge_deps(target.get("dependencies")));
            }
        }
        for dep in deps {
            if !ok.contains(&dep.as_str()) {
                offenders.push(format!("{name} -> {dep}"));
            }
        }
        seen.push(name);
    }
    assert!(
        offenders.is_empty(),
        "dependency direction broken: {offenders:?}"
    );
    for (name, _) in ALLOWED {
        assert!(seen.iter().any(|s| s == name), "{name} not found");
    }
}
