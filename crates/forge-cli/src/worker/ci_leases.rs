//! The `ci_lease_reaper` task of `forge worker` (M4 C3, plan 14 §5 C3,
//! ADR-0029): every [`PERIOD`] it takes back the CI jobs whose runner's
//! lease ended (`forge_pipelines::runners::reap_leases`: requeued once,
//! failed with `FG-CI-0004` at the third lost lease, or a cancelled job
//! released; each logged as `forge.ci.job_lease_lost`), then checks for
//! jobs waiting with no runner (`FG-CI-0005`, the alert, logged at most
//! every 10 minutes). A full batch runs again at once.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`) is returned and
//! logged once per failure streak by the supervisor with `error_code` and
//! `task=ci_lease_reaper`, then retried after the backoff. A single job
//! the pass could not take back does not stop the pass: the others are
//! taken back first, then the first such failure is returned the same way
//! (`reap_leases` logs any further ones itself).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use forge_core::RequestId;
use forge_db::ci_runners::lease::REAP_BATCH;
use forge_db::Db;
use forge_errors::ForgeError;
use forge_pipelines::runners::{check_runners, reap_leases};

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "ci_lease_reaper";
/// How often it runs (a lease is 120 s; a lost one is back in the queue
/// within about this much after it ended).
pub(crate) const PERIOD: Duration = Duration::from_secs(10);

/// When `FG-CI-0005` was last logged by this process.
static NO_RUNNER_LOGGED: Mutex<Option<Instant>> = Mutex::new(None);

/// One run: one reaper pass, then the no-runner check.
pub(crate) async fn run_ci_lease_reaper(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let reaped = reap_leases(db, &request_id).await?;
    // A poisoned lock only means a panic elsewhere while it was held; the
    // value is a timestamp, still usable.
    let last = *NO_RUNNER_LOGGED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if check_runners(db, &request_id, last).await? {
        *NO_RUNNER_LOGGED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }
    if i64::try_from(reaped).unwrap_or(i64::MAX) >= REAP_BATCH {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run with no held job waits for the period.
    #[tokio::test]
    async fn a_reaper_run_with_nothing_held_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_ci_lease_reaper(&t.db).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
