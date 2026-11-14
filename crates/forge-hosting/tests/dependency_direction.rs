#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The dependency direction gate (CLAUDE.md §3-9, ADR-0001 and its M1 phase
//! 8b amendment): `forge-web / forge-cli → feature crates (forge-hosting,
//! forge-review, forge-checks, forge-integ, forge-notify, forge-oidc) →
//! forge-auth / forge-git / forge-db → forge-core → forge-ci → forge-doc →
//! forge-errors` (forge-doc: plan 13 stage 2, ADR-0026 §8; forge-ci: M4 C1,
//! the pure `.forge-ci.yml` parse / lint / plan). Every
//! workspace crate's normal and build dependencies on other forge crates
//! are checked against the layer table; a crate missing from the table
//! fails too, so a new crate must be placed before it builds green.

use std::collections::BTreeMap;
use std::path::Path;

/// Each forge crate and the forge crates it may depend on.
const ALLOWED: &[(&str, &[&str])] = &[
    ("forge-errors", &[]),
    // Plan 13 stage 2 (ADR-0026 §8): pure document parsing, shared with gstn.
    ("forge-doc", &["forge-errors"]),
    // M4 C1 (contract 9): the pure `.forge-ci.yml` parse / lint / plan,
    // below forge-core. Like forge-doc it does no I/O and no DB; it reads
    // YAML with forge-doc's strict tree.
    ("forge-ci", &["forge-doc", "forge-errors"]),
    ("forge-core", &["forge-ci", "forge-doc", "forge-errors"]),
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
    // M3 R1a (ADR-0024 §11): merge requests, beside forge-hosting (and
    // never depending on it, nor on forge-checks).
    (
        "forge-review",
        &[
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-errors",
        ],
    ),
    // M3 C1 (ADR-0024 §11): check runs, beside forge-review (and never
    // depending on it: the merge conditions read the runs through
    // forge-core's `CheckResultsSource`). Plan 13 stage 2: forge-doc for
    // `forge/doc-lint` (the same lint as `gstn doc lint`).
    (
        "forge-checks",
        &[
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-ci",
            "forge-doc",
            "forge-errors",
        ],
    ),
    // M3 I1c (plan 11 §3.7, ADR-0016 §7): integrations — the event feed.
    // No forge-git: it reads only the outbox (domain events), never a
    // repository; never another feature crate.
    (
        "forge-integ",
        &["forge-auth", "forge-db", "forge-core", "forge-errors"],
    ),
    // M3 N1 (ADR-0016 Amendment N1): e-mail notifications. Like forge-integ it
    // reads only domain events (the outbox); never another feature crate.
    (
        "forge-notify",
        &["forge-auth", "forge-db", "forge-core", "forge-errors"],
    ),
    // M4 C2 (plan 14, contract 9): CI pipelines — planned from
    // `.forge-ci.yml` (the pure forge-ci) and written through forge-db;
    // never another feature crate (the confidential mark is measured by the
    // caller through `forge_pipelines::MarkMeter`).
    (
        "forge-pipelines",
        &[
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-ci",
            "forge-errors",
        ],
    ),
    // M4 C11 (plan 14, ADR-0031, contract 9): deployments over SSH —
    // written through forge-db, decided in forge-auth; forge-git reads
    // what may be deployed (a protected branch holds it); never another
    // feature crate.
    (
        "forge-deploy",
        &[
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-errors",
        ],
    ),
    // M2 MR4 (ADR-0021 §2): the OIDC sign-in, beside forge-hosting.
    (
        "forge-oidc",
        &["forge-auth", "forge-db", "forge-core", "forge-errors"],
    ),
    (
        "forge-web",
        &[
            "forge-oidc",
            "forge-hosting",
            "forge-review",
            "forge-checks",
            "forge-integ",
            "forge-notify",
            "forge-pipelines",
            "forge-deploy",
            "forge-auth",
            "forge-git",
            "forge-core",
            // Plan 13 stage 3: the document API maps forge-doc's view.
            "forge-doc",
            "forge-errors",
        ],
    ),
    (
        "forge-cli",
        &[
            "forge-web",
            "forge-oidc",
            "forge-hosting",
            "forge-review",
            "forge-checks",
            "forge-integ",
            "forge-notify",
            "forge-pipelines",
            "forge-deploy",
            "forge-auth",
            "forge-git",
            "forge-db",
            "forge-core",
            "forge-errors",
        ],
    ),
    // The `gstn` client sits outside the server graph (ADR-0020); it may
    // share the pure document parsing (ADR-0020 Amendment, ADR-0026 §8) and
    // the pure `.forge-ci.yml` lint and plan (ADR-0020 Amendment, M4 C1).
    ("forge-client", &["forge-errors", "forge-doc", "forge-ci"]),
    // The CI runner binary (M4 C4, contract 9, ADR-0029): the client side
    // only, never a server crate; no row lists it, so nothing depends on it.
    (
        "forge-runner",
        &["forge-core", "forge-errors", "forge-client"],
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

/// ADR-0024 §11: forge-review decides merges with the check runs and
/// forge-checks records them; neither may name the other (the runs travel
/// through forge-core's `CheckResultsSource`).
#[test]
fn forge_review_and_forge_checks_have_no_edge() {
    let allowed: BTreeMap<&str, &[&str]> = ALLOWED.iter().copied().collect();
    for (a, b) in [
        ("forge-review", "forge-checks"),
        ("forge-checks", "forge-review"),
    ] {
        let deps = allowed.get(a).expect("crate in the table");
        assert!(!deps.contains(&b), "{a} may not depend on {b}");
    }
}

/// M3 I1c (plan 11 §3.7): forge-integ reads domain events only — no edge to
/// another feature crate in either direction, and none to forge-git.
#[test]
fn forge_integ_has_no_edge_to_other_feature_crates() {
    let allowed: BTreeMap<&str, &[&str]> = ALLOWED.iter().copied().collect();
    let features = [
        "forge-hosting",
        "forge-review",
        "forge-checks",
        "forge-oidc",
    ];
    let integ = allowed
        .get("forge-integ")
        .expect("forge-integ in the table");
    for other in features.iter().chain(&["forge-git"]) {
        assert!(
            !integ.contains(other),
            "forge-integ may not depend on {other}"
        );
    }
    for other in features {
        let deps = allowed.get(other).expect("feature crate in the table");
        assert!(
            !deps.contains(&"forge-integ"),
            "{other} may not depend on forge-integ"
        );
    }
}
