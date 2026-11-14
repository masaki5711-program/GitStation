//! A person's auto-merge commands (M4 C9; plan 04 M-5; ADR-0017 Amendment
//! 2026-11-13): the project's `merge.auto` (`AutoMergeSettingsManage`,
//! the Owner in a session), the project's emergency stop
//! (`AutoMergeStopToggle`, Maintainer) and the instance's
//! (`AutoMergeInstanceStopToggle`, administrators). Each is decided again
//! under the locks (`reconfirm`) and audited in the same transaction; an
//! emergency stop turned on is logged once (`FG-REVIEW-0027`, warn).

use forge_auth::{Action, Authorized, InstanceCtx, ProjectCtx};
use forge_core::auto_merge::{AutoMergeMode, StopScope};
use forge_core::{MergeRequestId, ProjectId};
use forge_db::review::auto_merge::{
    self as db, AutoMergeSettings, ChangeOutcome, StopMark, StoredState,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::{account, change_context, loaded, require, Review, ReviewRequest};

fn caller_of<C: forge_auth::ResourceCtx>(
    proof: &Authorized<C>,
) -> Result<forge_core::UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}

fn log_stop(
    request: &ReviewRequest,
    scope: StopScope,
    project: Option<ProjectId>,
    stopped: bool,
    user: forge_core::UserId,
) {
    if stopped {
        tracing::warn!(
            target: "forge_review",
            event = "forge.auto_merge.emergency_stop",
            error_code = ErrorCode::AutoMergeEmergencyStopped.id(),
            request_id = %request.request_id,
            scope = scope.as_str(),
            project_id = project.map(ProjectId::get),
            user_id = user.get(),
            "auto-merge emergency stop turned on (runbook auto-merge)"
        );
    } else {
        tracing::info!(
            target: "forge_review",
            event = "forge.auto_merge.resumed",
            request_id = %request.request_id,
            scope = scope.as_str(),
            project_id = project.map(ProjectId::get),
            user_id = user.get(),
        );
    }
}

impl Review {
    /// `GET /api/v1/projects/{id}/auto-merge` (`MergeQueueRead`): the
    /// project's mode and both emergency stops.
    pub async fn auto_merge_settings(
        &self,
        project: ProjectId,
    ) -> Result<AutoMergeSettings, ForgeError> {
        db::settings(&self.db, project).await
    }

    /// `GET …/merge-requests/{iid}/auto-merge` (`MergeRequestRead`): what
    /// auto-merge did with merge request `mr` last (`None`: nothing).
    pub async fn auto_merge_state(
        &self,
        mr: MergeRequestId,
    ) -> Result<Option<StoredState>, ForgeError> {
        db::state_of(&self.db, mr).await
    }

    /// `PUT /api/v1/projects/{id}/auto-merge` (`AutoMergeSettingsManage`).
    pub async fn set_auto_merge_mode(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        mode: AutoMergeMode,
    ) -> Result<(ChangeOutcome, AutoMergeSettings), ForgeError> {
        require(proof.action(), Action::AutoMergeSettingsManage)?;
        let project: ProjectId = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, settings, ()) =
            db::set_mode_decided(&self.db, &ctx, project, user, mode, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            tracing::info!(
                target: "forge_review",
                event = "forge.auto_merge.mode_changed",
                request_id = %request.request_id,
                project_id = project.get(),
                user_id = user.get(),
                mode = mode.as_str(),
            );
        }
        Ok((outcome, settings))
    }

    /// `POST /api/v1/projects/{id}/auto-merge/stop` / `…/resume`
    /// (`AutoMergeStopToggle`).
    pub async fn set_auto_merge_stop(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        stopped: bool,
    ) -> Result<(ChangeOutcome, AutoMergeSettings), ForgeError> {
        require(proof.action(), Action::AutoMergeStopToggle)?;
        let project: ProjectId = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, settings, ()) =
            db::set_project_stop_decided(&self.db, &ctx, project, user, stopped, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            log_stop(request, StopScope::Project, Some(project), stopped, user);
        }
        Ok((outcome, settings))
    }

    /// `POST /api/v1/admin/auto-merge/stop` / `…/resume`
    /// (`AutoMergeInstanceStopToggle`).
    pub async fn set_instance_auto_merge_stop(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<InstanceCtx>,
        stopped: bool,
    ) -> Result<(ChangeOutcome, Option<StopMark>), ForgeError> {
        require(proof.action(), Action::AutoMergeInstanceStopToggle)?;
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, mark, ()) =
            db::set_instance_stop_decided(&self.db, &ctx, user, stopped, |facts| {
                proof.reconfirm(account(*facts), InstanceCtx).map(drop)
            })
            .await?;
        if outcome == ChangeOutcome::Changed {
            log_stop(request, StopScope::Instance, None, stopped, user);
        }
        Ok((outcome, mark))
    }
}
