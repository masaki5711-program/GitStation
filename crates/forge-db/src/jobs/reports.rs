//! What the job runner reports and logs: the outcome of a run ([`Ran`]),
//! of a reaped lease ([`Reported`]), the failure of a handler's task,
//! `FG-DB-0007` about a claimed job and `FG-DB-0006` for a dead job.

use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use tokio::task::JoinError;

use super::{ClaimedJob, JobFailure, JobId, ReapedJob};

/// What [`super::JobRunner::run_once`] did with the job it claimed.
#[derive(Debug)]
pub enum Ran {
    Succeeded(JobId),
    /// The run failed and the job is queued again (the failure was logged).
    Retrying {
        job_id: JobId,
        failure: ForgeError,
        retry_in: Duration,
    },
    /// The job is dead: `FG-DB-0006` (logged, alert).
    Dead {
        job_id: JobId,
        report: ForgeError,
    },
    /// The lease was lost before the result was recorded: `FG-DB-0007`
    /// (logged); the result is discarded.
    Fenced(ForgeError),
    /// Stopped before it finished and handed back without counting the
    /// attempt: `FG-DB-0007 reason=released_on_shutdown` (logged).
    Released {
        job_id: JobId,
        report: ForgeError,
    },
    /// Aborted (after its timeout or a stop) but the handler did not end in
    /// time (`FG-DB-0007 reason=left_leased`): the job keeps its lease and
    /// reap queues it after expiry, so the old run cannot overlap a new one.
    /// Or the hand-back did not answer (`release_timeout`): its outcome is
    /// unknown — it may still apply later (fenced, and the handler has
    /// ended) or the lease expires. Logged.
    LeftLeased {
        job_id: JobId,
        report: ForgeError,
    },
    /// Recording the result (complete / fail / release) failed: the job
    /// keeps its lease. `error` is that statement's failure with the job's
    /// context (logged once, with the job's request id).
    Unrecorded {
        job_id: JobId,
        error: ForgeError,
    },
}

/// What [`super::JobRunner::reap_and_report`] logged for one expired lease.
#[derive(Debug)]
pub enum Reported {
    /// `FG-DB-0007 reason=lease_expired` (warn): queued again.
    Requeued(ForgeError),
    /// `FG-DB-0006 reason=lease_expired` (alert): attempts used up.
    Dead(ForgeError),
}

/// The job's context on an error about it.
pub(super) fn with_job_context(e: ForgeError, job: &ClaimedJob) -> ForgeError {
    e.with_context("job_id", job.lease.job_id.get())
        .with_context("kind", job.kind.as_str())
        .with_context("attempt", job.lease.attempt)
        .with_context("origin_request_id", &job.origin_request_id)
}

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
        | Ran::LeftLeased { report: e, .. }
        | Ran::Unrecorded { error: e, .. } => telemetry::log_failure(e, request_id),
    }
}
