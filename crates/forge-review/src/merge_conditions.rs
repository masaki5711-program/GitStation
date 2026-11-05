//! The merge conditions' commands of [`Review`] (M3 R6, ADR-0024 §4.1,
//! §7, §8, §9, §12): the project's merge settings (read, write), the
//! time-boxed CODEOWNERS switch, and the merge status — `evaluate_merge()`
//! on the facts as they are now, every blocking reason (the stable JSON of
//! `gstn mr status --json`). Each write is decided twice by `authorize()`:
//! the route's proof, then `reconfirm` on the facts locked in the
//! transaction.

use forge_auth::{
    Action, Authorized, CodeownersToggle, CodeownersToggleCtx, MergeRequestCtx, MergeSettingsCtx,
    ProjectCtx,
};
use forge_core::{MergeMethod, MergeSettingsValues, ObjectId, ProjectRepository, UserId};
use forge_db::review::{
    self, CodeownersChange, SettingsOutcome, StoredMergeSettings, StoredMergeability,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::approvals::{evaluate, ApprovalReport, STATE_BUDGET};
use crate::merges::{
    conditions, evaluate_merge, stored_mergeable, BlockReason, MergeFacts, RequiredCheck,
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
    /// M3 I1d: whether a work item of the project's binding is linked, when
    /// the merge rule requires a link (`None`: not required).
    pub work_item_link: Option<bool>,
    /// The project's settings as read.
    pub settings: StoredMergeSettings,
    /// The protection in effect on the target branch, from the rules read
    /// with the approval facts (U2 review: the viewer flags share that
    /// read); `None` when they were not read (no version yet).
    pub target_protection: Option<Option<forge_auth::RefProtection>>,
}

/// The settings as locked are the ones the caller read (U2): the same
/// `updated_at`, or both the defaults. `FG-REVIEW-0017` otherwise.
fn unchanged_since(
    project: forge_core::ProjectId,
    expected: Option<&str>,
    current: &StoredMergeSettings,
) -> Result<(), ForgeError> {
    let actual = current.updated_at.as_deref();
    if actual == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::MergeSettingsChanged)
        .with_context("action", Action::MergeSettingsManage.name())
        .with_context("reason", "stale_settings")
        .with_context("project_id", project.get())
        .with_context("expected", expected.unwrap_or("none"))
        .with_context("actual", actual.unwrap_or("none")))
}

impl Review {
    /// The project's merge settings, for a route loader (the decision that
    /// binds a write reads them again under the lock).
    pub async fn stored_merge_settings(
        &self,
        project: forge_core::ProjectId,
    ) -> Result<StoredMergeSettings, ForgeError> {
        review::merge_settings(&self.db, project).await
    }

    /// The highest minimum push role of `project`'s protection rules (the
    /// CODEOWNERS toggle's loader; `None` without rules).
    pub async fn highest_protection(
        &self,
        project: forge_core::ProjectId,
    ) -> Result<Option<forge_core::Role>, ForgeError> {
        Ok(forge_db::protected_refs::rules_for(&self.db, project)
            .await?
            .highest_push_min_role())
    }

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
    /// not touched (it has its own action). R6 review: decided again on the
    /// settings as locked — a change that loosens the approvals needs a role
    /// at or above every protection of the project. U2: `expected_updated_at`
    /// is the `updated_at` the caller read (`None`: the defaults, never
    /// stored); another one under the lock is `FG-REVIEW-0017
    /// reason=stale_settings` — someone changed the settings in between and
    /// nothing is overwritten silently.
    pub async fn put_merge_settings(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeSettingsCtx>,
        values: &MergeSettingsValues,
        expected_updated_at: Option<&str>,
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
                let fresh = MergeSettingsCtx::new(
                    loaded(&facts.project),
                    facts.highest_protection,
                    review::settings::loosens(&facts.current.values, values),
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)?;
                unchanged_since(project, expected_updated_at, &facts.current)
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
                work_item_link: None,
                settings,
                target_protection: None,
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
        let target_protection =
            forge_auth::RefProtection::of(&facts.protection, &current.target_branch);
        let unresolved = review::unresolved_discussions(&self.db, current.id).await?;
        let stored: Option<StoredMergeability> = review::mergeability(&self.db, current.id).await?;
        let live = review::merges::latest_merge(&self.db, current.id)
            .await?
            .is_some_and(|m| m.state.is_live());
        // M3 C1: the head's runs, only when the merge rule requires a check;
        // a failed read fails the status (a store failure is never
        // "no run reported").
        let runs = if facts
            .protection
            .merge_rule(&current.target_branch)
            .required_checks
            .is_empty()
        {
            forge_core::CheckRunsOnHead::default()
        } else {
            self.checks.runs_on(project, &head).await?
        };
        // M3 I1d: the link, only when the merge rule requires one. Read on
        // the read pool like the rest of the status, so it may trail the
        // primary by a commit (a link just made can show as missing for a
        // moment); a merge decides it again under its locks (MergeGate).
        let linked = facts
            .protection
            .merge_rule(&current.target_branch)
            .require_work_item_link
            && forge_db::work_items::work_item_linked(&self.db, current.id).await?;
        let conditions = conditions(
            &facts.protection,
            &current.target_branch,
            &head,
            unresolved,
            linked,
            &runs,
        );
        let inputs = git.inputs(&current, &head, facts);
        let report = evaluate(&inputs);
        let mergeable = stored_mergeable(stored.as_ref(), &head, target_tip.as_ref());
        // R6 review: a fast-forward's ancestry is evaluated here too (the
        // job decides it again under the lock).
        // R6 review round 2: a git failure leaves it unknown (`None`, no
        // `ff_not_possible`), logged once here, instead of failing the read.
        let ff_possible = match (method, target_tip.as_ref()) {
            (MergeMethod::FastForward, Some(tip)) => match self
                .review_git()?
                .is_ancestor(
                    crate::ready(project, repo)?,
                    tip,
                    &head,
                    crate::approvals::READ_TIMEOUT,
                )
                .await
            {
                Ok(ancestor) => Some(ancestor),
                Err(e) => {
                    tracing::warn!(
                        target: "forge_review",
                        event = "forge.review.status_ff_unknown",
                        error_code = e.code().id(),
                        request_id = %request_id,
                        merge_request_id = current.id.get(),
                        project_id = project.get(),
                        target_tip = tip.as_str(),
                        head_sha = head.as_str(),
                        error = %e,
                        "the status could not check a fast-forward; shown as unknown (the merge job decides it)"
                    );
                    None
                }
            },
            (MergeMethod::FastForward, None)
            | (MergeMethod::MergeCommit | MergeMethod::Squash, _) => None,
        };
        let decision = evaluate_merge(&MergeFacts {
            state: current.state,
            draft: current.draft,
            merge_in_progress: live,
            source_present: source_tip.is_some(),
            mergeable: Some(mergeable),
            method,
            ff_possible,
            approvals: inputs,
            checks: conditions.checks.clone(), // clone: the status shows them too
            discussions: conditions.discussions,
            work_item_link: conditions.work_item_link,
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
            work_item_link: conditions.work_item_link,
            settings,
            target_protection: Some(target_protection),
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
