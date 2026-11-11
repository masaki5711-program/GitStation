//! forge-review: merge requests (M3 R1a, ADR-0024). A feature crate
//! (ADR-0024 §11): forge-web / forge-cli → forge-review → forge-auth /
//! forge-git / forge-db; it runs no SQL of its own (the SQL is
//! `forge_db::review`) and depends on no other feature crate.
//!
//! **Every command is decided twice by the one decision point, and the
//! second decision binds the write** (as forge-hosting): the route registry
//! proves the action on what its loader read; the command takes that
//! `Authorized<MergeRequestCtx>`, opens the write transaction through a
//! forge-db review command, which locks the project row, the merge request
//! row and the caller's roles and account, and calls `decide` — here
//! [`forge_auth::Authorized::reconfirm`] on those facts. A role removed, a
//! demotion or a disable committed in between is seen (denied, nothing
//! written). A proof for another action is `FG-SYS-0001
//! reason=proof_action_mismatch`; a create's proof (no iid) driving an
//! update, or the reverse, is `proof_shape_mismatch`.
//!
//! Before the transaction, a create checks both branches exist (and a
//! retarget the new target, a reopen both) through [`BranchProbe`] — in
//! `forge serve` the browse API's ref list (`forge_git::browse::Browser`,
//! no new git plumbing): `FG-REPO-0023 reason=ref_not_found` with
//! `branch=source_branch|target_branch`. A branch deleted later leaves the
//! merge request in place (ADR-0024 §1).

use std::sync::Arc;

use forge_auth::{
    AccountFacts, Action, Authorized, Credential, LoadedMergeRequest, LoadedProject,
    MergeRequestCtx, Principal,
};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    MergeRequestState, ObjectId, ProjectId, ProjectRepository, RefName, RepoId, RepositoryState,
    RequestId, UserId,
};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::ProjectAccessFacts;
use forge_db::review::{self, MergeRequestFacts, MergeRequestPatch, NewMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_db::review::{
    ApproveOutcome, ApproverNames, RuleOutcome, StoredApproval, StoredApprovalRule,
};
pub use forge_db::review::{EditOutcome, StateOutcome, StoredMergeRequest, UserName, MAX_PAGE};

mod after_version;
pub mod approvals;
pub mod conflicts;
pub mod discussions;
mod fields;
mod merge_conditions;
pub mod mergeability;
pub mod merges;
pub mod outdated;
pub mod position;
mod purge;
mod version_refs;
pub mod versions;
mod versions_carry;
pub mod work_items;

#[cfg(test)]
mod change_context_tests;

pub use conflicts::{scan, ConflictPredictor, ScanLimits, ScanReport};
pub use forge_db::review::settings::loosens as settings_loosen_approvals;
pub use forge_db::review::{
    CodeownersEnforcement, PurgeOutcome, RefreshRequest, SettingsOutcome, StoredMergeSettings,
    StoredMergeability, StoredVersion,
};
pub use forge_db::review::{StoredPrediction, StoredScan};
pub use merge_conditions::MergeStatus;
pub use mergeability::{check, prune_merge_results, CheckReport, Mergeability, PruneReport};
pub use merges::{
    evaluate_merge, BlockReason, CredentialRef, MergeAsk, MergeClearance, MergeReport,
    MergeSettings, MergeTimeouts, Merger, ReconcileReport, StoredMerge,
};
pub use versions::{refresh, RefreshReport, SyncReport, VersionRefs};

pub use discussions::DiffProbe;
pub use fields::{
    check_description, check_note_body, check_position_shape, check_title, MAX_DESCRIPTION_BYTES,
    MAX_NOTE_BYTES, MAX_PATH_BYTES, MAX_TITLE_CHARS,
};
pub use forge_db::review::{DiscussionPosition, ResolveOutcome, StoredDiscussion, StoredNote};
pub use outdated::{DiscussionList, DiscussionView, Outdated, MAX_DIFFS_PER_READ, OUTDATED_BUDGET};

/// A branch's head commit in a repository, for the commands that name one
/// (create, retarget, reopen — M3 R1b: the reopen also compares it with
/// the purged versions). `Ok(head)` when the branch exists; `FG-REPO-0023
/// reason=ref_not_found` when it does not; the lookup's own error
/// (`FG-GIT-0002` / `FG-GIT-0004`, …) otherwise — never "exists" on a
/// failure.
pub trait BranchProbe: Send + Sync {
    /// `caller` is the caller's key (`user:<id>`) for the browse limits.
    fn branch_head<'a>(
        &'a self,
        repo: RepoId,
        branch: &'a RefName,
        caller: &'a str,
    ) -> LookupFuture<'a, ObjectId>;
}

