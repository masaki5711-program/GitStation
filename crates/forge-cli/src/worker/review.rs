//! The `merge_request.refresh` job handler (M3 R1b, ADR-0024 §2, plan 11
//! §11.3): one run of [`forge_review::refresh`] — read the source branch,
//! record the version (row first), write its hidden refs (M3 R2: then the
//! version's bases and the mergeability request). Every step is
//! idempotent, so the job queue's at-least-once delivery is safe.
//!
//! The `merge_request.mergeability` job handler (M3 R2, ADR-0024 §3): one
//! run of [`forge_review::check`] — the trial merge of the recorded head
//! into the target's tip, stored only while its `check_seq` is the latest.
//! A git failure is stored as `check_failed` by forge-review and retried
//! here like the version job's; on the last attempt, or a failure that can
//! never succeed, the request is ended as `check_failed reason=job_failed`
//! (`forge_review::mergeability::fail`) before the job dies.
//!
//! Success is logged once by forge-review (`forge.review.version_refreshed`
//! with the job's request id); a failure is returned to the job runner,
//! which logs it once. Retry: git timeouts / busy / process failures (a
//! lost compare-and-swap after the sync's own second try), storage I/O,
//! the database unreachable or a query failed, and a repository still
//! being provisioned are retried with the queue's backoff; anything else
//! (a malformed payload, a broken invariant) is dead at once
//! (`FG-DB-0006`, an operator looks).

use std::sync::Arc;

use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::review::{mergeability_payload, refresh_payload, MERGEABILITY_JOB, REFRESH_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{Mergeability, VersionRefs};

use super::provision::Storage;

/// The handler of `merge_request.refresh`.
pub(crate) struct RefreshHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    refs: VersionRefs,
}

impl RefreshHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(REFRESH_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", REFRESH_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: pool handles
                // clone: the handler owns its git and storage handles
                refs: VersionRefs::new(storage.git.clone(), storage.root.clone()),
            }),
        })
    }
}

/// Whether a failed run may succeed later.
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
    )
}

impl JobHandler for RefreshHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let request = refresh_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            forge_review::refresh(&inner.db, &inner.refs, request, &request_id)
                .await
                .map(drop)
                .map_err(|e| {
                    if retryable(&e) {
                        JobFailure::retry(e)
                    } else {
                        JobFailure::permanent(e)
                    }
                })
        })
    }
}

/// The handler of `merge_request.mergeability` (M3 R2).
pub(crate) struct MergeabilityHandler {
    kind: JobKind,
    inner: Arc<CheckInner>,
}

struct CheckInner {
    db: Db,
    checker: Mergeability,
}

impl MergeabilityHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(MERGEABILITY_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", MERGEABILITY_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(CheckInner {
                db: db.clone(), // clone: pool handles
                // clone: the handler owns its git and storage handles
                checker: Mergeability::new(storage.git.clone(), storage.root.clone()),
            }),
        })
    }
}

impl JobHandler for MergeabilityHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let request = mergeability_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let ran = forge_review::check(&inner.db, &inner.checker, request, &request_id).await;
            let Err(e) = ran else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good (dead after this): never leave the request
            // `checking` (review HIGH). A timeout or crash never gets here;
            // the sweep ends those rows (`job_stale`).
            forge_review::mergeability::fail(&inner.db, request, &request_id, &e).await;
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mergeability_tests.rs"]
mod mergeability_tests;
