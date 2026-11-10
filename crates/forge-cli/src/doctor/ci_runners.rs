//! The CI runners check of `forge doctor` (M4 C3, plan 14 §6, runbook
//! ci-runner), from `forge_db::ci_runners::lease::health` on the read pool:
//!
//! - `ci.runners` — `FG-CI-0005` when jobs have waited longer than 10
//!   minutes without a runner claiming them (no runner registered or
//!   enabled, all silent, or none with the jobs' tags); a warning when no
//!   runner is enabled or none was heard from in 5 minutes while nothing
//!   waits; otherwise ok. Counts and one job id only.

use forge_db::ci_runners::lease::{health, RunnerHealth};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, warn, Check};

pub(super) const NAME: &str = "ci.runners";

/// `ci.runners` for `h` (pure: the tests feed reports).
fn line(h: &RunnerHealth) -> Check {
    if h.no_runner_available() {
        return fail(
            NAME,
            ForgeError::new(ErrorCode::NoRunnerAvailable)
                .with_context("waiting", h.waiting)
                .with_context("oldest_job_id", h.oldest_job_id.unwrap_or(0))
                .with_context("oldest_wait_s", h.oldest_wait_s.unwrap_or(0))
                .with_context("enabled_runners", h.enabled_runners)
                .with_context("live_runners", h.live_runners)
                .with_context("runbook", "ci-runner"),
        );
    }
    if h.enabled_runners == 0 || h.live_runners == 0 {
        return warn(
            NAME,
            &format!(
                "{} enabled runner(s), {} heard from in 5 min; no job waits yet (runbook ci-runner)",
                h.enabled_runners, h.live_runners
            ),
        );
    }
    ok(
        NAME,
        &format!(
            "{} enabled runner(s), {} live; no job waits over 10 min",
            h.enabled_runners, h.live_runners
        ),
    )
}

/// The check.
pub(super) async fn runners_check(db: &Db) -> Check {
    match health(db).await {
        Ok(h) => line(&h),
        Err(e) => fail(NAME, e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Outcome;
    use super::*;

    #[test]
    fn waiting_jobs_fail_and_no_live_runner_warns() {
        let live = RunnerHealth {
            enabled_runners: 2,
            live_runners: 1,
            ..RunnerHealth::default()
        };
        assert!(matches!(line(&live).outcome, Outcome::Ok(_)));
        assert!(matches!(
            line(&RunnerHealth::default()).outcome,
            Outcome::Warn(_)
        ));
        let waiting = RunnerHealth {
            waiting: 3,
            oldest_job_id: Some(7),
            oldest_wait_s: Some(900),
            ..live
        };
        let Outcome::Fail(e) = line(&waiting).outcome else {
            panic!("waiting jobs fail");
        };
        assert_eq!(e.code(), ErrorCode::NoRunnerAvailable);
        assert!(e
            .context()
            .iter()
            .any(|(k, v)| *k == "oldest_job_id" && v == "7"));
    }
}
