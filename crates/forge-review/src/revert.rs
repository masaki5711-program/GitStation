//! One-step revert of a merged merge request (M4 C10, plan 04 M-4,
//! ADR-0024 Amendment C10).
//!
//! One action opens a merge request that reverts ONE merge: its source is
//! the new branch `revert/mr-<iid>` holding ONE commit on the target's tip,
//! its target the merged merge request's target. The commit's tree is
//! what `git revert` leaves (`-m 1` for a merge commit; the squash commit;
//! a fast-forward of several commits at once): computed without a work tree
//! by [`forge_git::revert`] from the applied merge attempt's `expected_old`
//! (the tip the merge was built on) and `new_sha` (what the merge made).
//! The revert merge request then follows the normal rules: its pipeline,
//! approvals and merge (auto-merge only where the project's `merge.auto`
//! allows it).
//!
//! **Who may.** The person's revert (`POST …/merge-requests/{iid}/revert`,
//! `gstn mr revert`, the button on a merged merge request) is decided as
//! what it does, with the existing actions: `MergeRequestCreate` of the
//! revert merge request ([`revert_ctx`]: the caller's own, `revert/mr-<iid>`
//! → the target), proved by the route and re-decided under the review locks
//! ([`Authorized::reconfirm`]), and the branch's creation as a push
//! (`RepoPush` + `authorize_ref_updates` on the push path's protection
//! rules: a protected `revert/*` refuses as a push would). Both are decided
//! before git writes anything but unreachable objects.
//!
//! **The system's draft** ([`Reverter::draft_after_failure`], the job
//! `merge_request.revert_draft` of `forge serve`): when the target
//! branch's push pipeline fails on the commit a merge produced, and the
//! target's pipeline before that merge did not fail, the system opens the
//! same revert as a DRAFT, authored by the reverted merge request's author
//! and decided exactly as that author's revert (their current role; never
//! more than they could do themselves), audited as actor `system`. It
//! records `merge_request.revert_drafted` (opened, or conflicted), which
//! tells the author (notification kind `mr_revert_drafted`) and kanflow
//! (integration class). A draft never merges until a person marks it
//! ready.
//!
//! **Conflicts** (decided in ADR-0024 Amendment C10): a revert that
//! conflicts opens NOTHING — no branch, no merge request, no commit on a
//! ref — and answers `FG-REVIEW-0030` (the person's) or records
//! `outcome=conflicted` (the system's). forge never commits conflict
//! markers. Every other refusal is `FG-REVIEW-0029` with its `reason`.
//!
//! If the merge request cannot be opened after the branch was created (a
//! refusal under the lock, the database), the branch stays and the error is
//! returned: a second revert then answers `branch_exists`, and a merge
//! request can be opened from the branch by hand.

use std::time::Duration;

use forge_auth::{
    authorize_ref_updates, prove, AccountKind, Action, Authorized, Credential, LoadedMergeRequest,
    LoadedProject, MergeRequestCtx, Principal, ProjectCtx, RefUpdate, RefUpdateCtx,
};
use forge_core::{
    telemetry, MergeRequestChange, MergeRequestState, MergeState, ObjectId, ProjectAccessLoader,
    ProjectId, PushRecord, PushUpdateKind, PushedCommits, PushedRef, RefName, RepoId, RequestId,
    RevertDraftOutcome, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::reverts::{self, applied_merge_at, push_pipeline_status};
use forge_db::review::{self, NewMergeRequest, StoredMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_git::commit::{CommitIdentity, NewCommit};
use forge_git::mergeability::MergeTreeLimits;
use forge_git::revert::{RevertInputs, RevertTree};
use forge_git::{Git, RefChange, StorageRoot};

use crate::fields::MAX_TITLE_CHARS;
use crate::merges::{account_commit_identity, StoredMerge};
use crate::{
    account, caller, change_context, loaded, ready, require, shape_mismatch, ReviewRequest,
};

/// The audit label of the system's revert draft.
pub const REVERT_ACTOR_LABEL: &str = "forge-revert";

/// The identity of a commit the system writes for its draft (never a
/// person: the author is told, the commit is not theirs).
const SYSTEM_IDENTITY: (&str, &str, &str) =
    ("GitStation", "revert@gitstation.invalid", "gitstation");

/// The bounds of one revert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevertTimeouts {
    /// One ref read or tree read.
    pub read: Duration,
    /// The revert's `merge-tree` (and each construction commit).
    pub merge_tree: Duration,
    /// The commit and the branch's `update-ref`.
    pub write: Duration,
}

impl Default for RevertTimeouts {
    fn default() -> Self {
        Self {
            read: Duration::from_secs(10),
            merge_tree: MergeTreeLimits::default().timeout,
            write: Duration::from_secs(20),
        }
    }
}

/// `refs/heads/revert/mr-<iid>`: the revert branch of merge request `iid`.
pub fn revert_branch(iid: i64) -> Result<RefName, ForgeError> {
    RefName::parse(&format!("refs/heads/revert/mr-{iid}")).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "revert_branch_invalid")
            .with_context("iid", iid)
    })
}

