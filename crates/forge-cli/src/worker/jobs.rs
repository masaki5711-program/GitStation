//! The job tasks of `forge worker` (ADR-0016 §7): the job runner (claims
//! one job at a time for the kinds with a registered handler, every second
//! while idle, at once again after a job) and the reaper (every 30 s, also
//! once at startup so leases left by a crashed worker are ended early).
//!
//! Handlers are registered in [`handlers`]: `repository.provision` (step
//! D4 MR3b, `provision`), `merge_request.refresh` (M3 R1b, `review`),
//! `merge_request.mergeability` (M3 R2, `review`),
//! `merge_request.conflict_scan` (M3 X6, `review`) and `check.spec_drift`
//! (M3 X2, `spec_drift`) and `check.risk` (M3 X7, `risk`). The audit seal is a task (`seal`), not a job.
//!
//! Shutdown: a job may run for up to its timeout (≤ 1 h), longer than a
//! stop should wait. A job in progress gets [`SHUTDOWN_GRACE`]; then its
//! handler is aborted and, once it has ended, the job is handed back
//! (`queued`, ready now, the attempt not counted — a shutdown is not the
//! job's fault; `FG-DB-0007 reason=released_on_shutdown`). See
//! `forge_db::jobs::JobRunner::run_once_until`.
//!
//! Worst case a stop waits for the job runner: [`SHUTDOWN_GRACE`] (10 s) +
//! the abort wait (`forge_db::jobs::ABORT_WAIT`, 5 s) + the hand-back
//! (`forge_db::jobs::RELEASE_TIMEOUT`, 5 s) ≈ 20 s. A handler that does not
//! end after the abort keeps its job leased (`left_leased`) instead of
//! being released, so it can never overlap the next attempt.

use std::sync::Arc;
use std::time::Duration;

use forge_db::jobs::{JobHandler, JobRunner, Ran, REAP_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;
use tokio::sync::watch;

use super::pipelines::PipelineHandler;
use super::provision::{ProvisionHandler, Storage};
use super::review::{ConflictScanHandler, MergeabilityHandler, RefreshHandler};
use super::risk::RiskHandler;
use super::spec_drift::SpecDriftHandler;
use super::spec_index::SpecIndexHandler;
use super::supervise::Next;

/// How often an idle runner looks for a ready job.
pub(crate) const JOB_POLL_PERIOD: Duration = Duration::from_secs(1);
/// How often expired leases are reaped (ADR-0016 §7).
pub(crate) const REAP_PERIOD: Duration = Duration::from_secs(30);
/// How long a job in progress may finish after a shutdown request.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Every job handler of this build: `repository.provision` (D4 MR3b),
/// `merge_request.refresh` (M3 R1b: merge request versions),
/// `merge_request.mergeability` (M3 R2: the trial merge),
/// `merge_request.conflict_scan` (M3 X6: conflicts between merge requests),
/// `check.spec_drift` (M3 X2: `forge/spec-drift`), `check.risk` (M3 X7:
/// `forge/risk`) and `check.spec_index` (M3 X4: `forge/spec-lint` and
/// `forge/spec-trace`; plan 13 stage 2: `forge/doc-lint`; plan 14 C1:
/// `forge/ci-lint`).
pub(crate) fn handlers(db: &Db, storage: &Storage) -> Result<Vec<Arc<dyn JobHandler>>, ForgeError> {
    // clone: the handler owns its storage handles (paths and an Arc)
    let provision = ProvisionHandler::new(db, storage.clone())?;
    let refresh = RefreshHandler::new(db, storage)?;
    let mergeability = MergeabilityHandler::new(db, storage)?;
    let conflicts = ConflictScanHandler::new(db, storage)?;
    let spec_drift = SpecDriftHandler::new(db, storage)?;
    let risk = RiskHandler::new(db, storage)?;
    let spec_index = SpecIndexHandler::new(db, storage)?;
    // M4 C2: `ci.pipeline_create` (CI pipelines from pushes and versions).
    let pipelines = PipelineHandler::new(db, storage)?;
    Ok(vec![
        Arc::new(provision),
        Arc::new(refresh),
        Arc::new(mergeability),
        Arc::new(conflicts),
        Arc::new(spec_drift),
        Arc::new(risk),
        Arc::new(spec_index),
        Arc::new(pipelines),
    ])
}

/// One run of the job runner: one job (or none); a shutdown request gives
/// the job in progress `grace`, then releases it.
pub(crate) async fn run_job(
    jobs: &JobRunner,
    grace: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<Next, ForgeError> {
    let stop = async move {
        // The watch guard is dropped inside this statement, never held
        // across an await. A closed channel without a request never stops
        // the run (the task loop reports the channel).
        let closed = shutdown.wait_for(|stop| *stop).await.is_err();
        if closed {
            std::future::pending::<()>().await;
        }
    };
    Ok(match jobs.run_once_until(stop, grace).await? {
        // A job ran: more may be ready, look again at once.
        Some(
            Ran::Succeeded(_)
            | Ran::Retrying { .. }
            | Ran::Dead { .. }
            | Ran::Fenced(_)
            | Ran::Released { .. }
            | Ran::LeftLeased { .. }
            | Ran::Unrecorded { .. },
        ) => Next::Immediately,
        None => Next::AfterPeriod,
    })
}

/// One run of the reaper (it logs every expired lease itself).
pub(crate) async fn reap(jobs: &JobRunner) -> Result<Next, ForgeError> {
    let reported = jobs.reap_and_report().await?;
    let full = i64::try_from(reported.len()).unwrap_or(i64::MAX) >= REAP_BATCH;
    Ok(if full {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
