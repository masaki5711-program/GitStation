//! The `work_items.relink` job handler (ADR-0016 Amendment 2026-11-12):
//! after a project got a binding that resolves work items, its open and
//! recently merged merge requests are detected again
//! ([`forge_db::work_items::request_relink`]: one
//! `merge_request.relink_requested` per merge request, handled by the
//! `work-item-sync` subscriber). The job is enqueued by the binding
//! commands in their own transaction.
//!
//! Success is logged once by forge-db (`forge.work_items.relink_requested`);
//! a failure is returned to the job runner, which logs it once. Database
//! failures retry with the queue's backoff (each batch is idempotent); a
//! malformed payload is dead at once (`FG-DB-0006`).

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::work_items::{relink_payload, request_relink, RELINK_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

/// The handler of `work_items.relink`.
pub(crate) struct RelinkHandler {
    kind: JobKind,
    db: Db,
}

impl RelinkHandler {
    pub(crate) fn new(db: &Db) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(RELINK_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", RELINK_JOB)
        })?;
        Ok(Self {
            kind,
            db: db.clone(), // clone: pool handles
        })
    }
}

fn retryable(e: &ForgeError) -> bool {
    matches!(
        e.code(),
        ErrorCode::DbUnavailable | ErrorCode::DbQueryFailed | ErrorCode::RequestTimeout
    )
}

impl JobHandler for RelinkHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let db = self.db.clone(); // clone: pool handles for the job's future
        Box::pin(async move {
            let request = relink_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id: RequestId = job.request_id();
            let ctx = ChangeContext::new(Actor::System, request_id);
            match request_relink(&db, &ctx, &request).await {
                Ok(_) => Ok(()),
                Err(e) if retryable(&e) => Err(JobFailure::retry(e)),
                Err(e) => Err(JobFailure::permanent(e)),
            }
        })
    }
}
