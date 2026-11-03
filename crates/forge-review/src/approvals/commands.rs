//! The approval commands of [`Review`] (M3 R5): approve, unapprove, the
//! approval state, and the approval rules. Each write is decided twice by
//! `authorize()` — the route's proof, then `reconfirm` on the facts locked
//! in the transaction — like every review command.

use forge_auth::{Action, Authorized, MergeRequestCtx, ProjectCtx};
use forge_core::{ObjectId, ProjectRepository, RefPattern, UserId};
use forge_db::review::{
    self, ApproveOutcome, NewApprovalRule, RuleOutcome, StoredApproval, StoredApprovalRule,
};
use forge_errors::{ErrorCode, ForgeError};

use super::{evaluate, ApprovalReport, Asker, Budget, ReviewGit, REQUEST_BUDGET, STATE_BUDGET};
use crate::merges::request_head_changed;
use crate::{
    account, caller, change_context, loaded, locked, ready, require, shape_mismatch, Review,
    ReviewRequest,
};

/// What an approve or unapprove did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalAnswer {
    pub outcome: ApproveOutcome,
    /// The approval written, kept (unchanged) or ended (`None`: unapprove
    /// with nothing to withdraw).
    pub approval: Option<StoredApproval>,
}

/// The approval state of a merge request now (the GET route): how each
/// live approval counts, the rules, the CODEOWNERS rules the changes need,
/// and what blocks the merge on the approval side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalState {
    /// `None`: no version yet (nothing can be approved).
    pub head_sha: Option<ObjectId>,
    pub target_branch: forge_core::RefName,
    pub report: Option<ApprovalReport>,
    /// The trial merge of the head into the target tip conflicts (R5
    /// re-review round 2): nothing can merge, whatever the approvals say.
    pub conflict: bool,
}

/// An approval rule as asked for (users by username, groups by full path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleAsk {
    pub name: String,
    pub target_pattern: Option<String>,
    pub required_approvals: i32,
    pub users: Vec<String>,
    pub groups: Vec<String>,
}

fn param_invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

