#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The `forge/ci-lint` run (plan 14 C1): pure, on synthetic `.forge-ci.yml`
//! bytes and hand-built protection rules. The read from a real repository
//! and the database is `tests/ci_lint.rs`.

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion};
use forge_core::{MergeRule, ProtectionRule, ProtectionRules, RefPattern, Role};

use super::{assess_ci_lint, branch_patterns, CiFile, CiLintInput, CI_FILE};

const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A valid definition; `deploy` has a secret and runs on `main` only.
pub(crate) fn valid() -> String {
    format!(
        "version: 1
stages: [build, deploy]
jobs:
  build:
    stage: build
    image: registry.internal/rust@sha256:{D}
    timeout: 30m
    script: [cargo build]
  deploy:
    stage: deploy
    image: registry.internal/rust@sha256:{D}
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]
    rules:
      branches: [main]
      merge_requests: false
"
    )
}

fn input(file: CiFile, protected: &[&str]) -> CiLintInput {
    CiLintInput {
        file,
        protected: protected.iter().map(|p| (*p).to_owned()).collect(),
    }
}

fn found(text: &str, protected: &[&str]) -> CiLintInput {
    input(CiFile::Found(text.as_bytes().to_vec()), protected)
}

#[test]
fn a_clean_definition_succeeds() {
    let lint = assess_ci_lint(&found(&valid(), &["main"])).expect("run");
    assert_eq!(lint.run.name.as_str(), "forge/ci-lint");
    assert_eq!(
        lint.run.conclusion,
        Some(CheckConclusion::Success),
        "{}",
        lint.run.summary
    );
    assert!(lint.invalid.is_none());
    assert!(lint.run.annotations.is_empty());
    assert!(lint.run.summary.contains(CI_FILE), "{}", lint.run.summary);
}

#[test]
fn a_cycle_fails_with_its_reason_and_line() {
    let text = valid().replace(
        "  build:\n    stage: build\n",
        "  build:\n    stage: build\n    needs: [build]\n",
    );
    let lint = assess_ci_lint(&found(&text, &["main"])).expect("run");
    assert_eq!(
        lint.run.conclusion,
        Some(CheckConclusion::Failure),
        "{}",
        lint.run.summary
    );
    // `build:` is on line 4 of the file.
    assert!(
        lint.run
            .summary
            .contains(".forge-ci.yml:4 needs_cycle — build [FG-CI-0001]"),
        "{}",
        lint.run.summary
    );
    assert!(
        lint.run.summary.contains("1 problem(s)"),
        "{}",
        lint.run.summary
    );
    let a = lint.run.annotations.first().expect("an annotation");
    assert_eq!((a.path.as_str(), a.start_line), (CI_FILE, 4));
    assert_eq!(a.level, CheckAnnotationLevel::Failure);
    let err = lint.invalid.expect("FG-CI-0001 for the log");
    assert_eq!(err.code().id(), "FG-CI-0001");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "needs_cycle"));
}

#[test]
fn a_secret_on_an_unprotected_branch_fails_and_names_the_branch_only() {
    let lint = assess_ci_lint(&found(&valid(), &[])).expect("run");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        lint.run
            .summary
            .contains("secret_on_unprotected_ref — main [FG-CI-0001]"),
        "{}",
        lint.run.summary
    );
    // The secret's name is never the subject of this reason.
    assert!(
        !lint.run.summary.contains("DEPLOY_KEY"),
        "{}",
        lint.run.summary
    );
    assert!(lint
        .run
        .annotations
        .iter()
        .all(|a| !a.message.contains("DEPLOY_KEY")));
}

#[test]
fn a_malformed_secret_name_is_never_echoed() {
    let text = valid().replace("[DEPLOY_KEY]", "[hunter2-paste]");
    let lint = assess_ci_lint(&found(&text, &["main"])).expect("run");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        lint.run.summary.contains("secret_name_invalid"),
        "{}",
        lint.run.summary
    );
    assert!(
        !lint.run.summary.contains("hunter2"),
        "{}",
        lint.run.summary
    );
}

#[test]
fn no_definition_is_nothing_to_check() {
    let lint = assess_ci_lint(&input(CiFile::Absent, &[])).expect("run");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Success));
    assert!(
        lint.run.summary.contains("nothing to check"),
        "{}",
        lint.run.summary
    );
    assert!(lint.invalid.is_none());
}

#[test]
fn a_file_too_large_or_not_a_file_fails() {
    for (file, reason) in [
        (CiFile::TooLarge, "too_large"),
        (CiFile::NotAFile, "not_a_file"),
    ] {
        let lint = assess_ci_lint(&input(file, &["main"])).expect("run");
        assert_eq!(lint.run.conclusion, Some(CheckConclusion::Failure));
        assert!(
            lint.run
                .summary
                .contains(&format!(".forge-ci.yml:0 {reason} [FG-CI-0001]")),
            "{}",
            lint.run.summary
        );
        let err = lint.invalid.expect("invalid");
        assert!(err
            .context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == reason));
    }
}

#[test]
fn many_problems_are_cut_at_forge_cis_cap() {
    // 120 unknown keys at the top level: forge-ci keeps 100 and adds one
    // `too_many_problems`; the annotations stop at the table's cap.
    let mut text = valid();
    for i in 0..60 {
        text.push_str(&format!("unknown{i}: 1\n"));
    }
    let lint = assess_ci_lint(&found(&text, &["main"])).expect("run");
    assert_eq!(lint.run.conclusion, Some(CheckConclusion::Failure));
    assert!(
        lint.run.annotations.len() <= forge_core::checks::MAX_CHECK_ANNOTATIONS,
        "{}",
        lint.run.annotations.len()
    );
    assert!(
        lint.run.summary.len() <= forge_core::checks::MAX_CHECK_SUMMARY_BYTES,
        "{}",
        lint.run.summary.len()
    );
}

fn rule(pattern: &str) -> ProtectionRule {
    ProtectionRule {
        pattern: RefPattern::parse(pattern).expect("pattern"),
        push_min_role: Role::Maintainer,
        merge: MergeRule::default(),
    }
}

#[test]
fn branch_patterns_are_the_protected_heads_forge_ci_can_compare() {
    let rules = ProtectionRules::new(vec![
        rule("refs/heads/main"),
        rule("refs/heads/release/*"),
        rule("refs/heads/Hotfix"),
        rule("refs/tags/v*"),
        // A `*` inside the last component: forge-ci compares only a
        // trailing `*`, so it is left out (conservative: its branches are
        // treated as unprotected).
        rule("refs/heads/v*-lts"),
    ]);
    assert_eq!(
        branch_patterns(&rules),
        vec![
            "main".to_owned(),
            "release/*".to_owned(),
            "Hotfix".to_owned(),
            "hotfix".to_owned(),
        ]
    );
}
