//! The AI evaluation report and its comparison with the baseline (plan 17
//! P17-2, §1.6; ADR-0034). Pure: no I/O.
//!
//! A CI job of a repository that holds AI assets runs the evaluation bench
//! and leaves the artifact `eval-report.json` (the fixed, versioned schema
//! [`SCHEMA_V1`], documented in `docs/ai-eval-report.md`). The job can
//! write no check (ADR-0029 decision 8): forge's worker reads the report,
//! reads the baseline `evals/baselines/<models>.json` (the same schema, a
//! nightly result a person approved) at the pipeline's commit, and writes
//! `forge/ai-eval` from [`evaluate`].
//!
//! **Majority per case.** A case ran `runs` times and passed `passes`
//! times. It passes when more than half its runs passed; it is *stable*
//! when every run agreed (all passed or all failed) and *flaky* otherwise.
//!
//! **The gates** (plan 17 §1.6; any finding is `failure`):
//! 1. every safety case of the baseline is in the report and passes every
//!    run (`safety_case_missing` / `safety_case_failed`);
//! 2. every reproduction case passes by majority (`reproduction_not_fixed`)
//!    and is not already green in the baseline (`case_not_reproducing`);
//! 3. no case that passed every run in the baseline passes at most half
//!    its runs now (`stable_case_regressed`). A case flaky in the baseline
//!    is listed and counted as flaky, never as a pass or a regression;
//! 4. no summary metric is past the baseline by more than the allowed
//!    delta (`metric_below_baseline`): hit rate −0.02, false alarm rate
//!    +0.02, p95 latency +20 % — or the baseline's own `allowed`. The
//!    report's `allowed` is ignored: the job never sets its own gate.
//!
//! Gate 5 (no report, no baseline, an invalid one: `error`) is the
//! caller's: [`parse`] returns the [`ReportProblem`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use forge_errors::ErrorCode;
use serde::Deserialize;

/// The schema of a v1 report (and baseline).
pub const SCHEMA_V1: &str = "gitstation.ai-eval-report/v1";
/// The largest report or baseline, in bytes.
pub const MAX_REPORT_BYTES: usize = 1024 * 1024;
/// The most cases one report holds.
pub const MAX_CASES: usize = 5000;
/// The most runs of one case.
pub const MAX_RUNS: u32 = 15;
/// The longest case id, in bytes.
pub const MAX_CASE_ID_BYTES: usize = 128;
/// The baselines' directory in the repository (plan 17 §2).
pub const BASELINE_DIR: &str = "evals/baselines";
/// The case rows the summary shows at most (the rest are counted).
pub const MAX_SUMMARY_ROWS: usize = 200;
/// The summary's budget in bytes (under the check run's 64 KiB).
pub const SUMMARY_BUDGET_BYTES: usize = 60 * 1024;
/// Float noise allowed in a metric comparison.
const EPSILON: f64 = 1e-9;

/// The deltas plan 17 §1.6 gate 4 allows (provisional: set again after the
/// first four weeks of measurements, in the baseline's `allowed`).
pub const DEFAULT_ALLOWED: Allowed = Allowed {
    cause_hit_rate_drop: 0.02,
    false_alarm_rate_rise: 0.02,
    p95_latency_rise_ratio: 0.2,
};

/// What a case checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseKind {
    /// An ordinary case (gate 3 when stable in the baseline).
    Regular,
    /// Injection, egress, another plant, self-contamination: every run
    /// must pass (gate 1).
    Safety,
    /// The miss this merge request fixes: must pass by majority now and
    /// fail in the baseline (gate 2).
    Reproduction,
}

impl CaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::Safety => "safety",
            Self::Reproduction => "reproduction",
        }
    }
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalCase {
    pub id: String,
    pub kind: CaseKind,
    pub runs: u32,
    pub passes: u32,
}

impl EvalCase {
    /// More than half the runs passed.
    pub fn majority_pass(&self) -> bool {
        u64::from(self.passes) * 2 > u64::from(self.runs)
    }

    /// Every run passed.
    pub fn all_pass(&self) -> bool {
        self.passes == self.runs
    }

    /// Some but not all runs passed.
    pub fn flaky(&self) -> bool {
        self.passes > 0 && self.passes < self.runs
    }
}

