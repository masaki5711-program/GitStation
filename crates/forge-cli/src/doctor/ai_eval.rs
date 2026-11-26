//! `forge doctor` for the AI evaluation bench (plan 17 P17-8, §5.3;
//! runbook ai-eval-bench), from `forge_db::ci_ai_eval::health` on the read
//! pool. Counts and ids only.
//!
//! - `ai_eval.runner` — `FG-AIASSET-0002 reason=gpu_runner_unavailable`
//!   when a `gpu-eval` job has waited over 10 minutes and no `gpu-eval`
//!   runner was heard from in 5 minutes (the bench cannot run: every
//!   `forge/ai-eval` waits, then turns `error`). A warning when such a job
//!   waits while a `gpu-eval` runner is live (the runner is busy, or the
//!   project is not in `ci.gpu_eval_projects`), or when pipelines declared
//!   `eval-report.json` in the last 7 days and no `gpu-eval` runner is live.
//!   Ok otherwise, also when nothing uses the bench.
//! - `ai_eval.baseline_age` — `FG-AIASSET-0002 reason=nightly_not_passing`
//!   when a project with an active schedule whose pipelines declare the
//!   report has had no succeeded scheduled pipeline in 48 hours (two
//!   nights: no baseline candidate can come; runbook ai-eval-bench says
//!   SEV3). A warning when a baseline candidate merge request (branch
//!   `ai-eval/baseline-*`, plan 17 P17-6) has been open over 14 days (two
//!   weekly reviews missed: merge requests compare against an old
//!   baseline), or when a project with `ai_assets.enabled` has no nightly
//!   bench in 30 days. Ok otherwise.

use forge_db::ci_ai_eval::health::{
    baseline_health, runner_health, BaselineHealth, RunnerHealth, CANDIDATE_STALE_AFTER_S,
    IN_USE_WINDOW_S, NIGHTLY_STALE_AFTER_S, NIGHTLY_WINDOW_S,
};
use forge_db::ci_runners::lease::{NO_RUNNER_AFTER_S, RUNNER_SILENT_AFTER_S};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, warn, Check};

pub(super) const RUNNER: &str = "ai_eval.runner";
pub(super) const BASELINE_AGE: &str = "ai_eval.baseline_age";

const RUNBOOK: &str = "ai-eval-bench";
const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;

