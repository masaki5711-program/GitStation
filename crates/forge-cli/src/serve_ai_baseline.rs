//! The nightly AI evaluation baseline candidate in `forge serve` (plan 17
//! P17-6): the job `ai_eval.baseline_candidate` (queued by the worker's
//! `ci_ai_eval` task with a scheduled run's passing `forge/ai-eval` check)
//! runs on the system drafts' runner (`serve_reverts`), next to the browse
//! cache the candidate branch's write invalidates. `forge worker` does not
//! register the kind.
//!
//! The handler reads the candidate again (`forge_pipelines::ai_eval::
//! baseline_candidate`: the report artifact, the baseline at the merge
//! base with the default branch) and hands it to
//! `forge_review::BaselineProposer` (one draft merge request per models).
//! Nothing to propose completes the job (logged as
//! `forge.ai_eval.baseline_skipped`); a failure that may pass later
//! ([`retryable`]) is retried with the queue's backoff; any other dies with
//! its code (the job runner logs it once).

use std::sync::Arc;

use forge_db::ci_ai_eval::{baseline_candidate_payload, BASELINE_CANDIDATE_JOB};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_pipelines::ai_eval::{baseline_candidate, Candidate};
use forge_pipelines::artifacts::ArtifactStore;
use forge_review::{BaselineAsk, BaselineProposer};

use crate::serve_merges::retryable;

/// The handler of `ai_eval.baseline_candidate`.
pub(crate) struct BaselineCandidateHandler {
    kind: JobKind,
    db: Db,
    store: Arc<ArtifactStore>,
    browser: Browser,
    proposer: Arc<BaselineProposer>,
}

impl BaselineCandidateHandler {
    pub(crate) fn new(
        db: Db,
        store: ArtifactStore,
        browser: Browser,
        proposer: BaselineProposer,
    ) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(BASELINE_CANDIDATE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", BASELINE_CANDIDATE_JOB)
        })?;
        Ok(Self {
            kind,
            db,
            store: Arc::new(store),
            browser,
            proposer: Arc::new(proposer),
        })
    }
}

impl JobHandler for BaselineCandidateHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let db = self.db.clone(); // clone: the pool handle is shared
        let store = Arc::clone(&self.store);
        let browser = self.browser.clone(); // clone: shares the ref cache
        let proposer = Arc::clone(&self.proposer);
        Box::pin(async move {
            let (project, pipeline) =
                baseline_candidate_payload(&job.payload).map_err(JobFailure::permanent)?;
            let rid = job.request_id();
            let found = baseline_candidate(&db, &store, &browser, pipeline).await;
            let proposal = match found {
                Ok(Candidate::Propose(p)) => p,
                Ok(Candidate::Skip(reason)) => {
                    tracing::info!(
                        event = "forge.ai_eval.baseline_skipped",
                        request_id = %rid,
                        project_id = project.get(),
                        pipeline_id = pipeline,
                        reason,
                        "the nightly run proposes no baseline"
                    );
                    return Ok(());
                }
                Err(e) if retryable(&e) => return Err(JobFailure::retry(e)),
                Err(e) => return Err(JobFailure::permanent(e)),
            };
            let ask = BaselineAsk {
                project: proposal.project,
                repo: proposal.repo,
                pipeline_id: proposal.pipeline_id,
                owner: proposal.owner,
                target: proposal.target,
                start: proposal.start,
                models: proposal.models,
                content: proposal.content,
            };
            match proposer.propose(&ask, &rid).await {
                Ok(_report) => Ok(()),
                Err(e) if retryable(&e) => Err(JobFailure::retry(e)),
                Err(e) => Err(JobFailure::permanent(e)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review HIGH: the job's bound is above the sum of the proposal's git
    /// steps, so it is not cancelled between the branch's move and the
    /// merge request (an interrupted one is resumed anyway).
    #[test]
    fn the_job_outlives_every_git_step_of_a_proposal() {
        let steps = forge_review::ai_baseline::BASELINE_GIT_TIMEOUT * 14;
        assert!(
            forge_db::ci_ai_eval::BASELINE_CANDIDATE_TIMEOUT
                > steps + std::time::Duration::from_secs(60)
        );
    }

    /// A database or git failure that may pass is retried; a refusal of
    /// the payload is not.
    #[test]
    fn failures_that_may_pass_are_retried() {
        assert!(retryable(&ForgeError::new(ErrorCode::DbUnavailable)));
        assert!(!retryable(&ForgeError::new(ErrorCode::Internal)));
        assert!(baseline_candidate_payload(&serde_json::json!({ "pipeline_id": 1 })).is_err());
    }
}