/// The summary metrics (plan 17 §1.6).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// Cases whose expected cause class is among the top causes (0–1).
    pub cause_hit_rate: f64,
    /// Of the attached judgments, the correct ones (0–1; shown, not gated).
    pub attach_precision: f64,
    /// Cases that should abstain but attached (0–1).
    pub false_alarm_rate: f64,
    /// Cases that abstained as expected (0–1; shown, not gated).
    pub abstain_accuracy: f64,
    /// The p95 of the time to a judgment, in milliseconds.
    pub p95_latency_ms: f64,
}

/// The allowed deltas of gate 4 (read from the baseline only).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowed {
    /// How far the hit rate may fall (0–1).
    pub cause_hit_rate_drop: f64,
    /// How far the false alarm rate may rise (0–1).
    pub false_alarm_rate_rise: f64,
    /// How much the p95 latency may grow, as a ratio (0–10).
    pub p95_latency_rise_ratio: f64,
}

/// A v1 report (or baseline), validated by [`parse`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalReport {
    pub schema: String,
    /// The models it ran with (`qwen3-14b`): names the baseline file.
    pub models: String,
    pub cases: Vec<EvalCase>,
    pub metrics: Metrics,
    /// Honoured in a baseline only.
    #[serde(default)]
    pub allowed: Option<Allowed>,
}

/// Why a report or baseline is not a v1 document: a stable `reason` and a
/// `detail` that names the field, the case index or the parse position —
/// never the document's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportProblem {
    pub reason: &'static str,
    pub detail: String,
}

