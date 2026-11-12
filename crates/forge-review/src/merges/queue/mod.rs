//! The merge queue (M4 C8, plan 14 §5 C8, ADR-0030).
//!
//! - This file: a person's commands on [`crate::Review`] — "merge when
//!   ready" ([`crate::Review::enqueue`]), [`crate::Review::dequeue`],
//!   [`crate::Review::set_queue_paused`] and the reads — and the decision
//!   both the enqueue and the landing make ([`decide_queued`]): the merge's
//!   own `authorize(MergeRequestMerge)` and `evaluate_merge()`, with the
//!   required check `ci/pipeline` read from the queue's train
//!   ([`TrainRuns`]) instead of the merge request's merge result.
//! - `worker.rs`: [`MergeQueue`], the worker `forge serve` runs — batches
//!   of at most four entries built on the target tip as one train,
//!   the train's pipeline, the bisection of a red batch, and the landing
//!   (one compare-and-swap of the target to the tested commit, then the
//!   normal merge record per entry).

use forge_auth::{Action, Authorized, MergeRequestCtx, ProjectCtx, RefProtection};
use forge_core::merge_results::reported_on_merge_results;
use forge_core::{
    CheckConclusion, CheckName, CheckStatus, Freshness, MergeCheckRuns, MergeMethod, ObjectId,
    ProjectId, ProjectRepository, RefName,
};
use forge_db::review::merge_queue::{
    self as db, PauseOutcome, QueueView, StoredEntry, StoredQueue,
};
use forge_db::review::merges::{CredentialRef, MergeGate, NewEntry};
use forge_errors::{ErrorCode, ForgeError};

use super::conditions::CheckRuns;
use super::evaluate::{blocked, evaluate_merge, stored_mergeable, CheckRun, MergeFacts, Mergeable};
use crate::{
    account, caller, change_context, loaded, locked, require, shape_mismatch, Review, ReviewRequest,
};

mod worker;

pub use worker::{MergeQueue, QueueLimits, TickReport};

/// The runs a queued merge is decided on: `ci/pipeline` (the name forge
/// reports on merge results) from the train — `None` is "no run" — and
/// every other name as a merge request's merge decides it (C7's rules on
/// the head and its merge results).
pub(crate) struct TrainRuns<'a> {
    pub(crate) inner: &'a MergeCheckRuns,
    pub(crate) train: Option<CheckRun>,
}

impl CheckRuns for TrainRuns<'_> {
    fn run(
        &self,
        head: &ObjectId,
        target_tip: Option<&ObjectId>,
        name: &CheckName,
    ) -> Option<(CheckRun, Freshness)> {
        if reported_on_merge_results(name) {
            return self.train.map(|run| (run, Freshness::Fresh));
        }
        self.inner.resolve(head, target_tip, name)
    }
}

/// At the enqueue the train does not exist yet: `ci/pipeline` is the
/// queue's to prove (ADR-0030 §2), every other condition must hold now.
pub(crate) fn deferred_run() -> Option<CheckRun> {
    CheckRun::new(CheckStatus::Completed, Some(CheckConclusion::Success))
}

/// The `ci/pipeline` run a train's pipeline stands for — the same mapping
/// as forge-pipelines' `pipeline_check_run` (`ci_outputs`), so the queue
/// counts what the check would say without waiting for the worker to
/// write it. `None`: no pipeline (no `.forge-ci.yml`).
pub(crate) fn train_run(status: Option<forge_db::pipelines::PipelineStatus>) -> Option<CheckRun> {
    use forge_db::pipelines::PipelineStatus as P;
    let (status, conclusion) = match status? {
        P::Pending => (CheckStatus::Queued, None),
        P::Running => (CheckStatus::InProgress, None),
        P::Succeeded | P::Skipped => (CheckStatus::Completed, Some(CheckConclusion::Success)),
        P::Failed | P::Invalid => (CheckStatus::Completed, Some(CheckConclusion::Failure)),
        P::Error => (CheckStatus::Completed, Some(CheckConclusion::Error)),
        P::Canceled => (CheckStatus::Completed, Some(CheckConclusion::Neutral)),
    };
    CheckRun::new(status, conclusion)
}

/// What a queued merge is decided on besides the locked gate: who asked,
/// for which head and method, and the live tips.
pub(crate) struct Asked<'a> {
    pub(crate) requested_by: forge_core::UserId,
    pub(crate) head: &'a ObjectId,
    pub(crate) method: MergeMethod,
    pub(crate) target: &'a RefName,
    pub(crate) source_tip: Option<&'a ObjectId>,
    pub(crate) target_tip: Option<&'a ObjectId>,
    pub(crate) approvals: crate::approvals::GitFacts,
}

