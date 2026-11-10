#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

use crate::checks::{CheckConclusion, CheckRun, CheckRunsOnHead, CheckStatus};
use crate::{CheckName, ObjectId};

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn name(raw: &str) -> CheckName {
    CheckName::parse(raw).expect("name")
}

#[test]
fn statuses_and_conclusions_round_trip_their_names() {
    for s in CheckStatus::ALL {
        assert_eq!(CheckStatus::parse(s.as_str()), Some(s));
    }
    for c in CheckConclusion::ALL {
        assert_eq!(CheckConclusion::parse(c.as_str()), Some(c));
    }
    assert_eq!(CheckStatus::parse("done"), None);
    assert_eq!(CheckConclusion::parse("Success"), None);
    assert_eq!(CheckConclusion::parse(""), None);
}

#[test]
fn a_conclusion_exists_exactly_when_completed() {
    assert!(CheckRun::new(CheckStatus::Completed, None).is_none());
    assert!(CheckRun::new(CheckStatus::Queued, Some(CheckConclusion::Success)).is_none());
    assert!(CheckRun::new(CheckStatus::InProgress, None).is_some());
    assert!(CheckRun::new(CheckStatus::Completed, Some(CheckConclusion::Neutral)).is_some());
}

#[test]
fn only_completed_success_succeeds() {
    for c in CheckConclusion::ALL {
        let run = CheckRun::new(CheckStatus::Completed, Some(c)).expect("run");
        assert_eq!(run.succeeded(), c == CheckConclusion::Success, "{c:?}");
    }
    for s in [CheckStatus::Queued, CheckStatus::InProgress] {
        assert!(!CheckRun::new(s, None).expect("run").succeeded());
    }
}

#[test]
fn a_snapshot_answers_only_for_its_own_head() {
    let ok = CheckRun::new(CheckStatus::Completed, Some(CheckConclusion::Success)).expect("run");
    let runs = CheckRunsOnHead::new(sha('a'), [(name("ci/gates"), ok)]);
    assert_eq!(runs.run(&sha('a'), &name("ci/gates")), Some(ok));
    assert_eq!(runs.run(&sha('a'), &name("ci/other")), None);
    // A run on an old head never counts for another one.
    assert_eq!(runs.run(&sha('b'), &name("ci/gates")), None);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs.head(), Some(&sha('a')));
    let empty = CheckRunsOnHead::default();
    assert!(empty.is_empty());
    assert_eq!(empty.run(&sha('a'), &name("ci/gates")), None);
}

/// M3 X2: the names forge's own worker reports are valid `forge/` names,
/// and only they count as reported by forge.
#[test]
fn forge_reports_exactly_its_own_checks() {
    use crate::checks::{
        forge_reports, CI_LINT_CHECK, DOC_LINT_CHECK, FORGE_REPORTED_CHECKS, RISK_CHECK,
        SPEC_DRIFT_CHECK, SPEC_LINT_CHECK, SPEC_TRACE_CHECK,
    };
    use crate::merge_conditions::CheckNamespace;
    // M3 X7 adds forge/risk, M3 X4 forge/spec-lint and forge/spec-trace,
    // plan 13 stage 2 forge/doc-lint, plan 14 C1 forge/ci-lint.
    assert_eq!(
        FORGE_REPORTED_CHECKS,
        &[
            SPEC_DRIFT_CHECK,
            RISK_CHECK,
            SPEC_LINT_CHECK,
            SPEC_TRACE_CHECK,
            DOC_LINT_CHECK,
            CI_LINT_CHECK
        ]
    );
    assert_eq!(CI_LINT_CHECK, "forge/ci-lint");
    assert_eq!(DOC_LINT_CHECK, "forge/doc-lint");
    assert_eq!(RISK_CHECK, "forge/risk");
    assert_eq!(SPEC_LINT_CHECK, "forge/spec-lint");
    assert_eq!(SPEC_TRACE_CHECK, "forge/spec-trace");
    assert_eq!(
        crate::checks::MAX_STORED_CHECK_RUNS_PER_COMMIT,
        crate::checks::MAX_CHECK_RUNS_PER_COMMIT + 6
    );
    for raw in FORGE_REPORTED_CHECKS {
        let n = name(raw);
        assert_eq!(n.namespace(), CheckNamespace::Forge, "{raw}");
        assert!(forge_reports(&n), "{raw}");
    }
    for raw in [
        "forge/spec-conformance",
        "ci/spec-drift",
        "ci/spec-trace",
        "ci/risk",
        "ext/spec-drift",
    ] {
        assert!(!forge_reports(&name(raw)), "{raw}");
    }
}
