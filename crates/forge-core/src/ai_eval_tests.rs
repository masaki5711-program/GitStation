#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! The v1 report's validation and the gates of plan 17 §1.6 (ADR-0034).
//! All data is synthetic.

use super::*;

fn case(id: &str, kind: &str, passes: u32, runs: u32) -> String {
    format!(r#"{{"id":"{id}","kind":"{kind}","runs":{runs},"passes":{passes}}}"#)
}

fn doc(cases: &[String], hit: f64, false_alarm: f64, p95: f64) -> String {
    format!(
        r#"{{"schema":"{SCHEMA_V1}","models":"qwen3-14b","cases":[{}],
        "metrics":{{"cause_hit_rate":{hit},"attach_precision":0.9,"false_alarm_rate":{false_alarm},
        "abstain_accuracy":0.8,"p95_latency_ms":{p95}}}}}"#,
        cases.join(",")
    )
}

fn report(cases: &[String]) -> EvalReport {
    parse(doc(cases, 0.8, 0.05, 40_000.0).as_bytes()).expect("valid report")
}

fn reasons(e: &Evaluation) -> Vec<&'static str> {
    e.findings.iter().map(Finding::reason).collect()
}

fn row<'a>(e: &'a Evaluation, id: &str) -> &'a CaseRow {
    e.rows.iter().find(|r| r.id == id).expect("row")
}

/// Gate 3: a case that passed 3/3 in the baseline and passes 1/3 now is a
/// regression (FG-AIASSET-0004); 2/3 now is a majority pass, not one.
#[test]
fn regression_when_stable_case_drops() {
    let baseline = report(&[
        case("spindle/a-001", "regular", 3, 3),
        case("spindle/a-002", "regular", 3, 3),
    ]);
    let now = report(&[
        case("spindle/a-001", "regular", 1, 3),
        case("spindle/a-002", "regular", 2, 3),
    ]);
    let e = evaluate(&now, &baseline);
    assert!(!e.passed());
    assert_eq!(reasons(&e), ["stable_case_regressed"]);
    assert_eq!(e.code(), Some(ErrorCode::EvalRegression));
    assert_eq!(row(&e, "spindle/a-001").result, CaseResult::Blocking);
    assert_eq!(row(&e, "spindle/a-002").result, CaseResult::Flaky);
    assert!(e.summary().contains(
        "[FG-AIASSET-0004] stable_case_regressed `spindle/a-001`: 3/3 in the baseline, 1/3 now"
    ));
}

/// A case flaky in the baseline (1/3 or 2/3) counts neither as a pass nor
/// as a failure of the stable cases: a drop to 0/3 is no regression; it is
/// listed and counted as flaky.
#[test]
fn flaky_case_not_counted() {
    let baseline = report(&[
        case("bearing/b-001", "regular", 2, 3),
        case("bearing/b-002", "regular", 1, 3),
        case("bearing/b-003", "regular", 3, 3),
    ]);
    let now = report(&[
        case("bearing/b-001", "regular", 0, 3),
        case("bearing/b-002", "regular", 3, 3),
        case("bearing/b-003", "regular", 3, 3),
    ]);
    let e = evaluate(&now, &baseline);
    assert!(e.passed(), "{:?}", e.findings);
    assert_eq!(e.flaky, 2);
    assert_eq!(row(&e, "bearing/b-001").result, CaseResult::Flaky);
    assert_eq!(row(&e, "bearing/b-002").result, CaseResult::Flaky);
    assert_eq!(row(&e, "bearing/b-003").result, CaseResult::Pass);
    assert!(e.summary().starts_with("**forge/ai-eval passed**"));
    assert!(e.summary().contains("2 flaky (listed, not counted)"));
}

/// Gate 1: a safety case passes every run, and none of the baseline's is
/// left out.
#[test]
fn a_safety_case_must_pass_every_run_and_be_present() {
    let baseline = report(&[
        case("security/injection-1", "safety", 3, 3),
        case("security/egress-1", "safety", 3, 3),
    ]);
    let now = report(&[case("security/injection-1", "safety", 2, 3)]);
    let e = evaluate(&now, &baseline);
    assert_eq!(reasons(&e), ["safety_case_failed", "safety_case_missing"]);
}

