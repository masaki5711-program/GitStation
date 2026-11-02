//! The merge job runner of `forge serve` (M3 R4, ADR-0024 §4.2; ADR-0024
//! "実装で決めたこと（R4）"): the job `merge_request.merge` runs in the
//! process that serves the browse API, so the merge's ref writes drop the
//! same browse cache the next request reads (`Browser::invalidate`; the
//! worker process has no browse cache to drop). `forge worker` does not
//! register the kind, so only `forge serve` claims it; the worker's reaper
//! still requeues an expired lease.
//!
//! Two loops on one task: the job runner (one merge at a time, every second
//! while idle, at once again after a job) and the reconciliation
//! ([`forge_review::Merger::reconcile`], every [`RECONCILE_PERIOD`]). A
//! shutdown gives the job in progress [`SHUTDOWN_GRACE`], then releases it
//! (`forge_db::jobs::JobRunner::run_once_until`); an attempt that was
//! `executing` is the reconciliation's after the restart.
//!
//! The handler: success or a decision (a refusal, a conflict, a lost
//! compare-and-swap) completes the job; a failure that may pass later
//! (git, storage, the database, the merge lock, a locked ref, FG-REVIEW-0009
//! — the attempt stays `executing` and a retry asks git first) is retried
//! with the queue's backoff; on the last attempt, or a failure that can
//! never pass, an attempt that never passed the fence ends `superseded
//! reason=job_failed` (`Merger::give_up`) before the job dies.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind, JobRunner, WorkerId};
use forge_db::review::merges::{merge_payload, MERGE_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::Merger;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// How often an idle runner looks for a merge.
const POLL_PERIOD: Duration = Duration::from_secs(1);
/// How often stuck attempts are reconciled.
pub(crate) const RECONCILE_PERIOD: Duration = Duration::from_secs(60);
/// How long a merge in progress may finish after a shutdown request.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The handler of `merge_request.merge`.
pub(crate) struct MergeHandler {
    kind: JobKind,
    merger: Arc<Merger>,
}

impl MergeHandler {
    pub(crate) fn new(merger: Arc<Merger>) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(MERGE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", MERGE_JOB)
        })?;
        Ok(Self { kind, merger })
    }
}

/// Whether a failed run may pass later.
pub(crate) fn retryable(e: &ForgeError) -> bool {
    matches!(
        e.code(),
        ErrorCode::GitTimeout
            | ErrorCode::GitBusy
            | ErrorCode::GitProcessFailed
            | ErrorCode::GitUnavailable
            | ErrorCode::StorageIoFailed
            | ErrorCode::DbUnavailable
            | ErrorCode::DbQueryFailed
            | ErrorCode::RepositoryNotReady
            | ErrorCode::MergeExecutionFailed
    )
}

impl JobHandler for MergeHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let merger = Arc::clone(&self.merger);
        Box::pin(async move {
            let merge_id = merge_payload(&job.payload).map_err(JobFailure::permanent)?;
            let Err(e) = merger.execute(merge_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good: never leave an attempt that never moved a ref
            // live (an `executing` one is the reconciliation's).
            if let Err(cleanup) = merger.give_up(merge_id, &e).await {
                telemetry::log_failure(
                    &cleanup
                        .with_context("operation", "merge_give_up")
                        .with_context("merge_id", merge_id),
                    &job.request_id(),
                );
            }
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}

/// Start the merge runner and the reconciliation; they stop when `stop`
/// turns true. The task's own failures are logged and retried after
/// [`POLL_PERIOD`].
pub(crate) fn spawn(
    db: &Db,
    merger: Arc<Merger>,
    stop: watch::Receiver<bool>,
) -> Result<JoinHandle<()>, ForgeError> {
    merger.timeouts().check()?;
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(MergeHandler::new(Arc::clone(&merger))?))?;
    tracing::info!(
        worker_id = runner.worker_id().as_str(),
        kind = MERGE_JOB,
        "forge.serve.merge_runner_started"
    );
    Ok(tokio::spawn(run(runner, merger, stop)))
}

async fn run(runner: JobRunner, merger: Arc<Merger>, mut stop: watch::Receiver<bool>) {
    let mut next_reconcile = tokio::time::Instant::now();
    loop {
        if *stop.borrow() {
            break;
        }
        if tokio::time::Instant::now() >= next_reconcile {
            next_reconcile = tokio::time::Instant::now() + RECONCILE_PERIOD;
            let rid = RequestId::generate();
            if let Err(e) = merger.reconcile(&rid).await {
                telemetry::log_failure(&e.with_context("operation", "merge_reconcile"), &rid);
            }
        }
        let mut job_stop = stop.clone(); // clone: the run waits on its own receiver
        let stopping = async move {
            let closed = job_stop.wait_for(|s| *s).await.is_err();
            if closed {
                std::future::pending::<()>().await;
            }
        };
        let ran = runner.run_once_until(stopping, SHUTDOWN_GRACE).await;
        let idle = match ran {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(e) => {
                telemetry::log_failure(
                    &e.with_context("operation", "merge_runner_claim"),
                    &RequestId::generate(),
                );
                true
            }
        };
        if idle {
            tokio::select! {
                () = tokio::time::sleep(POLL_PERIOD) => {}
                changed = stop.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
    }
    tracing::info!("forge.serve.merge_runner_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_that_can_pass_later_are_retried_and_decisions_are_not() {
        for code in [
            ErrorCode::GitTimeout,
            ErrorCode::DbQueryFailed,
            ErrorCode::MergeExecutionFailed,
        ] {
            assert!(retryable(&ForgeError::new(code)), "{code:?}");
        }
        for code in [ErrorCode::Internal, ErrorCode::NotFound] {
            assert!(!retryable(&ForgeError::new(code)), "{code:?}");
        }
    }
}
