#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! forge-agent stays pure (plan 16 A1, ADR-0032, CLAUDE.md contract 9): it
//! reads bytes and returns values, so it may depend only on forge-errors and
//! computation crates — never on a database, an async runtime, an HTTP
//! client or the file system's crates.

/// Every dependency forge-agent may have (normal, build and target).
const ALLOWED: &[&str] = &[
    "forge-errors",
    "serde",
    "serde_json",
    "toml",
    "sha2",
    "url",
    "base64",
    "ring",
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
fn forge_agent_depends_only_on_pure_parts() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("Cargo.toml");
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
        "forge-agent must stay pure: {offenders:?}"
    );
}
