//! The `risk_sweep` task of `forge worker` (M3 X7, as `spec_drift_sweep`):
//! every 60 s it answers `error` for each `check.risk` job that died
//! without a `forge/risk` run on its head
//! (`forge_db::checks::risk::unanswered_dead_jobs`: the reaper ended it — a
//! crashed worker, the runner's own timeout — so the handler never
//! answered). At most [`BATCH`] per run, the oldest first; a full batch runs
//! again at once. Each answer carries the job's code and its request id
//! (`forge_checks::risk::record_error`, which logs a failure to record it);
//! an answered head is not seen again. A failed run (the database) is
//! returned and logged once per failure streak by the supervisor with
//! `error_code` and `task=risk_sweep`.

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

/// One run (module docs).
pub(crate) async fn run_risk_sweep(db: &Db) -> Result<Next, ForgeError> {
    let jobs = unanswered_dead_jobs(db, BATCH).await?;
    let mut answered = 0usize;
    for job in &jobs {
        let request_id =
            RequestId::parse(&job.origin_request_id).unwrap_or_else(RequestId::generate);
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
    let full = i64::try_from(jobs.len()).unwrap_or(i64::MAX) >= BATCH;
    Ok(if full && answered > 0 {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