fn ids(list: &[i64]) -> String {
    list.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// `ai_eval.runner` for `h` (pure: the tests feed reports).
fn runner_line(h: &RunnerHealth) -> Check {
    let wait_min = NO_RUNNER_AFTER_S / 60;
    let silent_min = RUNNER_SILENT_AFTER_S / 60;
    if h.waiting > 0 && h.live_gpu_runners == 0 {
        let mut e = ForgeError::new(ErrorCode::BenchUnavailable)
            .with_context("reason", "gpu_runner_unavailable")
            .with_context("waiting", h.waiting);
        if let Some(id) = h.oldest_job_id {
            e = e.with_context("oldest_job_id", id);
        }
        if let Some(wait) = h.oldest_wait_s {
            e = e.with_context("oldest_wait_s", wait);
        }
        return fail(
            RUNNER,
            e.with_context("gpu_runners", h.gpu_runners)
                .with_context("live_gpu_runners", h.live_gpu_runners)
                .with_context("runbook", RUNBOOK),
        );
    }
    if h.waiting > 0 {
        return warn(
            RUNNER,
            &format!(
                "{} gpu-eval job(s) pending over {wait_min} min (oldest job {}) with {} live \
                 gpu-eval runner(s): the runner is busy, or the project is not in \
                 ci.gpu_eval_projects (runbooks {RUNBOOK}, ci-runner)",
                h.waiting,
                h.oldest_job_id
                    .map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
                h.live_gpu_runners
            ),
        );
    }
    let days = IN_USE_WINDOW_S / DAY;
    if h.declaring_pipelines == 0 && h.gpu_runners == 0 {
        return ok(
            RUNNER,
            &format!("not in use (no pipeline declared eval-report.json in {days} days, no gpu-eval runner)"),
        );
    }
    if h.declaring_pipelines > 0 && h.live_gpu_runners == 0 {
        return warn(
            RUNNER,
            &format!(
                "{} pipeline(s) declared eval-report.json in {days} days; {} gpu-eval runner(s) \
                 enabled, none heard from in {silent_min} min (runbook {RUNBOOK})",
                h.declaring_pipelines, h.gpu_runners
            ),
        );
    }
    ok(
        RUNNER,
        &format!(
            "{} gpu-eval runner(s), {} live; {} bench pipeline(s) in {days} days; \
             no gpu-eval job waits over {wait_min} min",
            h.gpu_runners, h.live_gpu_runners, h.declaring_pipelines
        ),
    )
}

/// `ai_eval.baseline_age` for `h` (pure: the tests feed reports).
fn baseline_line(h: &BaselineHealth) -> Check {
    if h.stale_count > 0 {
        return fail(
            BASELINE_AGE,
            ForgeError::new(ErrorCode::BenchUnavailable)
                .with_context("reason", "nightly_not_passing")
                .with_context("stale_projects", h.stale_count)
                .with_context("project_ids", ids(&h.stale_projects))
                .with_context("nightly_projects", h.nightly_projects)
                .with_context("stale_after_h", NIGHTLY_STALE_AFTER_S / HOUR)
                .with_context("runbook", RUNBOOK),
        );
    }
    if let Some((project, iid, age)) = h.stale_candidate() {
        return warn(
            BASELINE_AGE,
            &format!(
                "baseline candidate !{iid} of project {project} open {} days (over {}): \
                 review and merge or close it; {} open candidate(s) (runbook {RUNBOOK})",
                age / DAY,
                CANDIDATE_STALE_AFTER_S / DAY,
                h.open_candidates
            ),
        );
    }
    let window = NIGHTLY_WINDOW_S / DAY;
    if !h.enabled_without_nightly.is_empty() {
        return warn(
            BASELINE_AGE,
            &format!(
                "project(s) {} have ai_assets.enabled and no nightly bench in {window} days: \
                 create a schedule whose pipeline declares eval-report.json (runbook {RUNBOOK})",
                ids(&h.enabled_without_nightly)
            ),
        );
    }
    if h.nightly_projects == 0 {
        return ok(
            BASELINE_AGE,
            &format!(
                "no nightly bench (no active schedule whose pipelines declared \
                 eval-report.json in {window} days)"
            ),
        );
    }
    let candidates = match h.oldest_candidate {
        Some((_, iid, age)) => format!(
            "{} open baseline candidate(s), oldest !{iid} {} days",
            h.open_candidates,
            age / DAY
        ),
        None => "no open baseline candidate".to_owned(),
    };
    ok(
        BASELINE_AGE,
        &format!(
            "{} nightly project(s), each passed within {} h; {candidates}",
            h.nightly_projects,
            h.fresh_oldest_s.unwrap_or(0) / HOUR + 1
        ),
    )
}

/// `ai_eval.runner`.
pub(super) async fn runner_check(db: &Db) -> Check {
    match runner_health(db).await {
        Ok(h) => runner_line(&h),
        Err(e) => fail(RUNNER, e),
    }
}

/// `ai_eval.baseline_age`.
pub(super) async fn baseline_check(db: &Db) -> Check {
    match baseline_health(db).await {
        Ok(h) => baseline_line(&h),
        Err(e) => fail(BASELINE_AGE, e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Outcome;
    use super::*;

    fn detail(c: &Check) -> String {
        match &c.outcome {
            Outcome::Ok(s) | Outcome::Warn(s) => s.clone(), // clone: the message for the assertion
            Outcome::Fail(e) => e.code().id().to_owned(),
            Outcome::Skip(s) => (*s).to_owned(),
        }
    }

    fn context(c: &Check, key: &str) -> Option<String> {
        let Outcome::Fail(e) = &c.outcome else {
            return None;
        };
        e.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone()) // clone: the value for the assertion
    }

    #[test]
    fn an_unused_bench_is_ok() {
        let c = runner_line(&RunnerHealth::default());
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        assert!(detail(&c).starts_with("not in use"), "{}", detail(&c));
        let c = baseline_line(&BaselineHealth::default());
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        assert!(detail(&c).starts_with("no nightly bench"), "{}", detail(&c));
    }

    #[test]
    fn a_waiting_gpu_job_without_a_live_runner_is_bench_unavailable() {
        let h = RunnerHealth {
            declaring_pipelines: 3,
            gpu_runners: 1,
            live_gpu_runners: 0,
            waiting: 2,
            oldest_job_id: Some(41),
            oldest_wait_s: Some(1800),
        };
        let c = runner_line(&h);
        assert_eq!(detail(&c), "FG-AIASSET-0002");
        assert_eq!(
            context(&c, "reason").as_deref(),
            Some("gpu_runner_unavailable")
        );
        assert_eq!(context(&c, "oldest_job_id").as_deref(), Some("41"));
        assert_eq!(context(&c, "runbook").as_deref(), Some("ai-eval-bench"));
        // A live runner: the job waits for something else (busy, policy).
        let c = runner_line(&RunnerHealth {
            live_gpu_runners: 1,
            ..h
        });
        assert!(matches!(c.outcome, Outcome::Warn(_)));
        assert!(
            detail(&c).contains("ci.gpu_eval_projects"),
            "{}",
            detail(&c)
        );
    }

    #[test]
    fn a_used_bench_without_a_live_runner_warns() {
        let h = RunnerHealth {
            declaring_pipelines: 2,
            gpu_runners: 1,
            ..RunnerHealth::default()
        };
        assert!(matches!(runner_line(&h).outcome, Outcome::Warn(_)));
        let live = RunnerHealth {
            live_gpu_runners: 1,
            ..h
        };
        let c = runner_line(&live);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        assert_eq!(
            detail(&c),
            "1 gpu-eval runner(s), 1 live; 2 bench pipeline(s) in 7 days; \
             no gpu-eval job waits over 10 min"
        );
    }

    #[test]
    fn a_nightly_without_a_recent_success_fails() {
        let h = BaselineHealth {
            nightly_projects: 2,
            stale_projects: vec![7],
            stale_count: 1,
            ..BaselineHealth::default()
        };
        let c = baseline_line(&h);
        assert_eq!(detail(&c), "FG-AIASSET-0002");
        assert_eq!(
            context(&c, "reason").as_deref(),
            Some("nightly_not_passing")
        );
        assert_eq!(context(&c, "project_ids").as_deref(), Some("7"));
        assert_eq!(context(&c, "stale_after_h").as_deref(), Some("48"));
    }

    #[test]
    fn an_old_candidate_and_an_asset_project_without_a_nightly_warn() {
        let fresh = BaselineHealth {
            nightly_projects: 1,
            fresh_oldest_s: Some(20 * HOUR),
            open_candidates: 1,
            oldest_candidate: Some((7, 3, 2 * DAY)),
            ..BaselineHealth::default()
        };
        let c = baseline_line(&fresh);
        assert!(matches!(c.outcome, Outcome::Ok(_)));
        assert_eq!(
            detail(&c),
            "1 nightly project(s), each passed within 21 h; \
             1 open baseline candidate(s), oldest !3 2 days"
        );
        let old = BaselineHealth {
            oldest_candidate: Some((7, 3, 20 * DAY)),
            ..fresh.clone() // clone: a second report from the first
        };
        let c = baseline_line(&old);
        assert!(matches!(c.outcome, Outcome::Warn(_)));
        assert!(
            detail(&c).contains("!3 of project 7 open 20 days"),
            "{}",
            detail(&c)
        );
        let unscheduled = BaselineHealth {
            enabled_without_nightly: vec![8, 9],
            ..fresh
        };
        let c = baseline_line(&unscheduled);
        assert!(matches!(c.outcome, Outcome::Warn(_)));
        assert!(detail(&c).contains("project(s) 8,9"), "{}", detail(&c));
    }
}
