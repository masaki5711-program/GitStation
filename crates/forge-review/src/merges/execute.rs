//! The merge job (ADR-0024 §4.2 steps 2–9): [`Merger::execute`].
//!
//! Every outcome that is a decision — a refusal, a conflict, a lost
//! compare-and-swap, an attempt someone else already moved — ends the run
//! successfully with a [`MergeReport`] (logged once here as
//! `forge.review.merge_completed`, with the refusal's `error_code`). A
//! failure the job should retry (the lock busy or lost, a locked ref, git
//! or the database unavailable before the ref moved) or that needs the
//! alert (`FG-REVIEW-0009`) is returned: the job runner logs it once.

use std::time::Duration;

use forge_auth::{prove, Action, Authorized, MergeRequestCtx, RefProtection};
use forge_core::{
    MergeMethod, MergeState, ObjectId, PushRecord, PushUpdateKind, PushedRef, RefName, RepoId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::merges::{
    self, finish_merge, GateOutcome, GateStep, MergeFinished, MergeGate, MergeLock, StoredMerge,
};
use forge_db::review::{self, StoredMergeRequest};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::commit::{CommitIdentity, NewCommit};
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::{CommitQuery, RefChange};

use super::evaluate::{blocked, evaluate_merge, MergeClearance, MergeFacts, Mergeable};
use super::request::head_changed;
use super::{job_principal, Merger};
use crate::{loaded, loaded_merge_request};

/// The most attempts one reconciliation run looks at.
pub const RECONCILE_BATCH: i64 = 50;

/// What one run did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeReport {
    /// `applied`, `already_applied`, `superseded`, `moved` (another run or
    /// the reconciliation owns the attempt), `finished` (the attempt was
    /// final already), `left_for_reconciliation`.
    pub outcome: &'static str,
    pub state: MergeState,
    pub new_sha: Option<ObjectId>,
    /// The refusal's code (`superseded`).
    pub error_code: Option<ErrorCode>,
}

impl MergeReport {
    fn of(outcome: &'static str, state: MergeState) -> Self {
        Self {
            outcome,
            state,
            new_sha: None,
            error_code: None,
        }
    }
}

/// The facts of one merge attempt the job needs besides the attempt.
pub(super) struct Subject {
    pub(super) mr: StoredMergeRequest,
    pub(super) repo: RepoId,
}

/// The `system` context of the job's own transactions (the attempt's
/// request id, so every line of the merge carries it).
pub(super) fn system_ctx(attempt: &StoredMerge) -> ChangeContext {
    ChangeContext::new(Actor::System, attempt.request_id.clone()) // clone: the context owns it
}

/// The requester's context (the merge's audit rows and events: the job
/// acts for them, ADR-0024 §5).
pub(super) fn requester_ctx(attempt: &StoredMerge, credential: Option<&[String]>) -> ChangeContext {
    let mut ctx = ChangeContext::new(
        Actor::User(attempt.requested_by),
        attempt.request_id.clone(), // clone: the context owns it
    );
    ctx.credential_id = Some(attempt.credential.id());
    ctx.effective_scope = credential.map(<[String]>::to_vec).unwrap_or_default();
    ctx
}

/// What the gate decided: both proofs a ref move needs (ADR-0024 §7).
pub(super) struct Decided {
    pub(super) proof: Authorized<MergeRequestCtx>,
    pub(super) clearance: MergeClearance,
    pub(super) scopes: Vec<String>,
}

/// The live tips the gate compares the attempt with, and the git facts of
/// the approval evaluation at the attempt's head into that target tip (M3
/// R5: read under the lock, ADR-0024 §4.2 step 4 / §9).
#[derive(Debug, Clone)]
pub(super) struct Tips {
    pub(super) source: Option<ObjectId>,
    pub(super) target: Option<ObjectId>,
    pub(super) approvals: crate::approvals::GitFacts,
}

fn not_prepared(attempt: &StoredMerge) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "merge_attempt_unprepared")
        .with_context("merge_id", attempt.id)
}

/// `FG-REVIEW-0009` MergeExecutionFailed with its reason and the attempt.
pub(super) fn execution_failed(
    reason: &'static str,
    attempt: &StoredMerge,
    repo: RepoId,
) -> ForgeError {
    let e = ForgeError::new(ErrorCode::MergeExecutionFailed)
        .with_context("reason", reason)
        .with_context("merge_id", attempt.id)
        .with_context("merge_request_id", attempt.merge_request.get())
        .with_context("repo_id", repo.get())
        .with_context("target", attempt.target_branch.as_str().to_owned());
    let e = match &attempt.expected_old {
        Some(old) => e.with_context("expected_old", old.as_str().to_owned()),
        None => e,
    };
    match &attempt.new_sha {
        Some(new) => e.with_context("new_sha", new.as_str().to_owned()),
        None => e,
    }
}