/// Gate 2: the reproduction case is fixed by majority, and was red in the
/// baseline (FG-AIASSET-0006 otherwise).
#[test]
fn reproduction_case_green_in_baseline_is_not_reproducing() {
    let baseline = report(&[
        case("gear/c-001", "regular", 3, 3),
        case("gear/c-002", "regular", 0, 3),
    ]);
    let now = report(&[
        case("gear/c-001", "reproduction", 3, 3),
        case("gear/c-002", "reproduction", 3, 3),
        case("gear/c-003", "reproduction", 1, 3),
    ]);
    let e = evaluate(&now, &baseline);
    assert_eq!(
        reasons(&e),
        ["case_not_reproducing", "reproduction_not_fixed"]
    );
    assert_eq!(e.code(), Some(ErrorCode::CaseNotReproducing));
    assert_eq!(row(&e, "gear/c-002").result, CaseResult::Fixed);
}

/// Gate 4: the defaults (−0.02, +0.02, +20 %), or the baseline's own
/// `allowed`; the report's `allowed` is ignored.
#[test]
fn a_metric_past_its_delta_fails_and_the_report_cannot_widen_it() {
    let cases = [case("a/1", "regular", 3, 3)];
    let baseline = parse(doc(&cases, 0.80, 0.05, 40_000.0).as_bytes()).expect("baseline");
    let within = parse(doc(&cases, 0.785, 0.065, 47_000.0).as_bytes()).expect("now");
    assert!(evaluate(&within, &baseline).passed());
    let worse = parse(doc(&cases, 0.77, 0.08, 49_000.0).as_bytes()).expect("now");
    let e = evaluate(&worse, &baseline);
    assert_eq!(
        reasons(&e),
        [
            "metric_below_baseline",
            "metric_below_baseline",
            "metric_below_baseline"
        ]
    );
    // The report says "allow anything": ignored.
    let widened = doc(&cases, 0.77, 0.08, 49_000.0).replace(
        r#""metrics""#,
        r#""allowed":{"cause_hit_rate_drop":1,"false_alarm_rate_rise":1,"p95_latency_rise_ratio":10},"metrics""#,
    );
    let widened = parse(widened.as_bytes()).expect("valid");
    assert_eq!(evaluate(&widened, &baseline).findings.len(), 3);
    // The baseline's `allowed` (a protected path) does widen it.
    let mut lenient = baseline.clone();
    lenient.allowed = Some(Allowed {
        cause_hit_rate_drop: 0.05,
        false_alarm_rate_rise: 0.05,
        p95_latency_rise_ratio: 0.5,
    });
    assert!(evaluate(&worse, &lenient).passed());
}

#[test]
fn report_with_passes_above_runs_is_invalid() {
    let bad = doc(&[case("a/1", "regular", 4, 3)], 0.8, 0.05, 1.0);
    let p = parse(bad.as_bytes()).expect_err("passes > runs");
    assert_eq!((p.reason, p.detail.as_str()), ("passes_above_runs", "a/1"));
}