/// The browse API's ref resolution (`resolve_ref` on the full
/// `refs/heads/…` name: only a branch of that exact name answers).
impl BranchProbe for forge_git::browse::Browser {
    fn branch_head<'a>(
        &'a self,
        repo: RepoId,
        branch: &'a RefName,
        caller: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        Box::pin(async move {
            let resolved = self.resolve_ref(repo, branch.as_str(), caller).await?;
            ObjectId::parse(&resolved.commit).map_err(|_| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "resolved_commit_invalid")
                    .with_context("repo_id", repo.get())
            })
        })
    }
}

/// The request a write belongs to (the audit row's request id and token).
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub request_id: RequestId,
    /// The personal access token the request authenticated with (`None`
    /// for a session).
    pub credential_id: Option<i64>,
}

/// A new merge request's free-text fields (the branches and the draft flag
/// are the proof's).
#[derive(Debug, Clone)]
pub struct NewFields {
    pub title: String,
    pub description: String,
}

/// An edit's free-text fields (`None` = unchanged; the draft flag and the
/// target are the proof's).
#[derive(Debug, Clone, Default)]
pub struct EditFields {
    pub title: Option<String>,
    pub description: Option<String>,
}

/// The merge request commands on the database.
#[derive(Clone)]
pub struct Review {
    db: Db,
    branches: Arc<dyn BranchProbe>,
    /// M3 R1b: where the purge deletes version refs (`None`: the purge is
    /// `FG-SYS-0001 reason=version_refs_unconfigured`).
    refs: Option<VersionRefs>,
    /// M3 R3: where inline positions and `outdated` read diffs (`None`:
    /// `FG-SYS-0001 reason=diffs_unconfigured` when one is needed).
    diffs: Option<Arc<dyn DiffProbe>>,
    /// M3 R3 re-review: the time one read (or a write's answer) may spend
    /// on `outdated` diffs (`outdated::OUTDATED_BUDGET` unless set).
    outdated_budget: std::time::Duration,
    /// M3 R5: where the approval evaluation reads git (`None`: approving
    /// and merging are `FG-SYS-0001 reason=review_git_unconfigured`).
    review_git: Option<approvals::ReviewGit>,
    /// M3 C1: where the merge status reads the head's check runs
    /// (only `forge serve` injects the `Db`; the database the
    /// commands use unless set). The request and the job read them in the
    /// gate's transaction instead.
    checks: Arc<dyn forge_core::CheckResultsSource>,
}

impl std::fmt::Debug for Review {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Review").finish_non_exhaustive()
    }
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

/// `FG-SYS-0001 reason=proof_shape_mismatch`: a create's proof (no iid)
/// driving a change of a stored merge request, or the reverse.
fn shape_mismatch(command: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_shape_mismatch")
        .with_context("command", command)
}

/// The caller the proof was decided for (a non-public action has one).
fn caller(proof: &Authorized<MergeRequestCtx>) -> Result<UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}

