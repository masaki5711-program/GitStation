//! A project's `ai_assets.enabled` (plan 17 §0.4, P17-3, ADR-0035): read by
//! anyone who reads the project, turned on or off by a person who is
//! Maintainer or above in a web session (`AiAssetsSettingManage`), decided
//! again under the project's lock (`reconfirm`) and audited in the same
//! transaction.

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::ProjectId;
use forge_db::ai_assets::{self as db, AiAssetsSetting};
use forge_db::review::auto_merge::ChangeOutcome;
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, change_context, loaded, require, Review, ReviewRequest};

impl Review {
    /// `GET /api/v1/projects/{id}/ai-assets` (`ProjectRead`).
    pub async fn ai_assets_setting(
        &self,
        project: ProjectId,
    ) -> Result<AiAssetsSetting, ForgeError> {
        db::setting(&self.db, project).await
    }

    /// `PUT /api/v1/projects/{id}/ai-assets` (`AiAssetsSettingManage`).
    pub async fn set_ai_assets_enabled(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        enabled: bool,
    ) -> Result<(ChangeOutcome, AiAssetsSetting), ForgeError> {
        require(proof.action(), Action::AiAssetsSettingManage)?;
        let project: ProjectId = *proof.id();
        let user = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_without_user")
                .with_context("action", proof.action().name())
        })?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, setting, ()) =
            db::set_enabled_decided(&self.db, &ctx, project, user, enabled, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            tracing::info!(
                target: "forge_review",
                event = "forge.ai_assets.changed",
                request_id = %request.request_id,
                project_id = project.get(),
                user_id = user.get(),
                enabled,
            );
        }
        Ok((outcome, setting))
    }
}
