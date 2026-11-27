//! A person's commands on the local LLM (plan 19 §8.2, ADR-0039 Amendment
//! `feat/core-llm-settings-and-rerun`):
//!
//! - a project's `llm.review` (`LlmSettingsManage`: a Maintainer, a person,
//!   a web session), decided again on the locked project (`reconfirm`);
//! - the gateway's pause and resume (`LlmPause` / `LlmResume`: an
//!   administrator, a person), decided again on the locked account;
//! - asking a merge request's first pass again (`AiReviewRerun`: Developer
//!   and above, a person — never an agent's token, act-as, or a request
//!   that declared `Forge-Context-Audience: agent`), at most 3 an hour per
//!   merge request.
//!
//! Each change is audited in its transaction and logged once here.

use forge_auth::{Action, Authorized, InstanceCtx, MergeRequestCtx, ProjectCtx};
use forge_core::llm::ReviewMode;
use forge_core::{ProjectId, UserId};
use forge_db::llm::ops::{self as db, GatewayPause, Rerun, ReviewSetting};
use forge_db::review::auto_merge::ChangeOutcome;
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, change_context, loaded, require, Review, ReviewRequest};

fn caller_of<C: forge_auth::ResourceCtx>(proof: &Authorized<C>) -> Result<UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}

impl Review {
    /// `GET /api/v1/projects/{id}/llm` (`ProjectRead`).
    pub async fn llm_review_setting(
        &self,
        project: ProjectId,
    ) -> Result<ReviewSetting, ForgeError> {
        db::review_setting(&self.db, project).await
    }

    /// `PUT /api/v1/projects/{id}/llm` (`LlmSettingsManage`).
    pub async fn set_llm_review(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        mode: ReviewMode,
    ) -> Result<(ChangeOutcome, ReviewSetting), ForgeError> {
        require(proof.action(), Action::LlmSettingsManage)?;
        let project: ProjectId = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, setting, ()) =
            db::set_review_mode_decided(&self.db, &ctx, project, user, mode, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            tracing::info!(
                target: "forge_review",
                event = "forge.llm.review_mode_changed",
                request_id = %request.request_id,
                project_id = project.get(),
                user_id = user.get(),
                mode = mode.as_str(),
            );
        }
        Ok((outcome, setting))
    }

    /// The gateway's pause as stored (for the pause routes' answer).
    pub async fn llm_gateway_pause(&self) -> Result<GatewayPause, ForgeError> {
        db::gateway_pause(&self.db).await
    }

    /// `POST /api/v1/admin/llm/pause` (`LlmPause`, `paused`) or `…/resume`
    /// (`LlmResume`). The worker follows the stored state within 10 s.
    pub async fn set_llm_paused(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<InstanceCtx>,
        paused: bool,
    ) -> Result<(ChangeOutcome, GatewayPause), ForgeError> {
        require(
            proof.action(),
            if paused {
                Action::LlmPause
            } else {
                Action::LlmResume
            },
        )?;
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, state, ()) = db::set_paused_decided(&self.db, &ctx, user, paused, |facts| {
            proof.reconfirm(account(*facts), InstanceCtx).map(drop)
        })
        .await?;
        if outcome == ChangeOutcome::Changed {
            tracing::warn!(
                target: "forge_review",
                event = if paused { "forge.llm.gateway_paused" } else { "forge.llm.gateway_resumed" },
                request_id = %request.request_id,
                user_id = user.get(),
                "the LLM gateway pause was changed by an administrator; the worker follows within 10 s"
            );
        }
        Ok((outcome, state))
    }

    /// `POST …/merge-requests/{iid}/ai-review/rerun` (`AiReviewRerun`,
    /// module docs). `declared_agent`: `Forge-Context-Audience: agent`
    /// (403 `reason=agent_audience`: an agent never spends the GPU on a
    /// person's behalf).
    pub async fn ai_review_rerun(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        declared_agent: bool,
    ) -> Result<Rerun, ForgeError> {
        require(proof.action(), Action::AiReviewRerun)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_shape_mismatch")
            );
        };
        if declared_agent {
            return Err(
                ForgeError::new(ErrorCode::Forbidden).with_context("reason", "agent_audience")
            );
        }
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (rerun, ()) = db::request_rerun(&self.db, &ctx, project, iid, user, |facts| {
            let fresh = MergeRequestCtx::new(loaded(&facts.project), crate::locked(facts)?);
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        if let Rerun::Queued {
            head_sha,
            remaining,
        } = &rerun
        {
            tracing::info!(
                target: "forge_review",
                event = "forge.llm.review_rerun_requested",
                request_id = %request.request_id,
                project_id = project.get(),
                iid,
                head_sha = %head_sha,
                remaining,
                user_id = user.get(),
            );
        }
        Ok(rerun)
    }
}