/// The change context of a write by `principal`: actor `user`, or
/// `service_account` when the caller's account is one (S0 review: a service
/// account's comment is audited as its own), the request id, the token and
/// its scopes.
///
/// M3 S2 (ADR-0025 §6): a service account acting for a person is audited
/// as `user_via_service_account` — the person, the service account, its
/// token, the keyed HMAC of the claimed person and the assertion's public
/// id; the effective scope is its token's.
///
/// A CI runner or job never reaches a review write (`authorize()` refuses
/// it every review action, ADR-0029 §8); if one did, it is a defect:
/// `FG-SYS-0001 reason=ci_principal_in_review`, never audited as a user.
fn change_context(
    request: &ReviewRequest,
    principal: &Principal,
    user: UserId,
) -> Result<ChangeContext, ForgeError> {
    let actor = match (principal.credential(), principal.account_kind()) {
        (Credential::ActAs(c), _) => Actor::UserViaServiceAccount {
            user,
            via: c.via,
            credential_id: c.via_token,
            act_as_claimed: c.claimed_mac.clone(), // clone: the audit row owns it
            act_as_key_id: c.claimed_key_id,
            via_assertion_id: c.assertion.as_ref().map(|a| a.public_id.clone()),
        },
        (Credential::Session | Credential::Token(_), forge_auth::AccountKind::ServiceAccount) => {
            Actor::ServiceAccount(user)
        }
        (
            Credential::Session | Credential::Token(_),
            forge_auth::AccountKind::Human | forge_auth::AccountKind::Bot,
        ) => Actor::User(user),
        (Credential::Runner(_) | Credential::Job(_), _) => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "ci_principal_in_review"))
        }
    };
    let mut ctx = ChangeContext::new(actor, request.request_id.clone());
    ctx.credential_id = match principal.act_as_credential() {
        Some(c) => Some(c.via_token),
        None => request.credential_id,
    };
    ctx.effective_scope = principal.scope_names();
    Ok(ctx)
}

/// The assertion an act-as change consumes (`None` otherwise).
fn act_as_use(principal: &Principal) -> Option<forge_db::act_as::ActAsUse> {
    let c = principal.act_as_credential()?;
    let a = c.assertion.as_ref()?;
    Some(forge_db::act_as::ActAsUse {
        assertion: a.id,
        public_id: a.public_id.clone(), // clone: the use owns it
        integration: c.integration,
        via: c.via,
    })
}

/// The reconfirmation's context under the lock (M3 S2, ADR-0025 §5.3):
/// the act-as facts read in the transaction (`facts.via`) and the merge
/// request as locked, with the target's tip read before it and the sha
/// the request named; the plain context for every other principal.
fn fresh_ctx(
    principal: &Principal,
    facts: &MergeRequestFacts,
    tip: Option<&ObjectId>,
    requested: Option<&ObjectId>,
) -> Result<MergeRequestCtx, ForgeError> {
    let plain = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
    if principal.act_as_credential().is_none() {
        return Ok(plain);
    }
    let (Some(via), Some(stored)) = (facts.via, facts.merge_request.as_ref()) else {
        // An act-as proof reconfirmed without the facts: refused by the
        // act-as arm (`act_as_facts_missing`), never decided as if the
        // service account were the person.
        return Ok(plain);
    };
    Ok(plain.acting(
        forge_auth::ViaView {
            role: via.role,
            bound: via.bound,
        },
        Some(forge_auth::ActAsSubject {
            project: stored.project_id,
            merge_request: stored.id,
            head_sha: stored.head_sha.clone(), // clone: the subject owns its values
            target_ref: stored.target_branch.clone(),
            target_sha: tip.map(|t| t.as_str().to_owned()),
            requested_sha: requested.map(|s| s.as_str().to_owned()),
        }),
    ))
}

fn account(a: Account) -> AccountFacts {
    AccountFacts {
        active: a.active,
        is_admin: a.is_admin,
    }
}

/// forge-auth's loaded project from the transaction's facts.
fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

/// forge-auth's loaded merge request from a stored row (the route loaders
/// and the commands build it the same way).
pub fn loaded_merge_request(mr: &StoredMergeRequest) -> LoadedMergeRequest {
    LoadedMergeRequest {
        iid: Some(mr.iid),
        author: mr.author,
        state: mr.state,
        draft: mr.draft,
        source_branch: mr.source_branch.clone(), // clone: the context owns its names
        target_branch: mr.target_branch.clone(),
    }
}

