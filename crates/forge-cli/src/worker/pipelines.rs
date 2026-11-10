//! The `ci.pipeline_create` job handler (M4 C2, plan 14): one run of
//! [`forge_pipelines::Pipelines::plan_and_record`] — read `.forge-ci.yml`
//! at the pushed commit, lint and plan it with the project's protected
//! branches, write the pipeline and its jobs. The job is enqueued by
//! forge-db's `ci-pipelines` subscriber (registered in
//! [`super::registry_with`]).
//!
//! Success is logged once by forge-pipelines (`forge.ci.pipeline_created`
//! or `forge.ci.no_definition`); a failure is returned to the job runner,
//! which logs it once. Retry (the queue's backoff): git timeouts / busy /
//! process failures, storage I/O, the database, a repository still being
//! provisioned, a lookup that ran out of time. On the last attempt, or a
//! failure that can never succeed, the pipeline is recorded as `error`
//! first ([`forge_pipelines::Pipelines::fail`], FG-CI-0002) so a push never
//! stays without an answer; a malformed payload is dead at once
//! (`FG-DB-0006`: there is no pipeline to answer on).

use std::sync::Arc;

use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_pipelines::{create_payload, Pipelines, PIPELINE_CREATE_JOB};

use super::provision::Storage;
use crate::ci_marks::ConfidentialMeter;

/// The handler of `ci.pipeline_create`.
pub(crate) struct PipelineHandler {
    kind: JobKind,
    pipelines: Arc<Pipelines>,
}

impl PipelineHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(PIPELINE_CREATE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", PIPELINE_CREATE_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            pipelines: Arc::new(Pipelines::new(
                db.clone(),     // clone: pool handles
                browse.clone(), // clone: the meter reads with the same browser
                Arc::new(ConfidentialMeter::new(browse)),
            )),
        })
    }
}

/// Whether a failed run may succeed later.
fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e) || e.code() == ErrorCode::RequestTimeout
}

impl JobHandler for PipelineHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let pipelines = Arc::clone(&self.pipelines);
        Box::pin(async move {
            let request = create_payload(&job.payload).map_err(JobFailure::permanent)?;
            let request_id = job.request_id();
            let Err(e) = pipelines.plan_and_record(&request, &request_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good (dead after this): answer with an `error` row.
            pipelines.fail(&request, &request_id, &e).await;
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}