/// The `MergeRequestCreate` context of `author`'s revert of `reverted`
/// on `project` (the route's loader, the status's viewer flag and the
/// system's draft build it the same way).
///
/// `(iid, target)`: the reverted merge request's number and target branch.
pub fn revert_ctx(
    project: LoadedProject,
    (iid, target): (i64, &RefName),
    author: UserId,
    draft: bool,
) -> Result<MergeRequestCtx, ForgeError> {
    Ok(MergeRequestCtx::new(
        project,
        LoadedMergeRequest {
            iid: None,
            author,
            state: MergeRequestState::Open,
            draft,
            source_branch: revert_branch(iid)?,
            target_branch: target.clone(), // clone: the context owns its names
        },
    ))
}

/// `FG-REVIEW-0029` with `reason`.
fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RevertRefused).with_context("reason", reason)
}

/// The revert's title: `Revert "<title>"`, cut to the title limit.
fn title_of(reverted: &StoredMergeRequest) -> String {
    let full = format!("Revert \"{}\"", reverted.title);
    if full.chars().count() <= MAX_TITLE_CHARS {
        return full;
    }
    let mut cut: String = full.chars().take(MAX_TITLE_CHARS - 1).collect();
    cut.push('…');
    cut
}

fn description_of(reverted: &StoredMergeRequest, merge: &StoredMerge, merged: &ObjectId) -> String {
    format!(
        "Reverts !{} ({}, `{}`).",
        reverted.iid,
        merge.method.as_str(),
        merged.as_str()
    )
}

fn message_of(reverted: &StoredMergeRequest, merged: &ObjectId) -> String {
    format!(
        "{}\n\nThis reverts merge request !{}.\nThis reverts commit {}.\n",
        title_of(reverted),
        reverted.iid,
        merged.as_str()
    )
}

/// What [`Reverter::revert`] opened.
#[derive(Debug, Clone)]
pub struct RevertOpened {
    /// The revert merge request.
    pub merge_request: StoredMergeRequest,
    /// The merge request it reverts.
    pub reverted_iid: i64,
    /// The revert commit (the new branch's tip).
    pub commit: ObjectId,
    /// The commit it was built on (the target's tip then).
    pub parent: ObjectId,
}

/// What [`Reverter::draft_after_failure`] did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftReport {
    /// A draft revert merge request `iid` was opened.
    Opened { iid: i64 },
    /// The revert conflicts: nothing was opened (recorded and told).
    Conflicted,
    /// Nothing to do, and why (`no_merge`, `not_target_pipeline`,
    /// `target_already_failing`, `not_merged`, `branch_exists`,
    /// `nothing_to_revert`, `target_missing`, `author_not_allowed`).
    Skipped(&'static str),
}

/// The applied merge of a merged merge request and the facts a revert is
/// built from.
struct Plan {
    repo: RepoId,
    reverted: StoredMergeRequest,
    merge: StoredMerge,
    merged: ObjectId,
    before: ObjectId,
    branch: RefName,
}

/// The tree the revert commit gets, on `parent`.
struct Built {
    tree: ObjectId,
    parent: ObjectId,
}

/// Builds revert merge requests (module docs): the database, git, the
/// storage root, the browse cache every ref write invalidates, the bounds.
#[derive(Clone)]
pub struct Reverter {
    db: Db,
    git: Git,
    storage: StorageRoot,
    browser: Option<Browser>,
    timeouts: RevertTimeouts,
}

