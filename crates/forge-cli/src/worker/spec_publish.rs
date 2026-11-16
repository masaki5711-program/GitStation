//! The `spec.publish` job handler (M4 C12, plan 04 §6 `spec-publish`): one
//! created version-shaped tag, decided with git and, when it publishes a
//! specification version, recorded as `spec.released`
//! ([`forge_pipelines::spec_publish::SpecPublisher::publish`]). The job is
//! enqueued by forge-db's `ci-pipelines` subscriber.
//!
//! Retried like `ci.pipeline_create` (git, storage, the database, a
//! repository still being provisioned, a lookup that ran out of time); a
//! failure that can never pass and a malformed payload are dead at once
//! (doctor `jobs.dead`). A decision not to publish is a success (logged at
//! info with its `reason`).

use std::sync::Arc;

use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::spec_releases::{publish_payload, SPEC_PUBLISH_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_pipelines::spec_publish::SpecPublisher;

use super::provision::Storage;

/// The handler of `spec.publish`.
pub(crate) struct SpecPublishHandler {
    kind: JobKind,
    publisher: Arc<SpecPublisher>,
}

impl SpecPublishHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(SPEC_PUBLISH_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", SPEC_PUBLISH_JOB)
        })?;
        let browse = Browser::new(
            storage.git.clone(),  // clone: the browser owns its handles
            storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        Ok(Self {
            kind,
            publisher: Arc::new(SpecPublisher::new(
                db.clone(), // clone: pool handles
                browse,
                storage.git.clone(),  // clone: the ancestry question
                storage.root.clone(), // clone: as git
            )),
        })
    }
}

fn retryable(e: &ForgeError) -> bool {
    super::review::retryable(e) || e.code() == ErrorCode::RequestTimeout
}

impl JobHandler for SpecPublishHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let publisher = Arc::clone(&self.publisher);
        Box::pin(async move {
            let request = publish_payload(&job.payload).map_err(JobFailure::permanent)?;
            match publisher.publish(&request, &job.request_id()).await {
                Ok(_) => Ok(()),
                Err(e) if retryable(&e) => Err(JobFailure::retry(e)),
                Err(e) => Err(JobFailure::permanent(e)),
            }
        })
    }
}