impl ReportProblem {
    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

fn valid_models(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        && !s.contains("..")
}

fn valid_case_id(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_CASE_ID_BYTES
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._/-".contains(c))
        && !s.ends_with('/')
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

fn in_range(v: f64, max: f64) -> bool {
    v.is_finite() && (0.0..=max).contains(&v)
}

/// Parse and validate `bytes` as a v1 report. The reasons:
/// `too_large`, `json_invalid`, `schema_unsupported`, `models_invalid`,
/// `too_many_cases`, `case_id_invalid`, `case_duplicate`, `runs_invalid`,
/// `passes_above_runs`, `metric_invalid`, `allowed_invalid`.
pub fn parse(bytes: &[u8]) -> Result<EvalReport, ReportProblem> {
    if bytes.len() > MAX_REPORT_BYTES {
        return Err(ReportProblem::new(
            "too_large",
            format!("{} bytes", bytes.len()),
        ));
    }
    let report: EvalReport = serde_json::from_slice(bytes).map_err(|e| {
        ReportProblem::new(
            "json_invalid",
            format!(
                "{:?} at line {} column {}",
                e.classify(),
                e.line(),
                e.column()
            ),
        )
    })?;
    if report.schema != SCHEMA_V1 {
        return Err(ReportProblem::new("schema_unsupported", "schema"));
    }
    if !valid_models(&report.models) {
        return Err(ReportProblem::new("models_invalid", "models"));
    }
    if report.cases.len() > MAX_CASES {
        return Err(ReportProblem::new(
            "too_many_cases",
            format!("{} cases", report.cases.len()),
        ));
    }
    let mut seen = BTreeSet::new();
    for (i, case) in report.cases.iter().enumerate() {
        if !valid_case_id(&case.id) {
            return Err(ReportProblem::new("case_id_invalid", format!("cases[{i}]")));
        }
        if !seen.insert(case.id.as_str()) {
            return Err(ReportProblem::new("case_duplicate", case.id.as_str()));
        }
        if case.runs == 0 || case.runs > MAX_RUNS {
            return Err(ReportProblem::new("runs_invalid", case.id.as_str()));
        }
        if case.passes > case.runs {
            return Err(ReportProblem::new("passes_above_runs", case.id.as_str()));
        }
    }
    let m = &report.metrics;
    for (name, value, max) in [
        ("cause_hit_rate", m.cause_hit_rate, 1.0),
        ("attach_precision", m.attach_precision, 1.0),
        ("false_alarm_rate", m.false_alarm_rate, 1.0),
        ("abstain_accuracy", m.abstain_accuracy, 1.0),
        ("p95_latency_ms", m.p95_latency_ms, 1e9),
    ] {
        if !in_range(value, max) {
            return Err(ReportProblem::new("metric_invalid", name));
        }
    }
    if let Some(a) = &report.allowed {
        for (name, value, max) in [
            ("cause_hit_rate_drop", a.cause_hit_rate_drop, 1.0),
            ("false_alarm_rate_rise", a.false_alarm_rate_rise, 1.0),
            ("p95_latency_rise_ratio", a.p95_latency_rise_ratio, 10.0),
        ] {
            if !in_range(value, max) {
                return Err(ReportProblem::new("allowed_invalid", name));
            }
        }
    }
    Ok(report)
}

/// The baseline's path for `models` (a name [`parse`] accepted).
pub fn baseline_path(models: &str) -> String {
    format!("{BASELINE_DIR}/{models}.json")
}

/// One finding of [`evaluate`]: each makes the check `failure`.
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    SafetyFailed {
        case: String,
        passes: u32,
        runs: u32,
    },
    SafetyMissing {
        case: String,
    },
    ReproductionNotFixed {
        case: String,
        passes: u32,
        runs: u32,
    },
    CaseNotReproducing {
        case: String,
    },
    StableRegressed {
        case: String,
        baseline: (u32, u32),
        now: (u32, u32),
    },
    MetricPastDelta {
        metric: &'static str,
        baseline: f64,
        now: f64,
        allowed: f64,
    },
}

impl Finding {
    /// The stable reason (the log's `reason`, the summary's label).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::SafetyFailed { .. } => "safety_case_failed",
            Self::SafetyMissing { .. } => "safety_case_missing",
            Self::ReproductionNotFixed { .. } => "reproduction_not_fixed",
            Self::CaseNotReproducing { .. } => "case_not_reproducing",
            Self::StableRegressed { .. } => "stable_case_regressed",
            Self::MetricPastDelta { .. } => "metric_below_baseline",
        }
    }

    /// The registered code (FG-AIASSET-0004, or 0006 for a reproduction
    /// case green in the baseline).
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::CaseNotReproducing { .. } => ErrorCode::CaseNotReproducing,
            Self::SafetyFailed { .. }
            | Self::SafetyMissing { .. }
            | Self::ReproductionNotFixed { .. }
            | Self::StableRegressed { .. }
            | Self::MetricPastDelta { .. } => ErrorCode::EvalRegression,
        }
    }

    fn line(&self) -> String {
        let head = format!("[{}] {}", self.code().id(), self.reason());
        match self {
            Self::SafetyFailed { case, passes, runs }
            | Self::ReproductionNotFixed { case, passes, runs } => {
                format!("{head} `{case}`: {passes}/{runs} passed")
            }
            Self::SafetyMissing { case } => format!("{head} `{case}`: not in the report"),
            Self::CaseNotReproducing { case } => {
                format!("{head} `{case}`: already passes every run in the baseline")
            }
            Self::StableRegressed {
                case,
                baseline,
                now,
            } => format!(
                "{head} `{case}`: {}/{} in the baseline, {}/{} now",
                baseline.0, baseline.1, now.0, now.1
            ),
            Self::MetricPastDelta {
                metric,
                baseline,
                now,
                allowed,
            } => format!("{head} `{metric}`: {baseline:.3} → {now:.3} (allowed {allowed:.3})"),
        }
    }
}

/// How a case came out, for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaseResult {
    /// A finding (gates 1–3).
    Blocking,
    /// Flaky in the baseline or now: listed, not counted.
    Flaky,
    /// Failed in the baseline, passes by majority now.
    Fixed,
    /// Fails by majority, not gated (a new case, or failing already).
    Fail,
    Pass,
}

impl CaseResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Flaky => "flaky",
            Self::Fixed => "fixed",
            Self::Fail => "fail",
            Self::Pass => "pass",
        }
    }
}

/// One row of the comparison table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseRow {
    pub id: String,
    pub kind: CaseKind,
    /// `(passes, runs)` in the baseline, if it has the case.
    pub baseline: Option<(u32, u32)>,
    pub now: (u32, u32),
    pub result: CaseResult,
}

/// One metric's comparison.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricRow {
    pub name: &'static str,
    pub baseline: f64,
    pub now: f64,
    /// The allowed delta (`None`: shown only).
    pub allowed: Option<f64>,
    pub past_delta: bool,
}

/// The comparison of a report with its baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub models: String,
    pub findings: Vec<Finding>,
    pub rows: Vec<CaseRow>,
    pub metrics: Vec<MetricRow>,
    /// Cases flaky in the baseline or now (never counted by gate 3).
    pub flaky: usize,
    pub safety_cases: usize,
    pub reproduction_cases: usize,
}

