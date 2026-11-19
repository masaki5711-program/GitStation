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
//! The `merge_request.conflict_scan` job handler (M3 X6, plan 04 C-2): one
//! run of [`forge_review::scan`] — the predicted conflicts of a merge
//! request with the other open ones on its target, stored only while its
//! `scan_seq` is the latest. Retried like the mergeability job; on the last
//! attempt, or a failure that can never succeed, the request is ended as
//! `failed reason=job_failed` (`forge_review::conflicts::fail`).
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
use forge_db::review::{
    conflict_scan_payload, mergeability_payload, pipeline_asked, refresh_payload,
    CONFLICT_SCAN_JOB, MERGEABILITY_JOB, REFRESH_JOB,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{ConflictPredictor, Mergeability, VersionRefs};

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
                // Plan 18 S4: the head's changed paths are stored with the
                // result (`review.path_filter` / `review.paths_max`).
                // Plan 18 S5: a target move's merge result gets the merge
                // request pipeline only with `ci.mr_pipeline_on_target_move`.
                checker: Mergeability::new(storage.git.clone(), storage.root.clone())
                    .with_paths(storage.merge_paths_max)
                    .with_pipeline_on_target_move(storage.mr_pipeline_on_target_move),
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
            // Plan 18 S5: a person asked for the pipeline on the current target.
            let asked = pipeline_asked(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let ran =
                forge_review::check_asked(&inner.db, &inner.checker, request, asked, &request_id)
                    .await;
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
            if asked {
                // Plan 18 S5 review: the person's ask goes with the job;
                // say so (the page still offers the button: asking again
                // starts it).
                tracing::warn!(
                    target: "forge_cli",
                    event = "forge.review.mergeability_ask_lost",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    merge_request_id = request.merge_request.get(),
                    check_seq = request.check_seq,
                    "a mergeability job that carried a request for the merge request pipeline failed for good; the request is lost"
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

/// The handler of `merge_request.conflict_scan` (M3 X6).
pub(crate) struct ConflictScanHandler {
    kind: JobKind,
    inner: Arc<ScanInner>,
}

struct ScanInner {
    db: Db,
    predictor: ConflictPredictor,
}

impl ConflictScanHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(CONFLICT_SCAN_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", CONFLICT_SCAN_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(ScanInner {
                db: db.clone(), // clone: pool handles
                // clone: the handler owns its git and storage handles
                predictor: ConflictPredictor::new(storage.git.clone(), storage.root.clone()),
            }),
        })
    }
}

impl JobHandler for ConflictScanHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let request = conflict_scan_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let ran = forge_review::scan(&inner.db, &inner.predictor, request, &request_id).await;
            let Err(e) = ran else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good: never leave the request unfinished. A
            // timeout or crash never gets here; the sweep ends those rows
            // (`job_stale`).
            forge_review::conflicts::fail(&inner.db, request, &request_id, &e).await;
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
