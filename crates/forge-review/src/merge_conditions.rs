//! The merge conditions' commands of [`Review`] (M3 R6, ADR-0024 §4.1,
//! §7, §8, §9, §12): the project's merge settings (read, write), the
//! time-boxed CODEOWNERS switch, and the merge status — `evaluate_merge()`
//! on the facts as they are now, every blocking reason (the stable JSON of
//! `gstn mr status --json`). Each write is decided twice by `authorize()`:
//! the route's proof, then `reconfirm` on the facts locked in the
//! transaction.

use forge_auth::{
    Action, Authorized, CodeownersToggle, CodeownersToggleCtx, MergeRequestCtx, ProjectCtx,
};
use forge_core::{MergeMethod, MergeSettingsValues, ObjectId, ProjectRepository, UserId};
use forge_db::review::{
    self, CodeownersChange, SettingsOutcome, StoredMergeSettings, StoredMergeability,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::approvals::{evaluate, ApprovalReport, STATE_BUDGET};
use crate::merges::{
    conditions, evaluate_merge, stored_mergeable, BlockReason, MergeFacts, NoCheckRuns,
    RequiredCheck,
};
use crate::{account, change_context, loaded, require, shape_mismatch, Review, ReviewRequest};

fn param_invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

fn caller_of<R: forge_auth::ResourceCtx>(proof: &Authorized<R>) -> Result<UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}

/// The merge status of a merge request now (`GET …/{iid}/status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeStatus {
    /// `None`: no version yet (nothing can merge; `reasons` says why).
    pub head_sha: Option<ObjectId>,
    pub target_branch: forge_core::RefName,
    /// The method evaluated (asked, or the project's default).
    pub method: MergeMethod,
    /// `evaluate_merge()` found nothing blocking. A merge decides again
    /// under the merge lock (the job's own trial merge, the live tips).
    pub mergeable: bool,
    /// Every blocking reason, in ADR-0024 §7's order.
    pub reasons: Vec<BlockReason>,
    /// The approval evaluation (`None` without a version).
    pub approvals: Option<ApprovalReport>,
    /// Every required check of the target's merge rule with its run.
    pub checks: Vec<RequiredCheck>,
    /// The unresolved discussions when the merge rule asks for them.
    pub discussions: Option<u32>,
    /// The project's settings as read.
    pub settings: StoredMergeSettings,
}

impl Review {
    /// `GET …/merge-settings` (`ProjectRead`): the project's settings
    /// (the defaults without a row).
    pub async fn merge_settings(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<StoredMergeSettings, ForgeError> {
        require(proof.action(), Action::ProjectRead)?;
        review::merge_settings(&self.db, *proof.id()).await
    }

    /// `PUT …/merge-settings` (`MergeSettingsManage`): at least one method,
    /// the default among them (`FG-WEB-0006`); the CODEOWNERS switch is
    /// not touched (it has its own action).
    pub async fn put_merge_settings(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        values: &MergeSettingsValues,
    ) -> Result<(SettingsOutcome, StoredMergeSettings), ForgeError> {
        require(proof.action(), Action::MergeSettingsManage)?;
        if values.allowed_methods.is_empty() {
            return Err(param_invalid("allowed_methods", "empty"));
        }
        if !values.allowed_methods.contains(&values.default_method) {
            return Err(param_invalid("default_method", "not_allowed"));
        }
        let project = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, stored, ()) =
            review::put_merge_settings_mr_decided(&self.db, &ctx, project, user, values, |facts| {
                proof
                    .reconfirm(
                        account(facts.account),
                        ProjectCtx::new(loaded(&facts.project)),
                    )
                    .map(drop)
            })
            .await?;
        Ok((outcome, stored))
    }