/// `FG-REPO-0005 resource=merge_request`: the row went away between the
/// proof and the lock (never in R1a: only the purge deletes rows).
fn absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "merge_request")
        .with_context("reason", "absent")
}

/// The repository must exist on disk to look a branch up (`FG-REPO-0016`).
fn ready(project: ProjectId, repo: ProjectRepository) -> Result<RepoId, ForgeError> {
    if repo.state == RepositoryState::Ready {
        return Ok(repo.id);
    }
    Err(ForgeError::new(ErrorCode::RepositoryNotReady)
        .with_context("repo_id", repo.id.get())
        .with_context("project_id", project.get())
        .with_context("state", repo.state.as_str()))
}

impl Review {
    pub fn new(db: Db, branches: Arc<dyn BranchProbe>) -> Self {
        Self {
            branches,
            refs: None,
            diffs: None,
            outdated_budget: outdated::OUTDATED_BUDGET,
            review_git: None,
            checks: Arc::new(db.clone()), // clone: the source shares the pool
            db,
        }
    }

    /// Where the merge status reads check runs (M3 C1; only `forge serve`
    /// passes the `Db` — `forge worker` builds no `Review` —; tests pass a
    /// spy or a failing source).
    #[must_use]
    pub fn with_check_results(mut self, checks: Arc<dyn forge_core::CheckResultsSource>) -> Self {
        self.checks = checks;
        self
    }

    /// Where the hidden version refs live (M3 R1b; `forge serve` sets it).
    #[must_use]
    pub fn with_version_refs(mut self, refs: VersionRefs) -> Self {
        self.refs = Some(refs);
        self
    }

