//! The enqueue's early refusal (load test 2026-11-19, ADR-0037 addendum
//! "enqueue fast refusal"): before any git call, the facts of the
//! decision that come from the database alone — the merge request's state
//! and draft flag, the required checks of the head, the unresolved
//! discussions and the work item link — are read without locks, and
//! `evaluate_merge()` runs on them. Only the reasons those facts decide
//! by themselves are kept ([`decided_without_git`]); every fact that needs
//! git (the source branch, the trial merge, the approvals' committers,
//! changed paths and CODEOWNERS, a check whose run may sit on a merge
//! result of the live target tip) is given its most permissive value, so
//! a kept reason is one the full decision finds too. When one is kept the
//! enqueue answers with the same `blocked()` error the full path would
//! give; otherwise it goes on to the full decision, which stays the final
//! word (a pre-read that finds nothing changes nothing).

use forge_core::merge_results::reported_on_merge_results;
use forge_core::{CheckName, Freshness, MergeCheckRuns, MergeMethod, MergeRequestState, ObjectId};
use forge_db::review::{ApprovalFacts, StoredMergeRequest, StoredMergeSettings};
use forge_errors::ForgeError;

use super::super::conditions::{conditions, CheckRuns};
use super::super::evaluate::{blocked, evaluate_merge, BlockReason, CheckRun, MergeFacts};
use super::deferred_run;
use crate::approvals::{ChangedPathsFact, CodeownersFact, Committers, GitFacts};
use crate::Review;

/// Whether `reason` is decided by the database facts alone — the same
/// for every value of the git facts. Exhaustive, so a new reason is
/// decided here before it can be kept.
pub(crate) fn decided_without_git(reason: &BlockReason) -> bool {
    match reason {
        BlockReason::NotOpen
        | BlockReason::Draft
        | BlockReason::RequiredCheckMissing { .. }
        | BlockReason::RequiredCheckNotSuccessful { .. }
        | BlockReason::UnresolvedDiscussions { .. }
        | BlockReason::WorkItemLinkMissing => true,
        // A live attempt is FG-REVIEW-0011, decided by forge-db.
        BlockReason::MergeInProgress
        // git: the branch tips, the trial merge, ancestry.
        | BlockReason::SourceBranchMissing
        | BlockReason::MergeabilityUnknown
        | BlockReason::Conflict
        | BlockReason::FfNotPossible
        | BlockReason::NothingToMerge
        // git: committers, changed paths, CODEOWNERS.
        | BlockReason::ApprovalsMissing { .. }
        | BlockReason::CodeownerApprovalMissing { .. }
        | BlockReason::CodeownersInvalid { .. }
        | BlockReason::ConfigChangeNeedsMaintainerApproval
        | BlockReason::ChangedPathsUnknown { .. }
        // The live target tip decides it.
        | BlockReason::RequiredCheckStale { .. } => false,
    }
}

/// The required checks as far as the head alone decides them:
/// `ci/pipeline` is the queue's to prove (as [`super::TrainRuns`] at the
/// enqueue), and a name with a run on any merge result of the head may be
/// decided by the current one (the live target tip): both pass here and
/// are left to the full decision. Every other name is its run on the
/// head, whatever the tip.
struct HeadOnly<'a>(&'a MergeCheckRuns);

impl CheckRuns for HeadOnly<'_> {
    fn run(
        &self,
        head: &ObjectId,
        _target_tip: Option<&ObjectId>,
        name: &CheckName,
    ) -> Option<(CheckRun, Freshness)> {
        let tip_decides = reported_on_merge_results(name)
            || self
                .0
                .results
                .iter()
                .any(|r| r.runs.run(&r.result, name).is_some());
        if tip_decides {
            return deferred_run().map(|run| (run, Freshness::Fresh));
        }
        self.0.resolve(head, None, name)
    }
}

/// The database facts of the early decision (read without locks).
pub(crate) struct DbFacts<'a> {
    pub(crate) stored: &'a StoredMergeRequest,
    pub(crate) head: &'a ObjectId,
    pub(crate) method: MergeMethod,
    pub(crate) settings: &'a StoredMergeSettings,
    pub(crate) protection: &'a forge_core::ProtectionRules,
    pub(crate) runs: &'a MergeCheckRuns,
    pub(crate) unresolved: u32,
    pub(crate) linked: bool,
}

