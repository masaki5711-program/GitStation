//! The `check.spec_index` job handler (M3 X4, ADR-0023 Amendment X4): one
//! run of [`forge_checks::spec_checks::SpecChecks`] — `forge/spec-lint`,
//! `forge/spec-trace` and `forge/doc-lint` (plan 13 stage 2) on a merge
//! request version's head. The job is
//! enqueued by forge-db's `spec-index` subscriber (registered in
//! [`super::registry_with`]).
//!
//! Success is logged once by forge-checks (`forge.spec_index.recorded`); a
//! failure is returned to the job runner, which logs it once. Retry (the
//! queue's backoff): git timeouts / busy / process failures, storage I/O,
//! the database, a repository still being provisioned, a lookup or the
//! evaluation running out of time. On the last attempt, or a failure that
//! can never succeed, the three runs are recorded as `error` first
//! ([`forge_checks::spec_checks::SpecChecks::fail`]) so no check stays
//! without an answer; a malformed payload is dead at once (`FG-DB-0006`,
//! an operator looks: there is no head to answer on).

use std::sync::Arc;

use forge_checks::spec_checks::SpecChecks;
use forge_db::checks::spec_index::{spec_index_payload, SPEC_INDEX_JOB};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};

use super::provision::Storage;

/// The handler of `check.spec_index`.
pub(crate) struct SpecIndexHandler {
    kind: JobKind,
    checks: Arc<SpecChecks>,
}

impl SpecIndexHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(SPEC_INDEX_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", SPEC_INDEX_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            checks: Arc::new(SpecChecks::new(
                db.clone(), // clone: pool handles
                browse,
                Arc::new(db.clone()), // clone: the same Db loads the project
            )),
        })
    }
}

/// Whether a failed run may succeed later.
fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e) || e.code() == ErrorCode::RequestTimeout
}

impl JobHandler for SpecIndexHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let checks = Arc::clone(&self.checks);
        Box::pin(async move {
            let request = spec_index_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let Err(e) = checks.run(&request, &request_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good (dead after this): answer `error` on the head.
            checks.fail(&request, &request_id, &e).await;
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}
