//! The `check.spec_drift` job handler (M3 X2, ADR-0023 Amendment X2): one
//! run of [`forge_checks::spec_drift::SpecDrift`] — `forge/spec-drift` on a
//! merge request version's head. The job is enqueued by forge-db's
//! `spec-drift` subscriber (registered in [`super::registry_with`]).
//!
//! Success is logged once by forge-checks (`forge.spec_drift.recorded`); a
//! failure is returned to the job runner, which logs it once. Retry (the
//! queue's backoff): git timeouts / busy / process failures, storage I/O,
//! the database, a repository still being provisioned, a lookup that ran
//! out of time. On the last attempt, or a failure that can never succeed,
//! the head's run is recorded as `error` first
//! ([`forge_checks::spec_drift::SpecDrift::fail`]) so the check never
//! stays without an answer; a malformed payload is dead at once
//! (`FG-DB-0006`, an operator looks: there is no head to answer on).

use std::sync::Arc;

use forge_checks::spec_drift::SpecDrift;
use forge_db::checks::spec_drift::{spec_drift_payload, SPEC_DRIFT_JOB};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};

use super::provision::Storage;

/// The handler of `check.spec_drift`.
pub(crate) struct SpecDriftHandler {
    kind: JobKind,
    drift: Arc<SpecDrift>,
}

impl SpecDriftHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(SPEC_DRIFT_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", SPEC_DRIFT_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        let lookups = Arc::new(db.clone()); // clone: pool handles
        Ok(Self {
            kind,
            drift: Arc::new(SpecDrift::new(
                db.clone(), // clone: pool handles
                browse,
                lookups.clone(), // clone: the same Db is the resolver
                lookups,
            )),
        })
    }
}

/// Whether a failed run may succeed later.
fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e) || e.code() == ErrorCode::RequestTimeout
}

impl JobHandler for SpecDriftHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let drift = Arc::clone(&self.drift);
        Box::pin(async move {
            let request = spec_drift_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let Err(e) = drift.run(&request, &request_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good (dead after this): answer `error` on the head.
            drift.fail(&request, &request_id, &e).await;
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}
