//! What the job runner reports and logs: the failure of a handler's task,
//! `FG-DB-0007` about a claimed job, `FG-DB-0006` for a dead job, and the
//! report of each reaped lease.

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinError;

use super::runner::{Ran, Reported};
use super::{ClaimedJob, JobFailure, JobId, ReapedJob};

/// The handler's result, or the failure of its task.
pub(super) fn finished(
    joined: Result<Result<(), JobFailure>, JoinError>,
) -> Result<(), JobFailure> {
    joined.unwrap_or_else(|e| {
        Err(JobFailure::retry(
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "jobs.run")
                .with_context(
                    "reason",
                    if e.is_panic() {
                        "job_panicked"
                    } else {
                        "job_cancelled"
                    },
                ),
        ))
    })
}

/// `FG-DB-0007` about a claimed job.
pub(super) fn job_error(
    job: &ClaimedJob,
    operation: &'static str,
    reason: &'static str,
) -> ForgeError {
    ForgeError::new(ErrorCode::JobLeaseExpired)
        .with_context("operation", operation)
        .with_context("reason", reason)
        .with_context("job_id", job.lease.job_id.get())
        .with_context("kind", job.kind.as_str())
        .with_context("worker_id", job.lease.worker_id.as_str())
        .with_context("attempt", job.lease.attempt)
        .with_context("origin_request_id", &job.origin_request_id)
}

pub(super) fn dead_lettered(
    job_id: JobId,
    kind: &str,
    attempts: i32,
    last_error_code: &str,
    origin_request_id: &str,
) -> ForgeError {
    ForgeError::new(ErrorCode::JobDeadLettered)
        .with_context("job_id", job_id.get())
        .with_context("kind", kind)
        .with_context("attempts", attempts)
        .with_context("last_error_code", last_error_code)
        .with_context("origin_request_id", origin_request_id)
}

pub(super) fn report_reaped(job: &ReapedJob) -> Reported {
    let request_id = RequestId::parse(&job.origin_request_id).unwrap_or_else(RequestId::generate);
    let reported = if job.dead {
        Reported::Dead(
            dead_lettered(
                job.job_id,
                &job.kind,
                job.attempts,
                ErrorCode::JobLeaseExpired.id(),
                &job.origin_request_id,
            )
            .with_context("reason", "lease_expired"),
        )
    } else {
        Reported::Requeued(
            ForgeError::new(ErrorCode::JobLeaseExpired)
                .with_context("operation", "jobs.reap")
                .with_context("reason", "lease_expired")
                .with_context("job_id", job.job_id.get())
                .with_context("kind", &job.kind)
                .with_context("worker_id", job.worker_id.as_deref().unwrap_or("none"))
                .with_context("attempt", job.attempts)
                .with_context("origin_request_id", &job.origin_request_id),
        )
    };
    match &reported {
        Reported::Requeued(e) | Reported::Dead(e) => telemetry::log_failure(e, &request_id),
    }
    reported
}

/// Log what the run reports (a handler failure was logged when it happened).
pub(super) fn log_outcome(outcome: &Ran, request_id: &RequestId) {
    match outcome {
        Ran::Succeeded(_) | Ran::Retrying { .. } => {}
        Ran::Dead { report: e, .. }
        | Ran::Fenced(e)
        | Ran::Released { report: e, .. }
        | Ran::LeftLeased { report: e, .. } => telemetry::log_failure(e, request_id),
    }
}