impl std::fmt::Debug for Reverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reverter")
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Reverter {
    pub fn new(db: Db, git: Git, storage: StorageRoot) -> Self {
        Self {
            db,
            git,
            storage,
            browser: None,
            timeouts: RevertTimeouts::default(),
        }
    }

    /// The browse cache every ref write invalidates (`forge serve`).
    #[must_use]
    pub fn with_browser(mut self, browser: Browser) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Other bounds (tests).
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: RevertTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// The person's revert of merge request `reverted_iid` (module docs).
    /// `create` is the route's `MergeRequestCreate` proof of the revert
    /// merge request ([`revert_ctx`]); `push` its `RepoPush` proof on the
    /// project (the branch's creation is decided with it).
    pub async fn revert(
        &self,
        request: &ReviewRequest,
        create: &Authorized<MergeRequestCtx>,
        push: &Authorized<ProjectCtx>,
        repo: forge_core::ProjectRepository,
        reverted_iid: i64,
    ) -> Result<RevertOpened, ForgeError> {
        require(create.action(), Action::MergeRequestCreate)?;
        let (project, None) = *create.id() else {
            return Err(shape_mismatch("merge_request_revert"));
        };
        if *push.id() != project {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "proof_project_mismatch"));
        }
        let user = caller(create)?;
        let fail = |e: ForgeError| {
            e.with_context("project_id", project.get())
                .with_context("iid", reverted_iid)
        };
        let reverted = review::get(&self.db, project, reverted_iid)
            .await?
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "merge_request")
                    .with_context("reason", "absent")
            })?;
        let wanted = create.ctx().merge_request();
        if wanted.source_branch != revert_branch(reverted.iid)?
            || wanted.target_branch != reverted.target_branch
            || wanted.draft
        {
            return Err(shape_mismatch("merge_request_revert"));
        }
        let plan = self
            .plan(ready(project, repo)?, reverted)
            .await
            .map_err(|e| self.refusal_logged(fail(e), &request.request_id))?;
        // The branch's creation, decided like a push before git writes it.
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let update = RefUpdateCtx::new(plan.branch.clone(), RefUpdate::Create, &rules); // clone: the context owns its name
        authorize_ref_updates(push, std::slice::from_ref(&update))?
            .into_iter()
            .collect::<Result<Vec<()>, ForgeError>>()
            .map_err(fail)?;
        let built = self
            .build(&plan, &request.request_id)
            .await
            .map_err(|e| self.refusal_logged(fail(e), &request.request_id))?;
        let identity = account_commit_identity(&self.db, user).await?;
        let commit = self
            .write(&plan, &built, (&identity, &identity))
            .await
            .map_err(|e| self.refusal_logged(fail(e), &request.request_id))?;
        let ctx = change_context(request, create.principal(), user)?;
        self.record_branch(&ctx, &plan, &commit, &request.request_id)
            .await;
        let new = NewMergeRequest {
            title: title_of(&plan.reverted),
            description: description_of(&plan.reverted, &plan.merge, &plan.merged),
            source_branch: plan.branch.clone(), // clone: forge-db owns its request
            target_branch: plan.reverted.target_branch.clone(), // clone: as above
            draft: false,
        };
        let (opened, ()) = review::open_mr_decided(&self.db, &ctx, project, user, &new, |facts| {
            let fresh = MergeRequestCtx::new(loaded(&facts.project), wanted.clone()); // clone: the fresh context owns it
            create.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
        .map_err(|e| fail(e).with_context("phase", "open_merge_request"))?;
        tracing::info!(
            target: "forge_review",
            event = "forge.review.reverted",
            request_id = %request.request_id,
            project_id = project.get(),
            merge_request_id = plan.reverted.id.get(),
            iid = plan.reverted.iid,
            merge_id = plan.merge.id,
            revert_iid = opened.iid,
            commit = %commit.as_str(),
            parent = %built.parent.as_str(),
        );
        Ok(RevertOpened {
            merge_request: opened,
            reverted_iid: plan.reverted.iid,
            commit,
            parent: built.parent,
        })
    }

    /// The system's draft after the push pipeline of a target branch failed
    /// on `sha` in `project` (module docs). Decisions are reports; a
    /// failure that may pass later (git, the database) is returned.
    pub async fn draft_after_failure(
        &self,
        project: ProjectId,
        sha: &ObjectId,
        request_id: &RequestId,
    ) -> Result<DraftReport, ForgeError> {
        let report = self.draft(project, sha, request_id).await?;
        tracing::info!(
            target: "forge_review",
            event = "forge.review.revert_drafted",
            request_id = %request_id,
            project_id = project.get(),
            sha = %sha.as_str(),
            outcome = match &report {
                DraftReport::Opened { .. } => "opened",
                DraftReport::Conflicted => "conflicted",
                DraftReport::Skipped(_) => "skipped",
            },
            reason = match &report {
                DraftReport::Skipped(r) => *r,
                DraftReport::Opened { .. } | DraftReport::Conflicted => "",
            },
            revert_iid = match &report {
                DraftReport::Opened { iid } => *iid,
                DraftReport::Conflicted | DraftReport::Skipped(_) => 0,
            },
        );
        Ok(report)
    }

    async fn draft(
        &self,
        project: ProjectId,
        sha: &ObjectId,
        request_id: &RequestId,
    ) -> Result<DraftReport, ForgeError> {
        let Some(merge) = applied_merge_at(&self.db, project, sha).await? else {
            return Ok(DraftReport::Skipped("no_merge"));
        };
        let target = merge.target_branch.clone(); // clone: the merge moves into the plan
                                                  // The target branch's own push pipeline at the merged commit, not a
                                                  // merge request's or another branch's that holds the same commit.
        if push_pipeline_status(&self.db, project, &target, sha)
            .await?
            .as_deref()
            != Some("failed")
        {
            return Ok(DraftReport::Skipped("not_target_pipeline"));
        }
        // The branch was failing before this merge: the merge is not the
        // cause (plan 04 M-4: identify the merge that broke it).
        if let Some(before) = &merge.expected_old {
            if push_pipeline_status(&self.db, project, &target, before)
                .await?
                .as_deref()
                == Some("failed")
            {
                return Ok(DraftReport::Skipped("target_already_failing"));
            }
        }
        let iid = reverts::merge_request_iid(&self.db, merge.merge_request).await?;
        let Some(reverted) = review::get(&self.db, project, iid).await? else {
            return Ok(DraftReport::Skipped("not_merged"));
        };
        let author = reverted.author;
        let Some(record) = self.db.load_project(project, Some(author)).await? else {
            return Ok(DraftReport::Skipped("not_merged"));
        };
        let project_view = LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        };
        // Decided as the author's own revert (their current role).
        let Some((kind, active, is_admin)) = reverts::account_facts(&self.db, author).await? else {
            return Ok(DraftReport::Skipped("author_not_allowed"));
        };
        let principal = author_principal(author, kind, active, is_admin);
        let create = match prove(
            &principal,
            Action::MergeRequestCreate,
            revert_ctx(
                project_view,
                (reverted.iid, &reverted.target_branch),
                author,
                true,
            )?,
        ) {
            Ok(proof) => proof,
            Err(e) => return Ok(self.skipped_refused(e, request_id, "author_not_allowed")),
        };
        let push = match prove(&principal, Action::RepoPush, ProjectCtx::new(project_view)) {
            Ok(proof) => proof,
            Err(e) => return Ok(self.skipped_refused(e, request_id, "author_not_allowed")),
        };
        let repo = ready(project, record.repository)?;
        let plan = match self.plan(repo, reverted).await {
            Ok(plan) => plan,
            Err(e) => return self.skipped(e, request_id),
        };
        if plan.merge.id != merge.id {
            // A later merge of the same merge request cannot exist (merged
            // is final); a mismatch is a defect, never a guess.
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "revert_merge_mismatch")
                .with_context("merge_id", merge.id));
        }
        let rules = forge_db::protected_refs::rules_for(&self.db, project).await?;
        let update = RefUpdateCtx::new(plan.branch.clone(), RefUpdate::Create, &rules); // clone: the context owns its name
        let allowed = authorize_ref_updates(&push, std::slice::from_ref(&update))?
            .into_iter()
            .collect::<Result<Vec<()>, ForgeError>>();
        if let Err(e) = allowed {
            return Ok(self.skipped_refused(e, request_id, "author_not_allowed"));
        }
        let ctx = system_ctx(request_id);
        let built = match self.build(&plan, request_id).await {
            Ok(built) => built,
            Err(e) if e.code() == ErrorCode::RevertConflict => {
                let files = conflict_files(&e);
                self.record_drafted(
                    &ctx,
                    project,
                    &plan,
                    RevertDraftOutcome::Conflicted,
                    None,
                    files,
                )
                .await?;
                return Ok(DraftReport::Conflicted);
            }
            Err(e) => return self.skipped(e, request_id),
        };
        let (name, email, username) = SYSTEM_IDENTITY;
        let identity = CommitIdentity::new(name, email, username)?;
        let commit = match self.write(&plan, &built, (&identity, &identity)).await {
            Ok(commit) => commit,
            Err(e) => return self.skipped(e, request_id),
        };
        self.record_branch(&ctx, &plan, &commit, request_id).await;
        let new = NewMergeRequest {
            title: title_of(&plan.reverted),
            description: description_of(&plan.reverted, &plan.merge, &plan.merged),
            source_branch: plan.branch.clone(), // clone: forge-db owns its request
            target_branch: plan.reverted.target_branch.clone(), // clone: as above
            draft: true,
        };
        let wanted = create.ctx().merge_request().clone(); // clone: the fresh context owns it
        let (opened, ()) =
            review::open_mr_decided(&self.db, &ctx, project, author, &new, |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), wanted);
                create.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await
            .map_err(|e| {
                e.with_context("phase", "open_merge_request")
                    .with_context("project_id", project.get())
            })?;
        self.record_drafted(
            &ctx,
            project,
            &plan,
            RevertDraftOutcome::Opened,
            Some(opened.iid),
            0,
        )
        .await?;
        Ok(DraftReport::Opened { iid: opened.iid })
    }

    /// The facts of the revert of `reverted` (refusals: not merged, no
    /// applied merge, the branch exists).
    async fn plan(&self, repo: RepoId, reverted: StoredMergeRequest) -> Result<Plan, ForgeError> {
        if reverted.state != MergeRequestState::Merged {
            return Err(refused("not_merged")
                .with_context("merge_request_id", reverted.id.get())
                .with_context("state", reverted.state.as_str()));
        }
        let merge = forge_db::review::merges::latest_merge(&self.db, reverted.id)
            .await?
            .filter(|m| m.state == MergeState::Applied);
        let Some(merge) = merge else {
            return Err(
                refused("no_applied_merge").with_context("merge_request_id", reverted.id.get())
            );
        };
        let (Some(before), Some(merged)) = (merge.expected_old.clone(), merge.new_sha.clone())
        else {
            return Err(refused("no_applied_merge").with_context("merge_id", merge.id));
        };
        let branch = revert_branch(reverted.iid)?;
        if self
            .git
            .read_ref(&self.storage, repo, &branch, self.timeouts.read)
            .await?
            .is_some()
        {
            return Err(refused("branch_exists").with_context("branch", branch.as_str().to_owned()));
        }
        Ok(Plan {
            repo,
            reverted,
            merge,
            merged,
            before,
            branch,
        })
    }

    /// The revert's tree on the target's tip now.
    async fn build(&self, plan: &Plan, request_id: &RequestId) -> Result<Built, ForgeError> {
        let target = &plan.reverted.target_branch;
        let Some(parent) = self
            .git
            .read_ref(&self.storage, plan.repo, target, self.timeouts.read)
            .await?
        else {
            return Err(
                refused("target_missing").with_context("target_branch", target.as_str().to_owned())
            );
        };
        let limits = MergeTreeLimits {
            timeout: self.timeouts.merge_tree,
            ..MergeTreeLimits::default()
        };
        let inputs = RevertInputs {
            merged: &plan.merged,
            before: &plan.before,
            onto: &parent,
        };
        match self
            .git
            .revert_tree(&self.storage, plan.repo, inputs, limits)
            .await?
        {
            RevertTree::Clean { tree } => Ok(Built { tree, parent }),
            RevertTree::Unchanged => Err(refused("nothing_to_revert")),
            RevertTree::Conflicted {
                files,
                files_truncated,
            } => {
                tracing::info!(
                    target: "forge_review",
                    event = "forge.review.revert_conflicted",
                    error_code = ErrorCode::RevertConflict.id(),
                    request_id = %request_id,
                    merge_request_id = plan.reverted.id.get(),
                    iid = plan.reverted.iid,
                    merge_id = plan.merge.id,
                    target_sha = %parent.as_str(),
                    conflict_files = files.len(),
                    files_truncated,
                    files = ?files,
                );
                Err(ForgeError::new(ErrorCode::RevertConflict)
                    .with_context("merge_request_id", plan.reverted.id.get())
                    .with_context("merge_id", plan.merge.id)
                    .with_context("target_sha", parent.as_str().to_owned())
                    .with_context("conflict_files", files.len())
                    .with_context("files_truncated", files_truncated))
            }
        }
    }

    /// The revert commit and its branch (ONE `update-ref` create: a branch
    /// that appeared meanwhile is `branch_exists`, nothing moved).
    async fn write(
        &self,
        plan: &Plan,
        built: &Built,
        (author, committer): (&CommitIdentity, &CommitIdentity),
    ) -> Result<ObjectId, ForgeError> {
        let message = message_of(&plan.reverted, &plan.merged);
        let parents = [built.parent.clone()]; // clone: the commit owns its parent
        let commit = self
            .git
            .commit_tree(
                &self.storage,
                plan.repo,
                &NewCommit {
                    tree: &built.tree,
                    parents: &parents,
                    message: &message,
                    author,
                    committer,
                },
                self.timeouts.write,
            )
            .await?;
        let change = RefChange::Create {
            name: plan.branch.clone(), // clone: the change owns its name
            new: commit.clone(),       // clone: as above
        };
        let created = self
            .git
            .update_refs(&self.storage, plan.repo, &[change], self.timeouts.write)
            .await;
        self.invalidate(plan.repo);
        if let Err(e) = created {
            // A concurrent revert won the create: read again.
            let now = self
                .git
                .read_ref(&self.storage, plan.repo, &plan.branch, self.timeouts.read)
                .await?;
            return Err(match now {
                Some(_) => {
                    refused("branch_exists").with_context("branch", plan.branch.as_str().to_owned())
                }
                None => e,
            });
        }
        Ok(commit)
    }

    /// The branch's creation as a push (`repository.pushed`: pipelines,
    /// versions, webhooks and the feed follow). The branch stands whatever
    /// happens here: a failure is logged once (the runbook's replay is the
    /// `forge.git.push_applied`-like line below).
    async fn record_branch(
        &self,
        ctx: &ChangeContext,
        plan: &Plan,
        commit: &ObjectId,
        request_id: &RequestId,
    ) {
        let zero = match ObjectId::parse(&"0".repeat(commit.as_str().len())) {
            Ok(zero) => zero,
            Err(e) => {
                telemetry::log_failure(
                    &e.with_context("phase", "revert_branch_record"),
                    request_id,
                );
                return;
            }
        };
        let record = PushRecord {
            repository_id: plan.repo,
            updates: vec![PushedRef {
                name: plan.branch.clone(), // clone: the record owns its name
                old: zero,
                new: commit.clone(), // clone: as above
                kind: PushUpdateKind::Create,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        tracing::info!(
            target: "forge_review",
            event = "forge.review.revert_branch_created",
            request_id = %request_id,
            repo_id = plan.repo.get(),
            r#ref = plan.branch.as_str(),
            new = commit.as_str(),
        );
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: the record owns it
            complete: true,
        };
        if let Err(e) =
            forge_db::repositories::record_push_with_commits(&self.db, ctx, &record, &commits).await
        {
            telemetry::log_failure(
                &e.with_context("phase", "revert_branch_record")
                    .with_context("repo_id", plan.repo.get())
                    .with_context("ref", plan.branch.as_str().to_owned()),
                request_id,
            );
        }
    }

    async fn record_drafted(
        &self,
        ctx: &ChangeContext,
        project: ProjectId,
        plan: &Plan,
        outcome: RevertDraftOutcome,
        revert_iid: Option<i64>,
        conflict_files: usize,
    ) -> Result<(), ForgeError> {
        let change = MergeRequestChange::RevertDrafted {
            merge_request: plan.reverted.id,
            iid: plan.reverted.iid,
            merge_id: plan.merge.id,
            target_branch: plan.reverted.target_branch.clone(), // clone: the change owns its name
            merged_sha: plan.merged.clone(),                    // clone: as above
            outcome,
            revert_iid,
            conflict_files,
        };
        reverts::record_revert_drafted(&self.db, ctx, project, change)
            .await
            .map(drop)
    }

    /// A refusal of the person's revert, logged once at info (the route
    /// logs the answer); anything else unchanged.
    fn refusal_logged(&self, e: ForgeError, request_id: &RequestId) -> ForgeError {
        if e.code() == ErrorCode::RevertRefused {
            tracing::info!(
                target: "forge_review",
                event = "forge.review.revert_refused",
                error_code = e.code().id(),
                request_id = %request_id,
                error = %e,
            );
        }
        e
    }

    /// A refusal of the system's draft is a report (`skipped` with its
    /// reason); any other failure is returned (the job retries).
    fn skipped(&self, e: ForgeError, request_id: &RequestId) -> Result<DraftReport, ForgeError> {
        if e.code() != ErrorCode::RevertRefused {
            return Err(e);
        }
        let reason = match reason_of(&e).as_str() {
            "not_merged" => "not_merged",
            "no_applied_merge" => "no_merge",
            "branch_exists" => "branch_exists",
            "nothing_to_revert" => "nothing_to_revert",
            "target_missing" => "target_missing",
            _ => "refused",
        };
        let _ = self.refusal_logged(e, request_id);
        Ok(DraftReport::Skipped(reason))
    }

    /// The author may not do it themselves: nothing is drafted (logged).
    fn skipped_refused(
        &self,
        e: ForgeError,
        request_id: &RequestId,
        reason: &'static str,
    ) -> DraftReport {
        tracing::info!(
            target: "forge_review",
            event = "forge.review.revert_draft_refused",
            error_code = e.code().id(),
            request_id = %request_id,
            reason,
            error = %e,
        );
        DraftReport::Skipped(reason)
    }

    fn invalidate(&self, repo: RepoId) {
        if let Some(browser) = &self.browser {
            browser.invalidate(repo);
        }
    }
}

impl crate::Review {
    /// A [`Reverter`] on this database with `git` and `storage` (the web
    /// route builds one per request from the Smart HTTP handles).
    pub fn reverter(&self, git: Git, storage: StorageRoot) -> Reverter {
        Reverter::new(self.db.clone(), git, storage) // clone: the pool handle is shared
    }
}

/// The reverted merge request's author as a principal for the system's
/// draft: their account's current flags and kind, the rights of a person
/// signed in (no token narrows them: none is used), never an agent.
fn author_principal(user: UserId, kind: AccountKind, active: bool, is_admin: bool) -> Principal {
    Principal::authenticated(user, is_admin, active, Credential::Session, kind)
}

/// The change context of the system's draft (actor `system`, label
/// [`REVERT_ACTOR_LABEL`]).
fn system_ctx(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(REVERT_ACTOR_LABEL.to_owned());
    ctx
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone()) // clone: the caller owns it
        .unwrap_or_default()
}

fn conflict_files(e: &ForgeError) -> usize {
    e.context()
        .iter()
        .find(|(k, _)| *k == "conflict_files")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_revert_branch_names_the_merge_request() {
        assert_eq!(
            revert_branch(12).expect("branch").as_str(),
            "refs/heads/revert/mr-12"
        );
    }

    #[test]
    fn a_long_title_is_cut_to_the_limit() {
        let long = "x".repeat(MAX_TITLE_CHARS);
        let title = format!("Revert \"{long}\"");
        assert!(title.chars().count() > MAX_TITLE_CHARS);
        let mut cut: String = title.chars().take(MAX_TITLE_CHARS - 1).collect();
        cut.push('…');
        assert_eq!(cut.chars().count(), MAX_TITLE_CHARS);
    }
}