/// The decision of one gate (shared by tx1a, tx1b and a resume): the
/// requester's principal rebuilt from the database, `authorize()` with the
/// target's protection as stored now, the recorded head and the source tip
/// against the requested head, then `evaluate_merge()`.
pub(super) fn decide(
    gate: &MergeGate,
    attempt: &StoredMerge,
    tips: &Tips,
    mergeable: Option<Mergeable>,
    ff_possible: Option<bool>,
) -> Result<Decided, ForgeError> {
    let stored = gate
        .facts
        .merge_request
        .as_ref()
        .ok_or_else(crate::absent)?;
    let principal = job_principal(attempt.requested_by, gate.facts.account, &gate.credential)?;
    if stored.target_branch != attempt.target_branch {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "merge_target_changed")
            .with_context("merge_id", attempt.id));
    }
    let protection = RefProtection::of(&gate.protection, &stored.target_branch);
    let ctx = MergeRequestCtx::merging(
        loaded(&gate.facts.project),
        loaded_merge_request(stored),
        protection,
    );
    let proof = prove(&principal, Action::MergeRequestMerge, ctx)?;
    // M3 R6 (ADR-0024 §4.2 steps 3, 5): a setting changed while the merge
    // waited is decided again — the method must still be allowed.
    super::conditions::method_allowed(&gate.approvals.settings, attempt.method)?;
    let head = attempt.head_sha.as_str();
    let recorded = stored.head_sha.as_deref().unwrap_or("");
    if recorded != head {
        return Err(head_changed("source_moved", head, recorded));
    }
    if let Some(tip) = &tips.source {
        if tip != &attempt.head_sha {
            return Err(head_changed("source_moved", head, tip.as_str()));
        }
    }
    let approvals = tips
        .approvals
        .clone() // clone: each gate evaluates its own inputs
        .inputs(stored, &attempt.head_sha, gate.approvals.clone()); // clone: as above
    let conditions = super::conditions::conditions(
        &gate.protection,
        &stored.target_branch,
        &attempt.head_sha,
        gate.unresolved_discussions,
        gate.work_item_linked,
        &gate.check_runs,
    );
    let facts = MergeFacts {
        state: stored.state,
        draft: stored.draft,
        merge_in_progress: false,
        source_present: tips.source.is_some(),
        mergeable,
        method: attempt.method,
        ff_possible,
        approvals,
        checks: conditions.checks,
        discussions: conditions.discussions,
        work_item_link: conditions.work_item_link,
    };
    let clearance = evaluate_merge(&facts).map_err(|reasons| blocked(&reasons))?;
    let scopes = match &gate.credential.credential {
        forge_db::review::merges::CredentialState::Token { scopes, .. } => {
            scopes.iter().map(|s| s.as_str().to_owned()).collect()
        }
        forge_db::review::merges::CredentialState::Session { .. }
        | forge_db::review::merges::CredentialState::Missing => Vec::new(),
    };
    Ok(Decided {
        proof,
        clearance,
        scopes,
    })
}

