#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use forge_core::checks::{
    CheckAnnotationLevel, CheckConclusion, CheckStatus, MAX_CHECK_ANNOTATIONS,
    MAX_CHECK_SUMMARY_BYTES,
};
use forge_core::risk::{Finding, RiskLevel};

use super::*;

fn target(name: &str, findings: Vec<Finding>) -> TargetFindings {
    TargetFindings {
        target: name.to_owned(),
        files: findings.len(),
        findings,
    }
}

fn stop(path: &str) -> Finding {
    Finding::for_rule("sql_drop")
        .expect("rule")
        .at(path)
        .on_line(3)
}

fn warn(path: &str) -> Finding {
    Finding::for_rule("dependency_change")
        .expect("rule")
        .at(path)
}

fn evaluation(targets: Vec<TargetFindings>) -> Evaluation {
    Evaluation {
        targets,
        general: Vec::new(),
    }
}

#[test]
fn no_finding_is_a_success_that_says_what_was_looked_at() {
    let run = assess(&evaluation(vec![target("main", Vec::new())])).expect("run");
    assert_eq!(run.name.as_str(), "forge/risk");
    assert_eq!(run.status, CheckStatus::Completed);
    assert_eq!(run.conclusion, Some(CheckConclusion::Success));
    assert!(run.summary.contains("No risk"), "{}", run.summary);
    assert!(run.summary.contains("main"), "{}", run.summary);
    assert!(run.annotations.is_empty());
}

#[test]
fn the_worst_finding_decides_the_conclusion_and_names_its_code() {
    let warn_only =
        assess(&evaluation(vec![target("main", vec![warn("Cargo.toml")])])).expect("run");
    assert_eq!(warn_only.conclusion, Some(CheckConclusion::Neutral));
    assert!(warn_only.summary.contains("FG-REVIEW-0024"));
    assert!(!warn_only.summary.contains("FG-REVIEW-0023"));

    let both = assess(&evaluation(vec![target(
        "main",
        vec![warn("Cargo.toml"), stop("db/migrations/2.sql")],
    )]))
    .expect("run");
    assert_eq!(both.conclusion, Some(CheckConclusion::Failure));
    assert!(both.summary.contains("FG-REVIEW-0023"), "{}", both.summary);
    // Worst first: the stop is the first annotation.
    assert_eq!(both.annotations[0].level, CheckAnnotationLevel::Failure);
    assert_eq!(both.annotations[0].start_line, 3);
    assert_eq!(both.annotations[1].level, CheckAnnotationLevel::Warning);
    assert_eq!(both.annotations[1].start_line, 1, "no line: the first");
}

#[test]
fn a_not_assessed_finding_names_its_own_code() {
    let general = Finding::for_rule("no_open_merge_request").expect("rule");
    let run = assess(&Evaluation {
        targets: Vec::new(),
        general: vec![general],
    })
    .expect("run");
    assert_eq!(run.conclusion, Some(CheckConclusion::Neutral));
    assert!(run.summary.contains("FG-REVIEW-0025"), "{}", run.summary);
    assert!(run.annotations.is_empty(), "no path to annotate");
}

#[test]
fn annotations_are_capped_and_the_summary_counts_what_is_left_out() {
    let many: Vec<Finding> = (0..MAX_CHECK_ANNOTATIONS + 10)
        .map(|i| warn(&format!("pkg{i}/Cargo.toml")))
        .collect();
    let run = assess(&evaluation(vec![target("main", many)])).expect("run");
    assert_eq!(run.annotations.len(), MAX_CHECK_ANNOTATIONS);
    assert!(run.summary.contains("60"), "{}", run.summary);
}

#[test]
fn a_path_an_annotation_cannot_hold_stays_in_the_summary_only() {
    for path in ["a\\b.env", "tab\there", "../x"] {
        let run = assess(&evaluation(vec![target("main", vec![stop(path)])])).expect("run");
        assert!(run.annotations.is_empty(), "{path:?}");
        assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
        assert!(
            !run.summary.chars().any(|c| c.is_control() && c != '\n'),
            "{path:?}: {:?}",
            run.summary
        );
    }
}

#[test]
fn the_summary_is_cut_to_its_limit_not_refused() {
    let long = "d/".repeat(1500);
    let many: Vec<Finding> = (0..4000).map(|i| stop(&format!("{long}{i}.sql"))).collect();
    let run = assess(&evaluation(vec![target("main", many)])).expect("run");
    assert!(run.summary.len() <= MAX_CHECK_SUMMARY_BYTES);
    assert!(run.summary.ends_with('…'));
    assert_eq!(run.conclusion, Some(CheckConclusion::Failure));
}

#[test]
fn levels_map_to_conclusions_one_to_one() {
    assert_eq!(conclusion_of(RiskLevel::Ok), CheckConclusion::Success);
    assert_eq!(conclusion_of(RiskLevel::Warn), CheckConclusion::Neutral);
    assert_eq!(conclusion_of(RiskLevel::Stop), CheckConclusion::Failure);
}
