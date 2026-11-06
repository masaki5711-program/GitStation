//! The `check.risk` job handler (M3 X7, ADR-0023 Amendment X7): one run of
//! [`forge_checks::risk::Risk`] — `forge/risk` on a merge request version's
//! head. The job is enqueued by forge-db's `risk` subscriber (registered in
//! [`super::registry_with`]).
//!
//! Success is logged once by forge-checks (`forge.risk.recorded`); a
//! failure is returned to the job runner, which logs it once. Retry (the
//! queue's backoff): git timeouts / busy / process failures, storage I/O,
//! the database, a repository still being provisioned, a lookup that ran
//! out of time. On the last attempt, or a failure that can never succeed,
//! the head's run is recorded as `error` first
//! ([`forge_checks::risk::Risk::fail`]) so the check never stays without an
//! answer; a malformed payload is dead at once (`FG-DB-0006`, an operator
//! looks: there is no head to answer on).

use std::sync::Arc;

use forge_checks::risk::Risk;
use forge_db::checks::risk::{risk_payload, RISK_JOB};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};

use super::provision::Storage;

/// The handler of `check.risk`.
pub(crate) struct RiskHandler {
    kind: JobKind,
    risk: Arc<Risk>,
}

impl RiskHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(RISK_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", RISK_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            risk: Arc::new(Risk::new(
                db.clone(),           // clone: pool handles
                storage.git.clone(),  // clone: the evaluation owns its handles
                storage.root.clone(), // clone: as git
                browse,
                Arc::new(db.clone()), // clone: the same Db loads projects
            )),
        })
    }
}

/// Whether a failed run may succeed later.
fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e) || e.code() == ErrorCode::RequestTimeout
}

impl JobHandler for RiskHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let risk = Arc::clone(&self.risk);
        Box::pin(async move {
            let request = risk_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let Err(e) = risk.run(&request, &request_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good (dead after this): answer `error` on the head.
            risk.fail(&request, &request_id, &e).await;
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}