impl Merger {
    /// The attempt's merge request and repository; `None`: the merge
    /// request is gone (purged).
    pub(super) async fn subject(
        &self,
        attempt: &StoredMerge,
    ) -> Result<Option<Subject>, ForgeError> {
        let Some(target) = review::refresh_target(&self.db, attempt.merge_request).await? else {
            return Ok(None);
        };
        if !target.repository_ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", target.repository.get())
                .with_context("merge_id", attempt.id));
        }
        Ok(Some(Subject {
            mr: target.merge_request,
            repo: target.repository,
        }))
    }

    /// `want`, cut to what is left of the merge lock's hold (review round 1:
    /// no git call under the lock outlives it); `FG-DB-0002
    /// reason=merge_lock_lost detail=hold_exceeded` (retryable) when nothing
    /// is left — git is not started.
    pub(super) fn bounded(&self, want: Duration, lock: &MergeLock) -> Result<Duration, ForgeError> {
        let left = lock.left();
        if left.is_zero() {
            return Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "merge_lock_lost")
                .with_context("detail", "hold_exceeded")
                .with_context("retryable", true)
                .with_context("repo_id", lock.repo().get()));
        }
        Ok(want.min(left))
    }

    /// The tips of the source and the target branch, and the approval
    /// evaluation's git facts at `attempt`'s head (under the lock; every
    /// git call bounded by what is left of the lock's hold).
    async fn tips(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        repo: RepoId,
        mr: &StoredMergeRequest,
    ) -> Result<Tips, ForgeError> {
        let timeout = self.bounded(self.timeouts.read, lock)?;
        let read = |name: &RefName| {
            let name = name.clone(); // clone: each read owns its name
            async move { self.git.read_ref(&self.storage, repo, &name, timeout).await }
        };
        let source = read(&mr.source_branch).await?;
        let target = read(&mr.target_branch).await?;
        let review_git = crate::approvals::ReviewGit::new(
            self.git.clone(),     // clone: Arc handles
            self.storage.clone(), // clone: the root path
        );
        let asker = crate::approvals::Asker {
            request_id: &attempt.request_id,
            project: attempt.project_id,
            repo,
        };
        let approvals = review_git
            .facts(
                &self.db,
                asker,
                mr,
                &attempt.head_sha,
                target.as_ref(),
                // One deadline for every read: what is left of the lock's
                // hold (R5 review).
                &mut crate::approvals::Budget::until(
                    std::time::Instant::now() + self.bounded(lock.left(), lock)?,
                    crate::approvals::JOB_CALLS,
                ),
            )
            .await?;
        Ok(Tips {
            source,
            target,
            approvals,
        })
    }

    /// Run merge attempt `merge_id` (module docs). Every log line carries
    /// the attempt's request id (the request that asked for the merge).
    pub async fn execute(&self, merge_id: i64) -> Result<MergeReport, ForgeError> {
        let attempt = merges::merge_attempt(&self.db, merge_id)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "merge_attempt")
                    .with_context("merge_id", merge_id)
            })?;
        if !attempt.state.is_live() {
            // A run that stopped between tx2 and the source deletion: the
            // retry deletes it (the outcome is still unset).
            if wants_source_deletion(&attempt) {
                if let Some(subject) = self.subject(&attempt).await? {
                    self.delete_source(&attempt, &subject).await;
                }
            }
            return Ok(MergeReport::of("finished", attempt.state));
        }
        let Some(subject) = self.subject(&attempt).await? else {
            merges::end_merge(
                &self.db,
                merge_id,
                &[MergeState::Queued, MergeState::Prepared],
                MergeState::Superseded,
                "merge_request_gone",
                None,
            )
            .await?;
            return Ok(MergeReport::of("superseded", MergeState::Superseded));
        };
        tracing::info!(
            target: "forge_review",
            event = "forge.review.merge_started",
            request_id = %attempt.request_id,
            merge_id,
            merge_request_id = attempt.merge_request.get(),
            project_id = attempt.project_id.get(),
            repo_id = subject.repo.get(),
            method = attempt.method.as_str(),
            state = attempt.state.as_str(),
        );
        let mut lock = merges::lock_merges(&self.db, subject.repo, self.timeouts.lock_wait).await?;
        let ran = self.locked(&mut lock, merge_id, &subject).await;
        lock.release().await;
        let report = ran?;
        log_completed(&attempt, &report);
        if report.outcome == "applied" || report.outcome == "already_applied" {
            let done = merges::merge_attempt(&self.db, merge_id).await?;
            if let Some(done) = done.filter(wants_source_deletion) {
                self.delete_source(&done, &subject).await;
            }
        }
        Ok(report)
    }

    /// The job's last attempt failed, or its failure can never succeed: an
    /// attempt that never passed the fence (`queued` / `prepared`: no ref
    /// moved) ends `superseded reason=job_failed` with the cause's code; an
    /// `executing` one is left to the reconciliation (only git can tell).
    /// `true` when this call ended it.
    pub async fn give_up(&self, merge_id: i64, cause: &ForgeError) -> Result<bool, ForgeError> {
        merges::end_merge(
            &self.db,
            merge_id,
            &[MergeState::Queued, MergeState::Prepared],
            MergeState::Superseded,
            "job_failed",
            Some(cause.code()),
        )
        .await
    }

    /// Everything under the merge lock: read the attempt again and go on
    /// from its state.
    async fn locked(
        &self,
        lock: &mut MergeLock,
        merge_id: i64,
        subject: &Subject,
    ) -> Result<MergeReport, ForgeError> {
        let attempt = merges::merge_attempt(&self.db, merge_id)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound).with_context("merge_id", merge_id)
            })?;
        match attempt.state {
            MergeState::Queued => self.prepare(lock, &attempt, subject).await,
            MergeState::Prepared => {
                // A run stopped after tx1b: decide again (nothing moves without
                // both proofs), then the fence.
                let tips = self.tips(lock, &attempt, subject.repo, &subject.mr).await?;
                let gate = self
                    .gate_facts(&attempt, &tips, Some(Mergeable::Clean), ff_known(&attempt))
                    .await;
                match gate {
                    Ok(decided) => self.apply(lock, &attempt, subject, &decided).await,
                    Err(refusal) => {
                        self.refused(&attempt, &[MergeState::Prepared], refusal)
                            .await
                    }
                }
            }
            MergeState::Executing => self.resume(lock, &attempt, subject).await,
            MergeState::Applied | MergeState::Superseded | MergeState::Failed => {
                Ok(MergeReport::of("finished", attempt.state))
            }
        }
    }

    /// A gate on the facts only (no state change).
    async fn gate_facts(
        &self,
        attempt: &StoredMerge,
        tips: &Tips,
        mergeable: Option<Mergeable>,
        ff_possible: Option<bool>,
    ) -> Result<Decided, ForgeError> {
        let outcome = merges::merge_gate_decided(
            &self.db,
            &system_ctx(attempt),
            attempt.id,
            GateStep::Facts,
            |g| decide(g, attempt, tips, mergeable, ff_possible),
        )
        .await?;
        match outcome {
            GateOutcome::Decided(d) => Ok(d),
            GateOutcome::Moved(state) => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "facts_gate_moved")
                .with_context("state", state.as_str())),
        }
    }

    /// End an attempt the decision refused (from `from`), and report it.
    async fn refused(
        &self,
        attempt: &StoredMerge,
        from: &[MergeState],
        refusal: ForgeError,
    ) -> Result<MergeReport, ForgeError> {
        if !is_refusal(&refusal) {
            // A failure, not a decision: the job retries or deads it.
            return Err(refusal);
        }
        let reason = refusal
            .context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map_or("refused", |(_, v)| match v.as_str() {
                "source_moved" => "source_moved",
                "target_moved" => "target_moved",
                "credential_revoked" => "credential_revoked",
                _ => "refused",
            });
        merges::end_merge(
            &self.db,
            attempt.id,
            from,
            MergeState::Superseded,
            reason,
            Some(refusal.code()),
        )
        .await?;
        log_refusal(attempt, &refusal);
        Ok(MergeReport {
            outcome: "superseded",
            state: MergeState::Superseded,
            new_sha: None,
            error_code: Some(refusal.code()),
        })
    }

    /// From `queued`: tx1a, the objects, tx1b, then [`Merger::apply`].
    async fn prepare(
        &self,
        lock: &mut MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
    ) -> Result<MergeReport, ForgeError> {
        let tips = self.tips(lock, attempt, subject.repo, &subject.mr).await?;
        // tx1a: decide before any object is written.
        let first = merges::merge_gate_decided(
            &self.db,
            &system_ctx(attempt),
            attempt.id,
            GateStep::Check,
            |g| decide(g, attempt, &tips, None, None).map(drop),
        )
        .await;
        match first {
            Ok(GateOutcome::Decided(())) => {}
            Ok(GateOutcome::Moved(state)) => return Ok(MergeReport::of("moved", state)),
            Err(refusal) => return self.refused_logged(attempt, refusal),
        }
        let Some(target_tip) = tips.target.clone() else {
            let refusal = blocked(&[super::BlockReason::MergeabilityUnknown])
                .with_context("detail", "target_missing");
            return self.refused(attempt, &[MergeState::Queued], refusal).await;
        };
        // The objects, outside any transaction (the lock is held).
        let (new_sha, mergeable, ff) = match attempt.method {
            MergeMethod::FastForward => {
                if target_tip == attempt.head_sha {
                    // The target is at the head already: nothing to merge
                    // (an update-ref to itself would record no change).
                    let refusal = blocked(&[super::BlockReason::NothingToMerge]);
                    return self.refused(attempt, &[MergeState::Queued], refusal).await;
                }
                let ff = self
                    .git
                    .is_ancestor(
                        &self.storage,
                        subject.repo,
                        &target_tip,
                        &attempt.head_sha,
                        self.bounded(self.timeouts.read, lock)?,
                    )
                    .await?;
                (attempt.head_sha.clone(), Mergeable::Clean, Some(ff)) // clone: the target moves to the head
            }
            MergeMethod::MergeCommit | MergeMethod::Squash => {
                let limits = MergeTreeLimits {
                    timeout: self.bounded(MergeTreeLimits::default().timeout, lock)?,
                    ..MergeTreeLimits::default()
                };
                match self
                    .git
                    .merge_tree(
                        &self.storage,
                        subject.repo,
                        &target_tip,
                        &attempt.head_sha,
                        limits,
                    )
                    .await?
                {
                    MergeTree::Conflicted {
                        files,
                        files_truncated,
                    } => {
                        let refusal = ForgeError::new(ErrorCode::MergeRequestConflict)
                            .with_context("reason", "conflict")
                            .with_context("head_sha", attempt.head_sha.as_str().to_owned())
                            .with_context("target_sha", target_tip.as_str().to_owned())
                            .with_context("conflict_files", files.len())
                            .with_context("files_truncated", files_truncated);
                        return self.refused(attempt, &[MergeState::Queued], refusal).await;
                    }
                    MergeTree::Clean { tree } => {
                        let timeout = self.bounded(self.timeouts.commit, lock)?;
                        let commit = match self
                            .write_commit(attempt, subject, &tree, &target_tip, timeout)
                            .await
                        {
                            Ok(commit) => commit,
                            // An identity git would misread (FG-REVIEW-0012).
                            Err(e) if is_refusal(&e) => {
                                return self.refused(attempt, &[MergeState::Queued], e).await
                            }
                            Err(e) => return Err(e),
                        };
                        (commit, Mergeable::Clean, None)
                    }
                }
            }
        };
        // The invariant of ADR-0024 §4.1: the new target descends from the old.
        if ff != Some(false)
            && !self
                .git
                .is_ancestor(
                    &self.storage,
                    subject.repo,
                    &target_tip,
                    &new_sha,
                    self.bounded(self.timeouts.read, lock)?,
                )
                .await?
        {
            let e = ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_not_descendant")
                .with_context("merge_id", attempt.id);
            merges::end_merge(
                &self.db,
                attempt.id,
                &[MergeState::Queued],
                MergeState::Superseded,
                "merge_not_descendant",
                Some(ErrorCode::Internal),
            )
            .await?;
            return Err(e);
        }
        // tx1b: decide again on fresh facts, then `queued → prepared`.
        let step = GateStep::Prepare {
            expected_old: target_tip.clone(), // clone: the step owns its ids
            new_sha: new_sha.clone(),         // clone: as above
        };
        let second =
            merges::merge_gate_decided(&self.db, &system_ctx(attempt), attempt.id, step, |g| {
                decide(g, attempt, &tips, Some(mergeable), ff)
            })
            .await;
        let decided = match second {
            Ok(GateOutcome::Decided(d)) => d,
            Ok(GateOutcome::Moved(state)) => return Ok(MergeReport::of("moved", state)),
            Err(refusal) => return self.refused_logged(attempt, refusal),
        };
        let prepared = StoredMerge {
            state: MergeState::Prepared,
            expected_old: Some(target_tip),
            new_sha: Some(new_sha),
            ..attempt.clone() // clone: the prepared view of the same attempt
        };
        self.apply(lock, &prepared, subject, &decided).await
    }

    /// A refusal the gate already recorded (`superseded` in its own
    /// transaction): log it and report; a failure is returned.
    fn refused_logged(
        &self,
        attempt: &StoredMerge,
        refusal: ForgeError,
    ) -> Result<MergeReport, ForgeError> {
        if !is_refusal(&refusal) {
            return Err(refusal);
        }
        log_refusal(attempt, &refusal);
        Ok(MergeReport {
            outcome: "superseded",
            state: MergeState::Superseded,
            new_sha: None,
            error_code: Some(refusal.code()),
        })
    }

    /// The merge or squash commit: author / committer from the database.
    async fn write_commit(
        &self,
        attempt: &StoredMerge,
        subject: &Subject,
        tree: &ObjectId,
        target_tip: &ObjectId,
        timeout: Duration,
    ) -> Result<ObjectId, ForgeError> {
        let merger = self.identity(attempt.requested_by).await?;
        let (author, parents) = match attempt.method {
            MergeMethod::Squash => (
                self.identity(subject.mr.author).await?,
                vec![target_tip.clone()], // clone: the commit owns its parents
            ),
            MergeMethod::MergeCommit | MergeMethod::FastForward => (
                merger.clone(), // clone: author and committer are both the merger
                vec![target_tip.clone(), attempt.head_sha.clone()],
            ),
        };
        if author.fell_back() || merger.fell_back() {
            tracing::info!(
                target: "forge_review",
                event = "forge.review.merge_started",
                request_id = %attempt.request_id,
                merge_id = attempt.id,
                identity_fallback = true,
            );
        }
        // Frozen at the request (review round 1); an attempt stored without
        // one (none since 20261110100100's request change) gets the default.
        let message = match &attempt.message {
            Some(m) => m.clone(), // clone: the commit owns its message
            None => super::request::default_message(attempt.method, &subject.mr),
        };
        self.git
            .commit_tree(
                &self.storage,
                subject.repo,
                &NewCommit {
                    tree,
                    parents: &parents,
                    message: &message,
                    author: &author,
                    committer: &merger,
                },
                timeout,
            )
            .await
    }

    /// The commit identity of `user` from the database (the e-mail falls
    /// back to `<username>@users.noreply.invalid` when the account has none).
    async fn identity(&self, user: forge_core::UserId) -> Result<CommitIdentity, ForgeError> {
        let found = merges::account_identity(&self.db, user)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "merge_identity_missing")
                    .with_context("user_id", user.get())
            })?;
        let email = found
            .email
            .unwrap_or_else(|| format!("{}@users.noreply.invalid", found.username));
        CommitIdentity::new(&found.display_name, &email, &found.username)
            .map_err(|e| e.with_context("user_id", user.get()))
    }

    /// Steps 6–9 from a prepared attempt with both proofs: the fence, the
    /// compare-and-swap, the record.
    async fn apply(
        &self,
        lock: &mut MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        decided: &Decided,
    ) -> Result<MergeReport, ForgeError> {
        // Both proofs, for this attempt's merge request and method.
        if decided.proof.ctx().merge_request().iid != Some(subject.mr.iid)
            || decided.clearance.method() != attempt.method
        {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_proof_mismatch")
                .with_context("merge_id", attempt.id));
        }
        let (Some(_), Some(_)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(not_prepared(attempt));
        };
        // (a) the lock still stands on its own connection.
        if let Err(e) = lock.alive().await {
            log_retry(attempt, "lock_lost", &e);
            return Err(e);
        }
        // (b) prepared → executing, or someone else owns it.
        if !merges::fence_merge(&self.db, attempt.id).await? {
            let now = merges::merge_attempt(&self.db, attempt.id).await?;
            return Ok(MergeReport::of(
                "moved",
                now.map_or(MergeState::Superseded, |a| a.state),
            ));
        }
        self.update_ref(lock, attempt, subject, decided).await
    }

    /// From `executing` (a run that stopped after the fence): settle by
    /// asking git first; a target still at `expected_old` is moved again
    /// inside the window, after deciding again.
    async fn resume(
        &self,
        lock: &mut MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
    ) -> Result<MergeReport, ForgeError> {
        let (Some(old), Some(new)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(not_prepared(attempt));
        };
        let tip = self
            .git
            .read_ref(
                &self.storage,
                subject.repo,
                &attempt.target_branch,
                self.bounded(self.timeouts.read, lock)?,
            )
            .await?;
        if tip.as_ref() == Some(new) {
            // Moved by the stopped run: the cache may still hold the old tip.
            self.invalidate(subject.repo);
            return self.record(lock, attempt, subject, &[]).await;
        }
        if tip.as_ref() != Some(old) {
            // Moved elsewhere: only the reconciliation (after the window)
            // decides between `applied` and `failed`.
            return Ok(MergeReport::of(
                "left_for_reconciliation",
                MergeState::Executing,
            ));
        }
        let tips = self.tips(lock, attempt, subject.repo, &subject.mr).await?;
        let decided = match self
            .gate_facts(attempt, &tips, Some(Mergeable::Clean), ff_known(attempt))
            .await
        {
            Ok(d) => d,
            Err(refusal) => {
                return self
                    .refused(attempt, &[MergeState::Executing], refusal)
                    .await
            }
        };
        if let Err(e) = lock.alive().await {
            log_retry(attempt, "lock_lost", &e);
            return Err(e);
        }
        self.update_ref(lock, attempt, subject, &decided).await
    }

    /// (c) the window, then step 7 (compare-and-swap) and step 9. Takes both
    /// proofs (`decided`: the `Authorized` proof and the `MergeClearance`),
    /// so no path moves the ref without them (ADR-0024 §7).
    async fn update_ref(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        decided: &Decided,
    ) -> Result<MergeReport, ForgeError> {
        if decided.proof.ctx().merge_request().iid != Some(subject.mr.iid)
            || decided.clearance.method() != attempt.method
        {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_proof_mismatch")
                .with_context("merge_id", attempt.id));
        }
        let scopes = decided.scopes.as_slice();
        let (Some(old), Some(new)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(not_prepared(attempt));
        };
        let elapsed = merges::executing_elapsed(&self.db, attempt.id).await?;
        let Some(elapsed) = elapsed else {
            let now = merges::merge_attempt(&self.db, attempt.id).await?;
            return Ok(MergeReport::of(
                "moved",
                now.map_or(MergeState::Superseded, |a| a.state),
            ));
        };
        if elapsed > self.timeouts.update_ref_window() {
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.merge_retry",
                request_id = %attempt.request_id,
                merge_id = attempt.id,
                reason = "window_passed",
                elapsed_ms = elapsed.as_millis(),
                "the attempt is past its update-ref window: the reconciliation settles it"
            );
            return Ok(MergeReport::of(
                "left_for_reconciliation",
                MergeState::Executing,
            ));
        }
        let timeout = self.bounded(self.timeouts.update_ref, lock)?;
        if let Some(hook) = &self.before_update_ref {
            hook();
        }
        let change = RefChange::Update {
            name: attempt.target_branch.clone(), // clone: the change owns its name
            new: new.clone(),                    // clone: as above
            old: old.clone(),                    // clone: as above
        };
        let moved = self
            .git
            .update_refs(&self.storage, subject.repo, &[change], timeout)
            .await;
        // Every outcome may have changed the ref: drop the browse caches.
        self.invalidate(subject.repo);
        if let Err(cause) = moved {
            return self.classify(lock, attempt, subject, scopes, cause).await;
        }
        self.record(lock, attempt, subject, scopes).await
    }

    /// A failed compare-and-swap, told apart by reading the ref again
    /// (ADR-0024 §4.2 step 7).
    async fn classify(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        scopes: &[String],
        cause: ForgeError,
    ) -> Result<MergeReport, ForgeError> {
        let (Some(old), Some(new)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(not_prepared(attempt));
        };
        let tip = match self
            .git
            .read_ref(
                &self.storage,
                subject.repo,
                &attempt.target_branch,
                self.bounded(self.timeouts.read, lock)?,
            )
            .await
        {
            Ok(tip) => tip,
            Err(e) => {
                return Err(execution_failed("update_ref_failed", attempt, subject.repo)
                    .with_context("cause_code", cause.code().id())
                    .with_context("read_code", e.code().id())
                    .with_source(cause))
            }
        };
        if tip.as_ref() == Some(new) {
            // An earlier attempt of this run (or git despite the error) moved it.
            return self.record(lock, attempt, subject, scopes).await;
        }
        if tip.as_ref() != Some(old) {
            let refusal = head_changed(
                "target_moved",
                old.as_str(),
                tip.as_ref().map_or("", ObjectId::as_str),
            );
            return self
                .refused(attempt, &[MergeState::Executing], refusal)
                .await;
        }
        if self
            .git
            .ref_locked(&self.storage, subject.repo, &attempt.target_branch)?
        {
            let e = cause
                .with_context("reason", "ref_locked")
                .with_context("retryable", true);
            log_retry(attempt, "ref_locked", &e);
            return Err(e);
        }
        Err(execution_failed("update_ref_failed", attempt, subject.repo)
            .with_context("cause_code", cause.code().id())
            .with_source(cause))
    }

    /// Step 8 and 9: the applied line first, then tx2 (under the merge
    /// lock: the job's or the reconciliation's).
    pub(super) async fn record(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        scopes: &[String],
    ) -> Result<MergeReport, ForgeError> {
        let (Some(old), Some(new)) = (&attempt.expected_old, &attempt.new_sha) else {
            return Err(not_prepared(attempt));
        };
        tracing::info!(
            target: "forge_review",
            event = "forge.review.merge_applied",
            request_id = %attempt.request_id,
            merge_id = attempt.id,
            merge_request_id = attempt.merge_request.get(),
            project_id = attempt.project_id.get(),
            repo_id = subject.repo.get(),
            method = attempt.method.as_str(),
            target = attempt.target_branch.as_str(),
            expected_old = old.as_str(),
            new_sha = new.as_str(),
            head_sha = attempt.head_sha.as_str(),
        );
        let push = self.push_record(lock, attempt, subject, old, new).await;
        let pushers = self.merge_commit_pushers(lock, attempt, subject, old).await;
        let scopes = (!scopes.is_empty()).then_some(scopes);
        let ctx = requester_ctx(attempt, scopes);
        match finish_merge(&self.db, &ctx, attempt, &push, pushers.as_deref()).await {
            Ok(MergeFinished::Recorded(_)) => Ok(MergeReport {
                outcome: "applied",
                state: MergeState::Applied,
                new_sha: Some(new.clone()), // clone: the report owns it
                error_code: None,
            }),
            Ok(MergeFinished::AlreadyApplied) => Ok(MergeReport {
                outcome: "already_applied",
                state: MergeState::Applied,
                new_sha: Some(new.clone()), // clone: as above
                error_code: None,
            }),
            Ok(MergeFinished::NotExecuting(state)) => {
                Err(execution_failed("record_failed", attempt, subject.repo)
                    .with_context("state", state.as_str()))
            }
            // The attempt is `failed` now (forge-db, same transaction): the
            // alert reaches a person (runbook merge-execution).
            Ok(MergeFinished::MergeRequestNotOpen(state)) => {
                Err(
                    execution_failed("merge_request_not_open", attempt, subject.repo)
                        .with_context("merge_request_state", state.as_str()),
                )
            }
            Err(e) => Err(execution_failed("record_failed", attempt, subject.repo)
                .with_context("cause_code", e.code().id())
                .with_source(e)),
        }
    }

    /// The target's update as a push record (the new commits listed like a
    /// push's; a failed listing records none and says so). The ref has moved
    /// already, so the record is never refused for the lock's hold: the
    /// listing gets what is left of it, and none at all (`partial`, warn)
    /// when nothing is left (review round 2).
    async fn push_record(
        &self,
        lock: &MergeLock,
        attempt: &StoredMerge,
        subject: &Subject,
        old: &ObjectId,
        new: &ObjectId,
    ) -> PushRecord {
        let query = CommitQuery {
            name: attempt.target_branch.clone(), // clone: the query owns its name
            old: old.as_str().to_owned(),
            new: new.as_str().to_owned(),
        };
        let listing = self.timeouts.read.min(lock.left());
        let listed = if listing.is_zero() {
            Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "merge_lock_lost")
                .with_context("detail", "hold_exceeded"))
        } else {
            self.git
                .new_commits(
                    &self.storage,
                    subject.repo,
                    std::slice::from_ref(&query),
                    &format!("merge:{}", attempt.id),
                    listing,
                )
                .await
                .map(|mut all| all.pop())
        };
        let (commits, total, partial) = match listed {
            Ok(Some(Ok(list))) => {
                let commits: Vec<ObjectId> = list
                    .commits
                    .iter()
                    .filter_map(|c| ObjectId::parse(c).ok())
                    .collect();
                let dropped = list.commits.len() - commits.len();
                if dropped > 0 {
                    // git printed something that is no object id: the list is
                    // recorded without it, and says it is incomplete.
                    tracing::warn!(
                        target: "forge_review",
                        event = "forge.review.merge_commits_unlisted",
                        error_code = ErrorCode::GitProcessFailed.id(),
                        request_id = %attempt.request_id,
                        merge_id = attempt.id,
                        dropped,
                        "commit ids the record could not read were left out"
                    );
                }
                let partial = list.partial || dropped > 0;
                (commits, list.total, partial)
            }
            Ok(Some(Err(e))) | Err(e) => {
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_commits_unlisted",
                    error_code = e.code().id(),
                    request_id = %attempt.request_id,
                    merge_id = attempt.id,
                    "the merge is recorded without its commit list"
                );
                (Vec::new(), 0, true)
            }
            Ok(None) => (Vec::new(), 0, true),
        };
        let total = total.max(commits.len() as u64);
        PushRecord {
            repository_id: subject.repo,
            updates: vec![PushedRef {
                name: attempt.target_branch.clone(), // clone: the record owns its name
                old: old.clone(),                    // clone: as above
                new: new.clone(),                    // clone: as above
                kind: PushUpdateKind::FastForward,
                commits,
                total_commits: total,
                total_commits_partial: partial,
            }],
        }
    }
}