/// The reasons `evaluate_merge()` gives on `facts` that no git fact can
/// change (module docs), in its order; empty when there is none.
pub(crate) fn refusal_without_git(facts: &DbFacts<'_>) -> Vec<BlockReason> {
    let target = &facts.stored.target_branch;
    let conditions = conditions(
        facts.protection,
        (target, None),
        facts.head,
        facts.unresolved,
        facts.linked,
        &HeadOnly(facts.runs),
    );
    // The most permissive git facts: no committer, nothing changed, no
    // CODEOWNERS file (every reason they could add is dropped below).
    let git = GitFacts {
        committers: Committers::Known(std::collections::BTreeMap::new()),
        changed: ChangedPathsFact::Known(Vec::new()),
        codeowners: CodeownersFact::Absent,
    };
    let approvals = ApprovalFacts {
        approvals: Vec::new(),
        rules: Vec::new(),
        settings: facts.settings.clone(), // clone: the inputs own their facts
        protection: facts.protection.clone(), // clone: as above
    };
    let decision = evaluate_merge(&MergeFacts {
        state: facts.stored.state,
        draft: facts.stored.draft,
        merge_in_progress: false,
        source_present: true,
        mergeable: None,
        method: facts.method,
        ff_possible: None,
        approvals: git.inputs(facts.stored, facts.head, approvals),
        checks: conditions.checks,
        discussions: conditions.discussions,
        work_item_link: conditions.work_item_link,
    });
    match decision {
        Ok(_) => Vec::new(),
        Err(reasons) => reasons.into_iter().filter(decided_without_git).collect(),
    }
}

impl Review {
    /// The enqueue's early refusal of `current` at `head` (module docs):
    /// `Some` FG-REVIEW-0007 when the database alone refuses, `None` to go
    /// on to the full decision — also when a merge attempt is live (the
    /// full path answers FG-REVIEW-0011 for it). Called after the route's
    /// `authorize()` and the stale-view check, never before.
    pub(super) async fn refused_without_git(
        &self,
        current: &StoredMergeRequest,
        head: &ObjectId,
        method: MergeMethod,
        settings: &StoredMergeSettings,
    ) -> Result<Option<ForgeError>, ForgeError> {
        let project = current.project_id;
        let protection = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let rule = protection.merge_rule(&current.target_branch);
        let open = current.state == MergeRequestState::Open;
        if open
            && !current.draft
            && rule.required_checks.is_empty()
            && !rule.require_resolved_discussions
            && !rule.require_work_item_link
        {
            // Nothing the database decides can refuse: no further read.
            return Ok(None);
        }
        let runs = if rule.required_checks.is_empty() {
            MergeCheckRuns::default()
        } else {
            self.checks.merge_runs_on(project, current.id, head).await?
        };
        let unresolved = if rule.require_resolved_discussions {
            forge_db::review::unresolved_discussions(&self.db, current.id).await?
        } else {
            0
        };
        let linked = rule.require_work_item_link
            && forge_db::work_items::work_item_linked(&self.db, current.id).await?;
        let reasons = refusal_without_git(&DbFacts {
            stored: current,
            head,
            method,
            settings,
            protection: &protection,
            runs: &runs,
            unresolved,
            linked,
        });
        if reasons.is_empty() {
            return Ok(None);
        }
        let live = forge_db::review::merges::latest_merge(&self.db, current.id)
            .await?
            .is_some_and(|m| m.state.is_live());
        if live {
            return Ok(None);
        }
        let names: Vec<&str> = reasons.iter().map(BlockReason::as_str).collect();
        tracing::debug!(
            target: "forge_review",
            event = "forge.merge_queue.enqueue_refused_without_git",
            project_id = project.get(),
            merge_request_id = current.id.get(),
            head_sha = head.as_str(),
            reasons = names.join(",").as_str(),
            "the database alone refuses the enqueue; no git was read"
        );
        Ok(Some(blocked(&reasons)))
    }
}
