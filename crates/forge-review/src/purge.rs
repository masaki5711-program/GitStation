//! The administrator's purge of version refs (M3 R1b, ADR-0024 §2;
//! runbook `secret-removal`), split from `lib.rs`.

use forge_auth::{Action, Authorized, MergeRequestCtx};
use forge_core::ProjectRepository;
use forge_db::review::{self, PurgeOutcome};
use forge_errors::{ErrorCode, ForgeError};

use crate::{
    account, caller, change_context, loaded, locked, ready, require, shape_mismatch, Review,
    ReviewRequest,
};

impl Review {
    /// `POST …/merge-requests/{iid}/version-refs/purge`
    /// (`MergeRequestVersionRefsPurge`, ADR-0024 §2: a human administrator's
    /// session only): mark `versions` (`None` = every version) as purged,
    /// decided again inside the transaction (`FG-REVIEW-0002` when the
    /// merge request is open and its head is among them), then delete
    /// their refs (and `head` when it named one). The marks commit first:
    /// a failed ref deletion (`FG-GIT-*`) is retried by asking again (the
    /// already purged versions' refs are deleted on every purge).
    pub async fn purge_version_refs(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        versions: Option<&[i64]>,
    ) -> Result<PurgeOutcome, ForgeError> {
        require(proof.action(), Action::MergeRequestVersionRefsPurge)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_version_refs_purge"));
        };
        let refs = self.refs.as_ref().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "version_refs_unconfigured")
        })?;
        let repo_id = ready(project, repo)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, ()) = review::purge_mr_version_refs_decided(
            &self.db,
            &ctx,
            project,
            iid,
            user,
            versions,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        refs.sync(
            &self.db,
            repo_id,
            outcome.merge_request.id,
            &request.request_id,
        )
        .await?;
        tracing::info!(
            target: "forge_review",
            event = "forge.review.version_refs_purged",
            request_id = %request.request_id,
            project_id = project.get(),
            iid,
            merge_request_id = outcome.merge_request.id.get(),
            purged = ?outcome.purged,
            already_purged = ?outcome.already_purged,
            "merge request version refs purged"
        );
        Ok(outcome)
    }
}
