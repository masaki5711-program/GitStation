//! The `risk_sweep` task of `forge worker` (M3 X7, as `spec_drift_sweep`):
//! every 60 s it answers `error` for each `check.risk` job that died
//! without a `forge/risk` run on its head
//! (`forge_db::checks::risk::unanswered_dead_jobs`: the reaper ended it — a
//! crashed worker, the runner's own timeout — so the handler never
//! answered). The dead jobs are walked **by id with a cursor**, at most
//! [`BATCH`] per run, wrapping to the start after the last (review X7: a
//! job whose answer keeps failing cannot starve the ones after it; jobs of
//! soft-deleted projects are not listed at all). A full batch runs again at
//! once. Each answer carries the job's code and its request id
//! (`forge_checks::risk::record_error`, which logs a failure to record it);
//! an answered head is not seen again. A failed run (the database) is
//! returned and logged once per failure streak by the supervisor with
//! `error_code` and `task=risk_sweep`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use forge_checks::risk::record_error;
use forge_core::RequestId;
use forge_db::checks::risk::unanswered_dead_jobs;
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "risk_sweep";
/// The task's period.
pub(crate) const PERIOD: Duration = Duration::from_secs(60);
/// Jobs answered per run at most.
const BATCH: i64 = 50;

/// Where the next run starts (the last job id seen; `0`: the start).
static CURSOR: AtomicI64 = AtomicI64::new(0);

/// One run (module docs).
pub(crate) async fn run_risk_sweep(db: &Db) -> Result<Next, ForgeError> {
    sweep(db, &CURSOR, BATCH).await
}

/// One run with its own cursor and batch size (tests).
pub(crate) async fn sweep(db: &Db, cursor: &AtomicI64, batch: i64) -> Result<Next, ForgeError> {
    let after = cursor.load(Ordering::Relaxed);
    let jobs = unanswered_dead_jobs(db, after, batch).await?;
    let mut answered = 0usize;
    for job in &jobs {
        let request_id = match RequestId::parse(&job.origin_request_id) {
            Some(id) => id,
            None => {
                let fresh = RequestId::generate();
                tracing::warn!(
                    target: "forge_cli",
                    task = NAME,
                    event = "forge.worker.risk_sweep_request_id_replaced",
                    job_id = job.job_id,
                    request_id = %fresh,
                    "a dead risk job's request id is unreadable; its answer carries a new one"
                );
                fresh
            }
        };
        if record_error(db, &job.request, &request_id, &job.last_error_code).await {
            answered += 1;
        }
    }
    if !jobs.is_empty() {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            event = "forge.worker.risk_swept",
            found = jobs.len(),
            answered,
            "dead risk jobs answered with error"
        );
    }
    let full = i64::try_from(jobs.len()).unwrap_or(i64::MAX) >= batch;
    let next_cursor = match (full, jobs.last()) {
        (true, Some(last)) => last.job_id,
        _ => 0,
    };
    cursor.store(next_cursor, Ordering::Relaxed);
    Ok(if full {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