/// The decision of a queued merge (the landing): the requester's principal
/// rebuilt from the database, `authorize(MergeRequestMerge)` with the
/// target's protection as stored now, the method still allowed, the head
/// unchanged, then `evaluate_merge()` with `runs` (module docs). The
/// enqueue reconfirms its route proof instead (see `Review::enqueue`).
pub(crate) fn decide_queued(
    gate: &MergeGate,
    asked: &Asked<'_>,
    mergeable: Option<Mergeable>,
    ff_possible: Option<bool>,
    runs: &dyn CheckRuns,
) -> Result<(), ForgeError> {
    let stored = gate
        .facts
        .merge_request
        .as_ref()
        .ok_or_else(crate::absent)?;
    let principal = super::job_principal(asked.requested_by, gate.facts.account, &gate.credential)?;
    if &stored.target_branch != asked.target {
        return Err(ForgeError::new(ErrorCode::MergeQueueEntryRefused)
            .with_context("reason", "retargeted")
            .with_context("merge_request_id", stored.id.get()));
    }
    let protection = RefProtection::of(&gate.protection, &stored.target_branch);
    let ctx = MergeRequestCtx::merging(
        loaded(&gate.facts.project),
        crate::loaded_merge_request(stored),
        protection,
    );
    forge_auth::prove(&principal, Action::MergeRequestMerge, ctx).map(drop)?;
    super::conditions::method_allowed(&gate.approvals.settings, asked.method)?;
    check_head(stored, asked)?;
    evaluate(gate, stored, asked, mergeable, ff_possible, runs)
}

/// FG-REVIEW-0008 when the merge request's head (or its source branch's
/// tip) is not the head that was queued.
fn check_head(
    stored: &forge_db::review::StoredMergeRequest,
    asked: &Asked<'_>,
) -> Result<(), ForgeError> {
    let head = asked.head.as_str();
    let recorded = stored.head_sha.as_deref().unwrap_or("");
    if recorded != head {
        return Err(super::request_head_changed("source_moved", head, recorded));
    }
    if let Some(tip) = asked.source_tip {
        if tip != asked.head {
            return Err(super::request_head_changed(
                "source_moved",
                head,
                tip.as_str(),
            ));
        }
    }
    Ok(())
}

fn evaluate(
    gate: &MergeGate,
    stored: &forge_db::review::StoredMergeRequest,
    asked: &Asked<'_>,
    mergeable: Option<Mergeable>,
    ff_possible: Option<bool>,
    runs: &dyn CheckRuns,
) -> Result<(), ForgeError> {
    let conditions = super::conditions::conditions(
        &gate.protection,
        (&stored.target_branch, asked.target_tip),
        asked.head,
        gate.unresolved_discussions,
        gate.work_item_linked,
        runs,
    );
    let facts = MergeFacts {
        state: stored.state,
        draft: stored.draft,
        merge_in_progress: false,
        source_present: asked.source_tip.is_some(),
        mergeable,
        method: asked.method,
        ff_possible,
        approvals: asked
            .approvals
            .clone() // clone: each decision evaluates its own inputs
            .inputs(stored, asked.head, gate.approvals.clone()), // clone: as above
        checks: conditions.checks,
        discussions: conditions.discussions,
        work_item_link: conditions.work_item_link,
    };
    evaluate_merge(&facts)
        .map(drop)
        .map_err(|reasons| blocked(&reasons))
}

/// What "merge when ready" asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueAsk {
    /// The head the requester saw (FG-REVIEW-0008 `stale_view` otherwise).
    pub sha: ObjectId,
    /// `None`: the project's default method.
    pub method: Option<MergeMethod>,
}