    /// `PUT …/merge-settings/codeowners-enforcement`
    /// (`CodeownersEnforcementToggle`): the change the proof was decided
    /// for, decided again on the locked facts (the caller's role and the
    /// project's highest protection now). One info line
    /// `forge.review.codeowners_enforcement_changed` besides the audit row
    /// and the event (runbook codeowners).
    pub async fn toggle_codeowners(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<CodeownersToggleCtx>,
    ) -> Result<(SettingsOutcome, StoredMergeSettings), ForgeError> {
        require(proof.action(), Action::CodeownersEnforcementToggle)?;
        let asked = proof.ctx().change();
        let change = match asked {
            CodeownersToggle::Enforce => CodeownersChange::Enforce,
            CodeownersToggle::Suspend {
                minutes: Some(minutes),
            } => CodeownersChange::Suspend { minutes },
            // authorize() refuses a suspension without an expiry.
            CodeownersToggle::Suspend { minutes: None } => {
                return Err(shape_mismatch("codeowners_toggle"))
            }
        };
        let project = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, stored, ()) =
            review::toggle_codeowners_mr_decided(&self.db, &ctx, project, user, change, |facts| {
                let fresh = CodeownersToggleCtx::new(
                    loaded(&facts.project),
                    facts.highest_protection,
                    asked,
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        if outcome == SettingsOutcome::Changed {
            let until = match &stored.codeowners {
                review::CodeownersEnforcement::Suspended { until, .. } => Some(until.as_str()),
                review::CodeownersEnforcement::Enforced
                | review::CodeownersEnforcement::Expired { .. } => None,
            };
            tracing::info!(
                target: "forge_review",
                event = "forge.review.codeowners_enforcement_changed",
                request_id = %request.request_id,
                project_id = project.get(),
                user_id = user.get(),
                enforced = stored.codeowners.enforced(),
                until,
                "CODEOWNERS enforcement changed (runbook codeowners)"
            );
        }
        Ok((outcome, stored))
    }

    /// `GET …/merge-requests/{iid}/status` (`MergeRequestRead`): what
    /// `evaluate_merge()` decides now for `method` (the project's default
    /// when `None`; a method the project does not allow is
    /// `FG-REVIEW-0010`). Reads only: no check is requested, nothing is
    /// locked; a merge decides again in its own gates.
    pub async fn merge_status(
        &self,
        request_id: &forge_core::RequestId,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        method: Option<MergeMethod>,
    ) -> Result<MergeStatus, ForgeError> {
        require(proof.action(), Action::MergeRequestRead)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_status"));
        };
        let user = crate::caller(proof)?;
        let current = self
            .merge_request(project, iid)
            .await?
            .ok_or_else(crate::absent)?;
        let settings = review::merge_settings(&self.db, project).await?;
        let method = method.unwrap_or(settings.values.default_method);
        crate::merges::method_allowed(&settings, method)?;
        let head = current
            .head_sha
            .as_deref()
            .map(ObjectId::parse)
            .transpose()?;
        let Some(head) = head else {
            // No version yet: nothing is decided for any head.
            let mut reasons = Vec::new();
            if current.state != forge_core::MergeRequestState::Open {
                reasons.push(BlockReason::NotOpen);
            }
            if current.draft {
                reasons.push(BlockReason::Draft);
            }
            reasons.push(BlockReason::MergeabilityUnknown);
            return Ok(MergeStatus {
                head_sha: None,
                target_branch: current.target_branch,
                method,
                mergeable: false,
                reasons,
                approvals: None,
                checks: Vec::new(),
                discussions: None,
                settings,
            });
        };
        let source_tip = self
            .branch_tip(project, repo, user, &current.source_branch)
            .await?;
        let target_tip = self
            .branch_tip(project, repo, user, &current.target_branch)
            .await?;
        let git = self
            .git_facts(request_id, repo, user, &current, &head, STATE_BUDGET)
            .await?;
        let facts = review::approval_facts(&self.db, project, current.id).await?;
        let unresolved = review::unresolved_discussions(&self.db, current.id).await?;
        let stored: Option<StoredMergeability> = review::mergeability(&self.db, current.id).await?;
        let live = review::merges::latest_merge(&self.db, current.id)
            .await?
            .is_some_and(|m| m.state.is_live());
        let conditions = conditions(
            &facts.protection,
            &current.target_branch,
            &head,
            unresolved,
            &NoCheckRuns,
        );
        let inputs = git.inputs(&current, &head, facts);
        let report = evaluate(&inputs);
        let mergeable = stored_mergeable(stored.as_ref(), &head, target_tip.as_ref());
        let decision = evaluate_merge(&MergeFacts {
            state: current.state,
            draft: current.draft,
            merge_in_progress: live,
            source_present: source_tip.is_some(),
            mergeable: Some(mergeable),
            method,
            // A fast-forward is decided by the job under the lock.
            ff_possible: None,
            approvals: inputs,
            checks: conditions.checks.clone(), // clone: the status shows them too
            discussions: conditions.discussions,
        });
        let (ok, reasons) = match decision {
            Ok(_) => (true, Vec::new()),
            Err(reasons) => (false, reasons),
        };
        Ok(MergeStatus {
            head_sha: Some(head),
            target_branch: current.target_branch,
            method,
            mergeable: ok,
            reasons,
            approvals: Some(report),
            checks: conditions.checks,
            discussions: conditions.discussions,
            settings,
        })
    }

    /// The tip of `branch` (`None`: absent), through the browse API.
    async fn branch_tip(
        &self,
        project: forge_core::ProjectId,
        repo: ProjectRepository,
        user: UserId,
        branch: &forge_core::RefName,
    ) -> Result<Option<ObjectId>, ForgeError> {
        match self
            .require_branches(project, repo, user, &[("branch", branch)])
            .await
        {
            Ok(mut heads) => Ok(heads.pop()),
            Err(e) if crate::merges::is_ref_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