/// An applied attempt that asked for the source branch's deletion and has
/// no outcome yet.
pub(super) fn wants_source_deletion(attempt: &StoredMerge) -> bool {
    attempt.state == MergeState::Applied
        && attempt.delete_source_branch
        && attempt.source_outcome.is_none()
}

/// Whether a fast-forward is known possible for a prepared attempt: its
/// `new_sha` is the head itself (a merge commit or squash never needs it).
fn ff_known(attempt: &StoredMerge) -> Option<bool> {
    match attempt.method {
        MergeMethod::FastForward => Some(attempt.new_sha.as_ref() == Some(&attempt.head_sha)),
        MergeMethod::MergeCommit | MergeMethod::Squash => None,
    }
}

fn log_retry(attempt: &StoredMerge, reason: &'static str, e: &ForgeError) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.review.merge_retry",
        error_code = e.code().id(),
        request_id = %attempt.request_id,
        merge_id = attempt.id,
        reason,
        "the merge is retried"
    );
}

fn log_refusal(attempt: &StoredMerge, refusal: &ForgeError) {
    let context: Vec<String> = refusal
        .context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    tracing::info!(
        target: "forge_review",
        event = "forge.review.merge_completed",
        outcome = "superseded",
        error_code = refusal.code().id(),
        request_id = %attempt.request_id,
        merge_id = attempt.id,
        merge_request_id = attempt.merge_request.get(),
        head_sha = attempt.head_sha.as_str(),
        context = %context.join(" "),
    );
}

fn log_completed(attempt: &StoredMerge, report: &MergeReport) {
    if report.outcome == "superseded" {
        return; // logged with its refusal
    }
    tracing::info!(
        target: "forge_review",
        event = "forge.review.merge_completed",
        outcome = report.outcome,
        state = report.state.as_str(),
        request_id = %attempt.request_id,
        merge_id = attempt.id,
        merge_request_id = attempt.merge_request.get(),
        new_sha = report.new_sha.as_ref().map(ObjectId::as_str),
    );
}

/// A decision (a 4xx: the requester's rights, the merge request's state,
/// a conflict, a moved head or target) rather than a failure (git, the
/// database, an invariant: 5xx).
pub(super) fn is_refusal(e: &ForgeError) -> bool {
    e.code().http_status() < 500
}