impl Review {
    /// `POST …/merge-requests/{iid}/merge-queue` (`MergeQueueEnqueue`,
    /// ADR-0030 §2): the method must be allowed (`FG-REVIEW-0010`), `sha`
    /// the merge request's head (`FG-REVIEW-0008 reason=stale_view`); then,
    /// under the review locks, the route's decision is made again with the
    /// target's protection as stored now and `evaluate_merge()` must find
    /// nothing but what the queue proves itself (`ci/pipeline`): FG-REVIEW-0007
    /// with every other reason. A live merge attempt is FG-REVIEW-0011, a
    /// live entry FG-CI-0020. Returns the entry (`queued`).
    pub async fn enqueue(
        &self,
        request: &ReviewRequest,
        credential: CredentialRef,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        ask: &QueueAsk,
    ) -> Result<StoredEntry, ForgeError> {
        require(proof.action(), Action::MergeQueueEnqueue)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_queue_enqueue"));
        };
        let settings = super::conditions::settings_of(
            &forge_db::review::merge_settings(&self.db, project).await?,
        );
        let method = ask.method.unwrap_or_else(|| settings.default_method());
        settings.check(method)?;
        let user = caller(proof)?;
        let current = self
            .merge_request(project, iid)
            .await?
            .ok_or_else(crate::absent)?;
        let recorded = current.head_sha.as_deref().unwrap_or("");
        if recorded != ask.sha.as_str() {
            return Err(
                super::request_head_changed("stale_view", recorded, ask.sha.as_str())
                    .with_context("merge_request_id", current.id.get()),
            );
        }
        let mut tips = Vec::with_capacity(2);
        for (which, name) in [
            ("source_branch", &current.source_branch),
            ("target_branch", &current.target_branch),
        ] {
            let found = self
                .require_branches(project, repo, user, &[(which, name)])
                .await;
            tips.push(match found {
                Ok(mut heads) => heads.pop(),
                Err(e) if super::is_ref_not_found(&e) => None,
                Err(e) => return Err(e),
            });
        }
        let target_tip = tips.pop().flatten();
        let source_tip = tips.pop().flatten();
        let asker = crate::approvals::Asker {
            request_id: &request.request_id,
            project,
            repo: crate::ready(project, repo)?,
        };
        let approval_git = self
            .review_git()?
            .facts(
                &self.db,
                asker,
                &current,
                &ask.sha,
                target_tip.as_ref(),
                &mut crate::approvals::Budget::new(crate::approvals::REQUEST_BUDGET),
            )
            .await?;
        let ctx = change_context(request, proof.principal(), user)?;
        let new = NewEntry {
            head_sha: ask.sha.clone(), // clone: forge-db owns its request
            method,
            message: None,
            credential,
        };
        let sha = &ask.sha;
        let (entry, ()) = forge_db::review::merges::enqueue_mr_decided(
            &self.db,
            &ctx,
            (project, iid),
            user,
            &new,
            |gate| {
                let mr = locked(&gate.facts)?;
                let protection = RefProtection::of(&gate.protection, &mr.target_branch);
                let fresh = MergeRequestCtx::merging(loaded(&gate.facts.project), mr, protection);
                proof
                    .reconfirm(account(gate.facts.account), fresh)
                    .map(drop)?;
                let stored = gate
                    .facts
                    .merge_request
                    .as_ref()
                    .ok_or_else(crate::absent)?;
                if gate.live.is_some() {
                    // forge-db answers FG-REVIEW-0011 for it.
                    return Ok(());
                }
                super::conditions::method_allowed(&gate.approvals.settings, method)?;
                let asked = Asked {
                    requested_by: user,
                    head: sha,
                    method,
                    target: &stored.target_branch,
                    source_tip: source_tip.as_ref(),
                    target_tip: target_tip.as_ref(),
                    approvals: approval_git.clone(), // clone: the gate evaluates its own copy
                };
                if stored.head_sha.as_deref() != Some(sha.as_str()) {
                    return Err(super::request_head_changed(
                        "stale_view",
                        stored.head_sha.as_deref().unwrap_or(""),
                        sha.as_str(),
                    ));
                }
                let runs = TrainRuns {
                    inner: &gate.check_runs,
                    train: deferred_run(),
                };
                let mergeable = Some(stored_mergeable(
                    gate.mergeability.as_ref(),
                    sha,
                    target_tip.as_ref(),
                ));
                evaluate(gate, stored, &asked, mergeable, None, &runs)
            },
            |mr| super::request::default_message(method, mr),
        )
        .await
        .map_err(|e| e.with_context("merge_request_id", current.id.get()))?;
        tracing::info!(
            target: "forge_review",
            event = "forge.merge_queue.enqueued",
            request_id = %request.request_id,
            project_id = project.get(),
            merge_request_id = current.id.get(),
            entry_id = entry.id,
            queue_id = entry.queue_id,
            head_sha = entry.head_sha.as_str(),
            method = entry.method.as_str(),
        );
        Ok(entry)
    }

    /// `DELETE …/merge-requests/{iid}/merge-queue` (`MergeQueueDequeue`):
    /// the entry, `removed` (`reason=dequeued`); FG-CI-0020
    /// `reason=not_queued` without a live entry.
    pub async fn dequeue(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
    ) -> Result<StoredEntry, ForgeError> {
        require(proof.action(), Action::MergeQueueDequeue)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_queue_dequeue"));
        };
        let user = caller(proof)?;
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (entry, ()) = db::dequeue_mr_decided(&self.db, &ctx, (project, iid), user, |facts| {
            let mr = locked(facts)?;
            let protection = RefProtection::of(&rules, &mr.target_branch);
            let fresh = MergeRequestCtx::merging(loaded(&facts.project), mr, protection);
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        tracing::info!(
            target: "forge_review",
            event = "forge.merge_queue.dequeued",
            request_id = %request.request_id,
            project_id = project.get(),
            merge_request_id = entry.merge_request.get(),
            entry_id = entry.id,
            queue_id = entry.queue_id,
        );
        Ok(entry)
    }

    /// `POST …/merge-queue/pause` (`MergeQueuePause`) and `…/resume`
    /// (`MergeQueueManage`): the queue of `target` after the change.
    pub async fn set_queue_paused(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<ProjectCtx>,
        target: &RefName,
        paused: bool,
    ) -> Result<(PauseOutcome, StoredQueue), ForgeError> {
        let action = if paused {
            Action::MergeQueuePause
        } else {
            Action::MergeQueueManage
        };
        require(proof.action(), action)?;
        let project: ProjectId = *proof.id();
        let user = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_without_user")
                .with_context("action", action.name())
        })?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, queue, ()) =
            db::set_paused_decided(&self.db, &ctx, (project, target), user, paused, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                let account = forge_auth::AccountFacts {
                    active: facts.account.active,
                    is_admin: facts.account.is_admin,
                };
                proof.reconfirm(account, fresh).map(drop)
            })
            .await?;
        tracing::info!(
            target: "forge_review",
            event = if paused { "forge.merge_queue.paused" } else { "forge.merge_queue.resumed" },
            request_id = %request.request_id,
            project_id = project.get(),
            queue_id = queue.id,
            target = target.as_str(),
            changed = outcome == PauseOutcome::Changed,
        );
        Ok((outcome, queue))
    }

    /// The queue of `target` in `project` (`MergeQueueRead`), with at most
    /// `recent` finished entries.
    pub async fn merge_queue(
        &self,
        project: ProjectId,
        target: &RefName,
        recent: i64,
    ) -> Result<QueueView, ForgeError> {
        db::queue_view(&self.db, project, target, recent).await
    }

    /// The latest entry of merge request `mr` and its place in the queue
    /// (`MergeQueueRead`).
    pub async fn merge_queue_entry(
        &self,
        mr: forge_core::MergeRequestId,
    ) -> Result<Option<(StoredEntry, Option<i64>)>, ForgeError> {
        db::entry_of(&self.db, mr).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_db::pipelines::PipelineStatus as P;

    fn name(s: &str) -> CheckName {
        CheckName::parse(s).expect("name")
    }

    #[test]
    fn the_train_decides_ci_pipeline_and_nothing_else() {
        let head = ObjectId::parse(&"a".repeat(40)).expect("oid");
        let inner = MergeCheckRuns::default();
        let green = TrainRuns {
            inner: &inner,
            train: train_run(Some(P::Succeeded)),
        };
        let (run, fresh) = green
            .run(&head, None, &name("ci/pipeline"))
            .expect("the train's run");
        assert!(run.succeeded());
        assert_eq!(fresh, Freshness::Fresh);
        assert!(green.run(&head, None, &name("ext/lint")).is_none());
        let none = TrainRuns {
            inner: &inner,
            train: train_run(None),
        };
        assert!(none.run(&head, None, &name("ci/pipeline")).is_none());
    }

    #[test]
    fn the_train_run_maps_like_the_ci_pipeline_check() {
        for (status, succeeded) in [
            (P::Succeeded, true),
            (P::Skipped, true),
            (P::Failed, false),
            (P::Invalid, false),
            (P::Error, false),
            (P::Canceled, false),
            (P::Pending, false),
            (P::Running, false),
        ] {
            let run = train_run(Some(status)).expect("a run");
            assert_eq!(run.succeeded(), succeeded, "{status:?}");
        }
        assert!(deferred_run().expect("run").succeeded());
    }
}
