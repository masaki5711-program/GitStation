//! The agent configuration's update merge requests in `forge serve` (plan
//! 16 A9, ADR-0032 Amendment A9): the job `agent.update_merge_request`
//! (queued by a person's `POST …/agent-builder/update`, or by a part
//! version's revocation in its own transaction for every project whose
//! lock uses it) runs on the system drafts' runner (`serve_reverts`), next
//! to the browse cache the update branch's write invalidates. `forge
//! worker` does not register the kind.
//!
//! The merge request is opened by forge-review's `Review::open`
//! ([`ReviewOpener`], the marketplace's `UpdateOpener`): only forge-review
//! opens merge requests, and the two feature crates meet only here.
//!
//! The handler hands the payload to `forge_marketplace::AgentUpdater`
//! (logged there as `forge.agent.update_opened` /
//! `forge.agent.update_skipped`): an update opened, already open or with
//! nothing to do completes the job; a failure that may pass later
//! ([`retryable`]) is retried with the queue's backoff; any other dies
//! with its code — `FG-AGENT-0016` when nobody may have it opened — and
//! the job runner logs it once.

use std::sync::Arc;

use forge_auth::{Authorized, MergeRequestCtx};
use forge_core::{ProjectRepository, RequestId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_errors::{ErrorCode, ForgeError};
use forge_marketplace::updates::{OpenFuture, UpdateOpener};
use forge_marketplace::{agent_update_payload, AgentUpdater, AGENT_UPDATE_JOB};
use forge_review::{NewFields, Review, ReviewRequest};

/// forge-review's `Review::open` as the marketplace's opener (the proof
/// is the person's `MergeRequestCreate`, decided again in the open's
/// transaction; audited as that person, with the job's request id).
pub(crate) struct ReviewOpener(pub(crate) Review);

impl UpdateOpener for ReviewOpener {
    fn open<'a>(
        &'a self,
        request_id: &'a RequestId,
        proof: &'a Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        title: String,
        description: String,
    ) -> OpenFuture<'a> {
        Box::pin(async move {
            let opened = self
                .0
                .open(
                    &ReviewRequest {
                        request_id: request_id.clone(), // clone: the request owns its id
                        credential_id: None,
                    },
                    proof,
                    repo,
                    &NewFields { title, description },
                )
                .await?;
            Ok((opened.id, opened.iid))
        })
    }
}

use crate::serve_merges::retryable;

/// The handler of `agent.update_merge_request`.
pub(crate) struct AgentUpdateHandler {
    kind: JobKind,
    updater: Arc<AgentUpdater>,
}

impl AgentUpdateHandler {
    pub(crate) fn new(updater: AgentUpdater) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(AGENT_UPDATE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", AGENT_UPDATE_JOB)
        })?;
        Ok(Self {
            kind,
            updater: Arc::new(updater),
        })
    }
}

impl JobHandler for AgentUpdateHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let updater = Arc::clone(&self.updater);
        Box::pin(async move {
            let asked = agent_update_payload(&job.payload).map_err(JobFailure::permanent)?;
            let rid = job.request_id();
            match updater.run(&asked, &rid).await {
                Ok(_report) => Ok(()),
                Err(e) if retryable(&e) => Err(JobFailure::retry(e)),
                Err(e) => Err(JobFailure::permanent(e)),
            }
        })
    }
}

#[cfg(test)]
#[path = "serve_agent_updates_tests.rs"]
mod git_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The job's bound is above every bounded step of one update: the plan
    /// and the commit (60 s each), the reads and the records.
    #[test]
    fn the_job_outlives_the_update_steps() {
        assert!(
            forge_db::agent_builder::AGENT_UPDATE_TIMEOUT
                > forge_marketplace::PUBLISH_TIMEOUT * 2
                    + forge_git::file_commit::COMMIT_DEADLINE
                    + forge_marketplace::updates::UPDATE_GIT_TIMEOUT * 4
        );
    }

    /// A refusal of the update is never retried (the job dies with its
    /// code, logged once); a database outage is.
    #[test]
    fn refusals_die_and_outages_retry() {
        assert!(!retryable(&ForgeError::new(
            ErrorCode::AgentUpdateNotOpened
        )));
        assert!(!retryable(&ForgeError::new(
            ErrorCode::AgentUpdateNotNeeded
        )));
        assert!(retryable(&ForgeError::new(ErrorCode::DbUnavailable)));
        assert!(agent_update_payload(&serde_json::json!({ "project_id": 1 })).is_err());
    }
}