    /// Each of `names` exists in `repo` (`which` names it in the error);
    /// their heads, in order.
    async fn require_branches(
        &self,
        project: ProjectId,
        repo: ProjectRepository,
        user: UserId,
        names: &[(&'static str, &RefName)],
    ) -> Result<Vec<ObjectId>, ForgeError> {
        let id = ready(project, repo)?;
        let key = format!("user:{}", user.get());
        let mut heads = Vec::with_capacity(names.len());
        for (which, name) in names {
            heads.push(
                self.branches
                    .branch_head(id, name, &key)
                    .await
                    .map_err(|e| e.with_context("branch", *which))?,
            );
        }
        Ok(heads)
    }

    // ── reads for the route loaders and the list (read pool) ──

    /// Merge request `iid` of `project` (`None` = absent).
    pub async fn merge_request(
        &self,
        project: ProjectId,
        iid: i64,
    ) -> Result<Option<StoredMergeRequest>, ForgeError> {
        review::get(&self.db, project, iid).await
    }

    /// One page of `project`'s merge requests, newest first.
    pub async fn merge_requests(
        &self,
        project: ProjectId,
        state: Option<MergeRequestState>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMergeRequest>, ForgeError> {
        review::list(&self.db, project, state, before, limit).await
    }

    /// Every version of merge request `mr`, oldest first (M3 R2; the
    /// caller decided `MergeRequestRead` on it).
    pub async fn versions(
        &self,
        mr: forge_core::MergeRequestId,
    ) -> Result<Vec<review::StoredVersion>, ForgeError> {
        review::read_versions(&self.db, mr).await
    }

    /// The stored mergeability of merge request `mr` (`None`: never
    /// requested — shown as `unchecked`; M3 R2). Never enqueues a check:
    /// reading writes nothing (ADR-0024 R2 decision 6).
    pub async fn mergeability(
        &self,
        mr: forge_core::MergeRequestId,
    ) -> Result<Option<review::StoredMergeability>, ForgeError> {
        review::mergeability(&self.db, mr).await
    }

    /// The stored conflict scan of merge request `mr` and its current
    /// `conflict` / `failed` predictions (M3 X6; both heads current, both
    /// open, on the same target). Never enqueues a scan.
    pub async fn conflicts(
        &self,
        mr: forge_core::MergeRequestId,
    ) -> Result<(Option<StoredScan>, Vec<StoredPrediction>), ForgeError> {
        let scan = review::conflict_scan(&self.db, mr).await?;
        let predictions = review::conflict_predictions(&self.db, mr).await?;
        Ok((scan, predictions))
    }

    // ── writes, each decided again inside its transaction ──

    /// `POST /api/v1/projects/{id}/merge-requests` (`MergeRequestCreate`):
    /// the branches and draft flag the proof was decided for, by the
    /// proof's caller. The fields are checked first (`FG-WEB-0006`), then
    /// the branches' existence (`FG-REPO-0016`, `FG-REPO-0023`), then the
    /// transaction (`FG-REVIEW-0001`, the reconfirmed decision).
    pub async fn open(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        fields: &NewFields,
    ) -> Result<StoredMergeRequest, ForgeError> {
        require(proof.action(), Action::MergeRequestCreate)?;
        let (project, None) = *proof.id() else {
            return Err(shape_mismatch("merge_request_open"));
        };
        check_title(&fields.title)?;
        check_description(&fields.description)?;
        let user = caller(proof)?;
        let wanted = proof.ctx().merge_request();
        self.require_branches(
            project,
            repo,
            user,
            &[
                ("source_branch", &wanted.source_branch),
                ("target_branch", &wanted.target_branch),
            ],
        )
        .await?;
        let ctx = change_context(request, proof.principal(), user)?;
        let new = NewMergeRequest {
            title: fields.title.clone(), // clone: forge-db owns its request
            description: fields.description.clone(), // clone: forge-db owns its request
            source_branch: wanted.source_branch.clone(), // clone: forge-db owns its request
            target_branch: wanted.target_branch.clone(), // clone: forge-db owns its request
            draft: wanted.draft,
        };
        let (stored, ()) = review::open_mr_decided(&self.db, &ctx, project, user, &new, |facts| {
            let fresh = MergeRequestCtx::new(loaded(&facts.project), wanted.clone()); // clone: the fresh context owns it
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        Ok(stored)
    }

    /// `PATCH /api/v1/projects/{id}/merge-requests/{iid}`
    /// (`MergeRequestUpdate`): the free-text `fields`, and the draft flag
    /// and target the proof was decided for. A new target must exist; it is
    /// checked whenever one is asked for — not only when the proof's view of
    /// the current target differs — so a concurrent retarget can never let
    /// an unchecked branch through. Returns the outcome and the row as it
    /// stands after the transaction.
    pub async fn edit(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
        fields: &EditFields,
    ) -> Result<(EditOutcome, StoredMergeRequest), ForgeError> {
        require(proof.action(), Action::MergeRequestUpdate)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_edit"));
        };
        if let Some(title) = &fields.title {
            check_title(title)?;
        }
        if let Some(description) = &fields.description {
            check_description(description)?;
        }
        let user = caller(proof)?;
        let edit = proof.ctx().edit();
        if let Some(target) = &edit.target_branch {
            self.require_branches(project, repo, user, &[("target_branch", target)])
                .await?;
        }
        let ctx = change_context(request, proof.principal(), user)?;
        let patch = MergeRequestPatch {
            title: fields.title.clone(), // clone: forge-db owns its request
            description: fields.description.clone(), // clone: forge-db owns its request
            draft: edit.draft,
            target_branch: edit.target_branch.clone(), // clone: forge-db owns its request
        };
        let (outcome, stored, ()) =
            review::edit_mr_decided(&self.db, &ctx, project, iid, user, &patch, |facts| {
                let mr = locked(facts)?;
                let fresh = MergeRequestCtx::edited(loaded(&facts.project), mr, edit.clone()); // clone: the fresh context owns it
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok((outcome, stored))
    }

    /// `POST …/merge-requests/{iid}/close` (`MergeRequestClose`): the
    /// outcome and the row after the transaction.
    pub async fn close(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
    ) -> Result<(StateOutcome, StoredMergeRequest), ForgeError> {
        require(proof.action(), Action::MergeRequestClose)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_close"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        let (outcome, stored, ()) =
            review::close_mr_decided(&self.db, &ctx, project, iid, user, |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), locked(facts)?);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok((outcome, stored))
    }

    /// `POST …/merge-requests/{iid}/reopen` (`MergeRequestReopen`): both
    /// branches must still exist (a merge request whose branch was deleted
    /// stays closed). The branches are checked from the row as read just
    /// before the transaction, and the transaction refuses to go on when
    /// the row it locks is closed with branches other than the ones checked
    /// (or was not closed when read): it re-reads and checks again, at most
    /// [`STALE_ATTEMPTS`] times, then `FG-DB-0002
    /// reason=merge_request_changed_concurrently` (retryable). A concurrent
    /// close or retarget therefore never skips the check.
    pub async fn reopen(
        &self,
        request: &ReviewRequest,
        proof: &Authorized<MergeRequestCtx>,
        repo: ProjectRepository,
    ) -> Result<(StateOutcome, StoredMergeRequest), ForgeError> {
        require(proof.action(), Action::MergeRequestReopen)?;
        let (project, Some(iid)) = *proof.id() else {
            return Err(shape_mismatch("merge_request_reopen"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user)?;
        for _ in 0..STALE_ATTEMPTS {
            let current = self.merge_request(project, iid).await?.ok_or_else(absent)?;
            // The branches of a closed merge request, checked to exist; the
            // source's head is compared with the purged versions (R1b).
            let (checked, source_head) = if current.state == MergeRequestState::Closed {
                let heads = self
                    .require_branches(
                        project,
                        repo,
                        user,
                        &[
                            ("source_branch", &current.source_branch),
                            ("target_branch", &current.target_branch),
                        ],
                    )
                    .await?;
                (
                    Some((current.source_branch, current.target_branch)),
                    heads.into_iter().next(),
                )
            } else {
                (None, None)
            };
            let attempt = review::reopen_mr_decided(
                &self.db,
                &ctx,
                project,
                iid,
                user,
                source_head.as_ref(),
                |facts| {
                    let mr = locked(facts)?;
                    let unchecked = mr.state == MergeRequestState::Closed
                        && checked.as_ref().is_none_or(|(source, target)| {
                            *source != mr.source_branch || *target != mr.target_branch
                        });
                    if unchecked {
                        return Err(changed_concurrently(iid));
                    }
                    let fresh = MergeRequestCtx::new(loaded(&facts.project), mr);
                    proof.reconfirm(account(facts.account), fresh).map(drop)
                },
            )
            .await;
            match attempt {
                Err(e) if is_changed_concurrently(&e) => continue,
                Err(e) => return Err(e),
                Ok((outcome, stored, ())) => return Ok((outcome, stored)),
            }
        }
        Err(changed_concurrently(iid))
    }
}

/// How often a reopen re-reads and re-checks a row that changed under it.
pub const STALE_ATTEMPTS: usize = 3;

/// `FG-DB-0002 reason=merge_request_changed_concurrently` (retryable): the
/// row changed between the branch check and the lock.
fn changed_concurrently(iid: i64) -> ForgeError {
    ForgeError::new(ErrorCode::DbQueryFailed)
        .with_context("reason", "merge_request_changed_concurrently")
        .with_context("retryable", true)
        .with_context("iid", iid)
}

fn is_changed_concurrently(e: &ForgeError) -> bool {
    e.code() == ErrorCode::DbQueryFailed
        && e.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == "merge_request_changed_concurrently")
}

/// The merge request as the transaction locked it.
fn locked(facts: &MergeRequestFacts) -> Result<LoadedMergeRequest, ForgeError> {
    facts
        .merge_request
        .as_ref()
        .map(loaded_merge_request)
        .ok_or_else(absent)
}