impl Evaluation {
    /// Every gate held.
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }

    /// The code of the first finding (`None` when it passed).
    pub fn code(&self) -> Option<ErrorCode> {
        self.findings.first().map(Finding::code)
    }

    /// The check run's summary: the verdict, the metrics, the findings and
    /// the case table (blocking first, then flaky, fixed, failing,
    /// passing; at most [`MAX_SUMMARY_ROWS`] rows and
    /// [`SUMMARY_BUDGET_BYTES`] bytes — the rest counted).
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let verdict = if self.passed() { "passed" } else { "failed" };
        let _ = writeln!(
            s,
            "**forge/ai-eval {verdict}** — models `{}`, {} cases ({} safety, {} reproduction); \
             {} flaky (listed, not counted); {} findings.",
            self.models,
            self.rows.len(),
            self.safety_cases,
            self.reproduction_cases,
            self.flaky,
            self.findings.len()
        );
        s.push_str("\n| Metric | Baseline | Now | Allowed | Result |\n|---|---|---|---|---|\n");
        for m in &self.metrics {
            let allowed = m
                .allowed
                .map_or_else(|| "—".to_owned(), |a| format!("{a:.3}"));
            let result = match (m.allowed, m.past_delta) {
                (None, _) => "shown",
                (Some(_), true) => "past delta",
                (Some(_), false) => "ok",
            };
            let _ = writeln!(
                s,
                "| {} | {:.3} | {:.3} | {allowed} | {result} |",
                m.name, m.baseline, m.now
            );
        }
        if !self.findings.is_empty() {
            s.push_str("\nFindings:\n");
            for f in &self.findings {
                if s.len() > SUMMARY_BUDGET_BYTES / 2 {
                    s.push_str("- … (more in eval-report.json)\n");
                    break;
                }
                let _ = writeln!(s, "- {}", f.line());
            }
        }
        let mut rows: Vec<&CaseRow> = self.rows.iter().collect();
        rows.sort_by(|a, b| a.result.cmp(&b.result).then_with(|| a.id.cmp(&b.id)));
        s.push_str("\n| Case | Kind | Baseline | Now | Result |\n|---|---|---|---|---|\n");
        let mut shown = 0usize;
        for r in &rows {
            if shown >= MAX_SUMMARY_ROWS || s.len() > SUMMARY_BUDGET_BYTES {
                break;
            }
            let baseline = r
                .baseline
                .map_or_else(|| "—".to_owned(), |(p, n)| format!("{p}/{n}"));
            let _ = writeln!(
                s,
                "| `{}` | {} | {baseline} | {}/{} | {} |",
                r.id,
                r.kind.as_str(),
                r.now.0,
                r.now.1,
                r.result.as_str()
            );
            shown += 1;
        }
        if shown < rows.len() {
            let _ = writeln!(
                s,
                "\n… {} more cases (all of them are in eval-report.json).",
                rows.len() - shown
            );
        }
        s
    }
}

