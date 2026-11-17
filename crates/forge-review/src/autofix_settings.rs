//! A person's autofix command and the reads behind the merge request's
//! line (M4 C14, ADR-0017 Amendment C14): the project's switch
//! (`AutofixSettingsManage`, a Maintainer, people only), decided again
//! under the project's lock (`reconfirm`) and audited in the same
//! transaction (`autofix.settings_changed`). Off by default.

use forge_auth::{Action, Authorized, ProjectCtx};
use forge_core::{MergeRequestId, ProjectId};
use forge_db::autofix::{self as db, AutofixSettings, ChangeOutcome, StoredAttempt};
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, change_context, loaded, require, Review, ReviewRequest};

impl Review {
    /// `GET /api/v1/projects/{id}/autofix` (`ProjectRead`): the switch.
    pub async fn autofix_settings(
        &self,
        project: ProjectId,
    ) -> Result<AutofixSettings, ForgeError> {
        db::settings(&self.db, project).await
    }

    /// `GET …/merge-requests/{iid}/autofix` (`MergeRequestRead`): the
    /// merge request's latest attempt (`None`: none).
    pub async fn autofix_state(
        &self,
        mr: MergeRequestId,
    ) -> Result<Option<StoredAttempt>, ForgeError> {
        db::latest(&self.db, mr).await
    }

    /// `PUT /api/v1/projects/{id}/autofix` (`AutofixSettingsManage`).
    pub async fn set_autofix_enabled(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        enabled: bool,
    ) -> Result<(ChangeOutcome, AutofixSettings), ForgeError> {
        require(proof.action(), Action::AutofixSettingsManage)?;
        let project: ProjectId = *proof.id();
        let user = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_without_user")
                .with_context("action", proof.action().name())
        })?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, settings, ()) =
            db::set_enabled_decided(&self.db, &ctx, project, user, enabled, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            tracing::info!(
                target: "forge_review",
                event = "forge.autofix.settings_changed",
                request_id = %request.request_id,
                project_id = project.get(),
                user_id = user.get(),
                enabled,
            );
        }
        Ok((outcome, settings))
    }
}