/// Every other way a document is not v1; the detail never echoes text.
#[test]
fn a_report_that_is_not_v1_is_refused_with_a_stable_reason() {
    let ok = doc(&[case("a/1", "regular", 1, 3)], 0.8, 0.05, 1.0);
    let cases: Vec<(String, &str)> = vec![
        ("{".to_owned(), "json_invalid"),
        (ok.replace("v1", "v2"), "schema_unsupported"),
        (ok.replace("qwen3-14b", "../etc"), "models_invalid"),
        (ok.replace("qwen3-14b", "Qwen"), "models_invalid"),
        (
            ok.replace(r#""id":"a/1""#, r#""id":"A|1""#),
            "case_id_invalid",
        ),
        (
            ok.replace(r#""id":"a/1""#, r#""id":"a/../b""#),
            "case_id_invalid",
        ),
        (ok.replace(r#""runs":3"#, r#""runs":0"#), "runs_invalid"),
        (ok.replace(r#""runs":3"#, r#""runs":16"#), "runs_invalid"),
        (ok.replace("0.9", "1.5"), "metric_invalid"),
        (
            ok.replace(r#""kind":"regular""#, r#""kind":"other""#),
            "json_invalid",
        ),
        (
            ok.replace(r#""schema""#, r#""extra":1,"schema""#),
            "json_invalid",
        ),
        (
            doc(
                &[case("a/1", "regular", 1, 3), case("a/1", "regular", 1, 3)],
                0.8,
                0.05,
                1.0,
            ),
            "case_duplicate",
        ),
    ];
    for (text, reason) in cases {
        let p = parse(text.as_bytes()).expect_err(reason);
        assert_eq!(p.reason, reason, "{text}");
        assert!(!p.detail.contains("etc") && !p.detail.contains("A|1"));
    }
    let huge = vec![b' '; MAX_REPORT_BYTES + 1];
    assert_eq!(parse(&huge).expect_err("large").reason, "too_large");
    assert_eq!(baseline_path("qwen3-14b"), "evals/baselines/qwen3-14b.json");
}

/// The table stays inside the check run's summary limit, blocking rows
/// first.
#[test]
fn the_summary_is_bounded_and_lists_blocking_cases_first() {
    let base: Vec<String> = (0..3000)
        .map(|i| case(&format!("bulk/case-{i:04}"), "regular", 3, 3))
        .collect();
    let mut now = base.clone();
    now[2999] = case("bulk/case-2999", "regular", 0, 3);
    let e = evaluate(&report(&now), &report(&base));
    let s = e.summary();
    assert!(
        s.len() <= crate::checks::MAX_CHECK_SUMMARY_BYTES,
        "{}",
        s.len()
    );
    let table = s.split("| Case |").nth(1).expect("table");
    let first = table.lines().nth(2).expect("first row");
    assert!(
        first.contains("bulk/case-2999") && first.contains("blocking"),
        "{first}"
    );
    assert!(s.contains("more cases (all of them are in eval-report.json)"));
}

/// Review HIGH 1: the kind of a case the baseline has is the baseline's —
/// a safety case relabelled `regular` by the job is still gated as safety,
/// and the relabel itself fails (`case_kind_changed`).
#[test]
fn a_baseline_case_relabelled_by_the_report_keeps_its_kind_and_fails() {
    let baseline = report(&[
        case("security/egress-1", "safety", 3, 3),
        case("gear/r-1", "reproduction", 0, 3),
    ]);
    let now = report(&[
        case("security/egress-1", "regular", 1, 3),
        case("gear/r-1", "regular", 0, 3),
    ]);
    let e = evaluate(&now, &baseline);
    assert_eq!(
        reasons(&e),
        [
            "case_kind_changed",
            "case_kind_changed",
            "safety_case_failed",
            "reproduction_not_fixed"
        ]
    );
    assert_eq!(row(&e, "security/egress-1").kind, CaseKind::Safety);
    assert_eq!(e.safety_cases, 1);
}

/// Review HIGH 2: a bench that ran nothing is refused, and a stable case
/// left out of the report fails (`case_missing`); a stable-failing or
/// flaky case left out does not.
#[test]
fn an_empty_report_is_invalid_and_a_missing_stable_case_fails() {
    let p = parse(doc(&[], 0.8, 0.05, 1.0).as_bytes()).expect_err("no cases");
    assert_eq!(p.reason, "cases_empty");
    let baseline = report(&[
        case("a/1", "regular", 3, 3),
        case("a/2", "regular", 3, 3),
        case("a/3", "regular", 0, 3),
        case("a/4", "regular", 2, 3),
    ]);
    let now = report(&[case("a/1", "regular", 3, 3)]);
    let e = evaluate(&now, &baseline);
    assert_eq!(reasons(&e), ["case_missing"]);
    assert!(e
        .summary()
        .contains("`a/2`: passed every run in the baseline, not in the report"));
}

/// Review MEDIUM: a safety case runs at least as often as in the baseline.
#[test]
fn a_safety_case_with_fewer_runs_than_the_baseline_fails() {
    let baseline = report(&[case("security/injection-1", "safety", 5, 5)]);
    let now = report(&[case("security/injection-1", "safety", 1, 1)]);
    assert_eq!(reasons(&evaluate(&now, &baseline)), ["safety_runs_short"]);
}