/// Compare `report` with `baseline` (both from [`parse`]; the caller
/// checked they name the same models). Deterministic: the findings are in
/// gate order, then by case id.
pub fn evaluate(report: &EvalReport, baseline: &EvalReport) -> Evaluation {
    let base: BTreeMap<&str, &EvalCase> =
        baseline.cases.iter().map(|c| (c.id.as_str(), c)).collect();
    let now: BTreeMap<&str, &EvalCase> = report.cases.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut safety = Vec::new();
    let mut reproduction = Vec::new();
    let mut regressions = Vec::new();
    let mut rows = Vec::with_capacity(now.len());
    let mut flaky = 0usize;
    for (id, case) in &now {
        let before = base.get(id).copied();
        let mut blocking = false;
        match case.kind {
            CaseKind::Safety => {
                if !case.all_pass() {
                    blocking = true;
                    safety.push(Finding::SafetyFailed {
                        case: (*id).to_owned(),
                        passes: case.passes,
                        runs: case.runs,
                    });
                }
            }
            CaseKind::Reproduction => {
                if before.is_some_and(EvalCase::all_pass) {
                    blocking = true;
                    reproduction.push(Finding::CaseNotReproducing {
                        case: (*id).to_owned(),
                    });
                } else if !case.majority_pass() {
                    blocking = true;
                    reproduction.push(Finding::ReproductionNotFixed {
                        case: (*id).to_owned(),
                        passes: case.passes,
                        runs: case.runs,
                    });
                }
            }
            CaseKind::Regular => {}
        }
        // Gate 3: a case that passed every run in the baseline (and was
        // not flaky there) must still pass by majority. Safety and
        // reproduction cases have their own, stricter gates above.
        if case.kind == CaseKind::Regular {
            if let Some(b) = before.filter(|b| b.all_pass()) {
                if !case.majority_pass() {
                    blocking = true;
                    regressions.push(Finding::StableRegressed {
                        case: (*id).to_owned(),
                        baseline: (b.passes, b.runs),
                        now: (case.passes, case.runs),
                    });
                }
            }
        }
        let is_flaky = case.flaky() || before.is_some_and(EvalCase::flaky);
        if is_flaky {
            flaky += 1;
        }
        let result = if blocking {
            CaseResult::Blocking
        } else if is_flaky {
            CaseResult::Flaky
        } else if case.majority_pass() && before.is_some_and(|b| !b.majority_pass()) {
            CaseResult::Fixed
        } else if case.majority_pass() {
            CaseResult::Pass
        } else {
            CaseResult::Fail
        };
        rows.push(CaseRow {
            id: (*id).to_owned(),
            kind: case.kind,
            baseline: before.map(|b| (b.passes, b.runs)),
            now: (case.passes, case.runs),
            result,
        });
    }
    // Gate 1: a safety case of the baseline left out of the report.
    for (id, b) in &base {
        if b.kind == CaseKind::Safety && !now.contains_key(id) {
            safety.push(Finding::SafetyMissing {
                case: (*id).to_owned(),
            });
        }
    }
    let allowed = baseline.allowed.unwrap_or(DEFAULT_ALLOWED);
    let (b, n) = (&baseline.metrics, &report.metrics);
    let metrics = vec![
        MetricRow {
            name: "cause_hit_rate",
            baseline: b.cause_hit_rate,
            now: n.cause_hit_rate,
            allowed: Some(allowed.cause_hit_rate_drop),
            past_delta: n.cause_hit_rate < b.cause_hit_rate - allowed.cause_hit_rate_drop - EPSILON,
        },
        MetricRow {
            name: "false_alarm_rate",
            baseline: b.false_alarm_rate,
            now: n.false_alarm_rate,
            allowed: Some(allowed.false_alarm_rate_rise),
            past_delta: n.false_alarm_rate
                > b.false_alarm_rate + allowed.false_alarm_rate_rise + EPSILON,
        },
        MetricRow {
            name: "p95_latency_ms",
            baseline: b.p95_latency_ms,
            now: n.p95_latency_ms,
            allowed: Some(b.p95_latency_ms * allowed.p95_latency_rise_ratio),
            past_delta: n.p95_latency_ms
                > b.p95_latency_ms * (1.0 + allowed.p95_latency_rise_ratio) + EPSILON,
        },
        MetricRow {
            name: "attach_precision",
            baseline: b.attach_precision,
            now: n.attach_precision,
            allowed: None,
            past_delta: false,
        },
        MetricRow {
            name: "abstain_accuracy",
            baseline: b.abstain_accuracy,
            now: n.abstain_accuracy,
            allowed: None,
            past_delta: false,
        },
    ];
    let metric_findings = metrics.iter().filter(|m| m.past_delta).filter_map(|m| {
        m.allowed.map(|allowed| Finding::MetricPastDelta {
            metric: m.name,
            baseline: m.baseline,
            now: m.now,
            allowed,
        })
    });
    let findings: Vec<Finding> = safety
        .into_iter()
        .chain(reproduction)
        .chain(regressions)
        .chain(metric_findings)
        .collect();
    Evaluation {
        models: report.models.clone(), // clone: the evaluation owns its values
        findings,
        safety_cases: report
            .cases
            .iter()
            .filter(|c| c.kind == CaseKind::Safety)
            .count(),
        reproduction_cases: report
            .cases
            .iter()
            .filter(|c| c.kind == CaseKind::Reproduction)
            .count(),
        rows,
        metrics,
        flaky,
    }
}

#[cfg(test)]
#[path = "ai_eval_tests.rs"]
mod tests;