fn name_ok(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 64
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

impl Review {
    /// Where the approval evaluation reads git (M3 R5; `forge serve` sets
    /// it). Without it, approving and merging are `FG-SYS-0001
    /// reason=review_git_unconfigured`.
    #[must_use]
    pub fn with_review_git(mut self, git: ReviewGit) -> Self {
        self.review_git = Some(git);
        self
    }

    pub(crate) fn review_git(&self) -> Result<&ReviewGit, ForgeError> {
        self.review_git.as_ref().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "review_git_unconfigured")
        })
    }

    /// The target branch's tip (`None`: absent), through the browse API's
    /// ref list.
    pub(crate) async fn target_tip(
        &self,
        project: forge_core::ProjectId,
        repo: ProjectRepository,
        user: UserId,
        mr: &review::StoredMergeRequest,
    ) -> Result<Option<ObjectId>, ForgeError> {
        match self
            .require_branches(project, repo, user, &[("target_branch", &mr.target_branch)])
            .await
        {
            Ok(mut heads) => Ok(heads.pop()),
            Err(e) if crate::merges::is_ref_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The git facts of `mr` at `head` (approve, the state, the merge
    /// request).
    pub(crate) async fn git_facts(
        &self,
        request_id: &forge_core::RequestId,
        repo: ProjectRepository,
        user: UserId,
        mr: &review::StoredMergeRequest,
        head: &ObjectId,
        budget: (std::time::Duration, usize),
    ) -> Result<super::GitFacts, ForgeError> {
        let git = self.review_git()?;
        let tip = self.target_tip(mr.project_id, repo, user, mr).await?;
        let asker = Asker {
            request_id,
            project: mr.project_id,
            repo: ready(mr.project_id, repo)?,
        };
        git.facts(
            &self.db,
            asker,
            mr,
            head,
            tip.as_ref(),
            &mut Budget::new(budget),
        )
        .await
    }

    /// `POST …/merge-requests/{iid}/approve` (`MergeRequestApprove`): approve
    /// the head the caller saw (`sha`, required; another head is
    /// `FG-REVIEW-0008 reason=stale_view`) into the current target
    /// (ADR-0024 §8.1). The answer says whether the approval counts now
    /// (`merge_request.approved` carries the same).
    pub async fn approve(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        sha: &ObjectId,
    ) -> Result<ApprovalAnswer, ForgeError> {
        require(proof.action(), Action::MergeRequestApprove)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_approve"));
        };
        let user = caller(proof)?;
        let current = self
            .merge_request(project, iid)
            .await?
            .ok_or_else(crate::absent)?;
        let recorded = current.head_sha.as_deref().unwrap_or("");
        if recorded != sha.as_str() {
            return Err(request_head_changed("stale_view", recorded, sha.as_str())
                .with_context("merge_request_id", current.id.get()));
        }
        let git = self
            .git_facts(
                &request.request_id,
                repo,
                user,
                &current,
                sha,
                REQUEST_BUDGET,
            )
            .await?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, approval, ()) = review::approve_mr_decided(
            &self.db,
            &ctx,
            (project, iid),
            user,
            |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)?;
                let stored = facts.merge_request.as_ref().ok_or_else(crate::absent)?;
                if stored.head_sha.as_deref() != Some(sha.as_str()) {
                    return Err(request_head_changed(
                        "stale_view",
                        stored.head_sha.as_deref().unwrap_or(""),
                        sha.as_str(),
                    ));
                }
                if stored.target_branch != current.target_branch {
                    // Retargeted since the git facts were read.
                    return Err(
                        request_head_changed("stale_view", sha.as_str(), sha.as_str())
                            .with_context("detail", "target_changed"),
                    );
                }
                Ok(())
            },
            |facts, approvals, id| {
                let Some(mr) = facts.merge_request.as_ref() else {
                    return (false, None);
                };
                let inputs = git.clone().inputs(mr, sha, approvals.clone()); // clone: the evaluation owns its inputs
                let report = evaluate(&inputs);
                report
                    .approvals
                    .iter()
                    .find(|a| a.approval == id)
                    .map_or((false, None), |a| (a.counted, a.uncounted_reason))
            },
        )
        .await
        .map_err(|e| e.with_context("merge_request_id", current.id.get()))?;
        Ok(ApprovalAnswer {
            outcome,
            approval: Some(approval),
        })
    }

    /// `POST …/merge-requests/{iid}/unapprove` (`MergeRequestUnapprove`):
    /// withdraw the caller's own approval (none: unchanged).
    pub async fn unapprove(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
    ) -> Result<ApprovalAnswer, ForgeError> {
        require(proof.action(), Action::MergeRequestUnapprove)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_unapprove"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, approval, ()) =
            review::unapprove_mr_decided(&self.db, &ctx, (project, iid), user, |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(ApprovalAnswer { outcome, approval })
    }

    /// `GET …/merge-requests/{iid}/approvals` (`MergeRequestRead`): the
    /// evaluation of the approvals now (reads only).
    pub async fn approval_state(
        &self,
        request_id: &forge_core::RequestId,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
    ) -> Result<ApprovalState, ForgeError> {
        require(proof.action(), Action::MergeRequestRead)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_approvals"));
        };
        let user = caller(proof)?;
        let current = self
            .merge_request(project, iid)
            .await?
            .ok_or_else(crate::absent)?;
        let head = current
            .head_sha
            .as_deref()
            .map(ObjectId::parse)
            .transpose()?;
        let Some(head) = head else {
            return Ok(ApprovalState {
                head_sha: None,
                target_branch: current.target_branch,
                report: None,
                conflict: false,
            });
        };
        let git = self
            .git_facts(request_id, repo, user, &current, &head, STATE_BUDGET)
            .await?;
        let facts = review::approval_facts(&self.db, project, current.id).await?;
        let conflict = git.changed == super::ChangedPathsFact::Conflict;
        let report = evaluate(&git.inputs(&current, &head, facts));
        Ok(ApprovalState {
            head_sha: Some(head),
            target_branch: current.target_branch,
            report: Some(report),
            conflict,
        })
    }

    /// `GET …/approval-rules` (`ProjectRead`): the project's rules.
    pub async fn approval_rules(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<StoredApprovalRule>, ForgeError> {
        require(proof.action(), Action::ProjectRead)?;
        review::approval_rules(&self.db, *proof.id()).await
    }

    /// Check a rule's fields and resolve its approvers (`FG-WEB-0006`).
    async fn rule(&self, ask: &RuleAsk) -> Result<NewApprovalRule, ForgeError> {
        if !name_ok(&ask.name) {
            return Err(param_invalid("name", "format"));
        }
        if !(1..=20).contains(&ask.required_approvals) {
            return Err(param_invalid("required_approvals", "range"));
        }
        let target_pattern = ask
            .target_pattern
            .as_deref()
            .map(|p| RefPattern::parse(p).map_err(|_| param_invalid("target_pattern", "format")))
            .transpose()?;
        if ask.users.len() + ask.groups.len() > review::MAX_RULE_APPROVERS {
            return Err(param_invalid("users", "too_many_approvers"));
        }
        let found = review::owners(&self.db, &ask.users, &ask.groups).await?;
        let mut users = Vec::with_capacity(ask.users.len());
        for name in &ask.users {
            let id = found
                .users
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, id)| *id)
                .ok_or_else(|| param_invalid("users", "unknown_user"))?;
            users.push(id);
        }
        let mut groups = Vec::with_capacity(ask.groups.len());
        for path in &ask.groups {
            let id = found
                .groups
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, id)| *id)
                .ok_or_else(|| param_invalid("groups", "unknown_group"))?;
            groups.push(id);
        }
        Ok(NewApprovalRule {
            name: ask.name.clone(), // clone: the command owns its rule
            target_pattern,
            required_approvals: ask.required_approvals,
            users,
            groups,
        })
    }

    /// `PUT …/approval-rules/{name}` (`ApprovalRuleManage`): create or
    /// replace a rule; never touches approvals (ADR-0024 §8.6). U2
    /// follow-up: `expected_updated_at` is the rule's `updated_at` the
    /// caller read (`None`: create only); another rule under the lock, after
    /// the re-authorization, is `FG-REVIEW-0018`.
    pub async fn put_approval_rule(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        ask: &RuleAsk,
        expected_updated_at: Option<&str>,
    ) -> Result<(RuleOutcome, StoredApprovalRule), ForgeError> {
        require(proof.action(), Action::ApprovalRuleManage)?;
        let project = *proof.id();
        let user = caller_of(proof)?;
        let new = self.rule(ask).await?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, rule, ()) = review::put_approval_rule_mr_decided(
            &self.db,
            &ctx,
            project,
            user,
            &new,
            expected_updated_at,
            |facts| {
                proof
                    .reconfirm(
                        account(facts.account),
                        ProjectCtx::new(loaded(&facts.project)),
                    )
                    .map(drop)
            },
        )
        .await?;
        Ok((outcome, rule))
    }

    /// `DELETE …/approval-rules/{name}` (`ApprovalRuleManage`).
    pub async fn remove_approval_rule(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        name: &str,
    ) -> Result<RuleOutcome, ForgeError> {
        require(proof.action(), Action::ApprovalRuleManage)?;
        let project = *proof.id();
        let user = caller_of(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, ()) =
            review::remove_approval_rule_mr_decided(&self.db, &ctx, project, user, name, |facts| {
                proof
                    .reconfirm(
                        account(facts.account),
                        ProjectCtx::new(loaded(&facts.project)),
                    )
                    .map(drop)
            })
            .await?;
        Ok(outcome)
    }
}

fn caller_of(proof: &Authorized<ProjectCtx>) -> Result<UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}
