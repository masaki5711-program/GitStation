//! The nightly baseline candidate's merge request (plan 17 P17-6, §1.6,
//! §5.1 "毎晩 全ケースのベンチ、基準の候補"; ADR-0034 Amendment P17-6).
//!
//! A scheduled (C12) pipeline on the default branch whose bench report
//! passes the `forge/ai-eval` gates (or finds no baseline yet) proposes
//! the next baseline `evals/baselines/<models>.json`. forge-pipelines
//! builds the file (`forge_core::ai_eval::baseline_candidate`: the
//! baseline's kinds and `allowed` kept, a fixed reproduction case turned
//! regular); this module puts it on the branch
//! `ai-eval/baseline-<models>` and keeps **ONE** draft merge request of it
//! into the default branch:
//!
//! - an open merge request from that branch exists → the file is committed
//!   on its tip (a fast-forward: a new version of the same merge request);
//!   the same content → nothing (`unchanged`);
//! - none, and no branch → the branch is created with one commit on the
//!   default branch's tip and a DRAFT merge request is opened;
//! - none, and the branch is merged into the default branch (an earlier
//!   candidate was merged) → the branch is moved forward to the new commit
//!   on the default tip (a fast-forward) and a new draft is opened;
//! - none, and the branch is NOT merged (a person closed the merge request)
//!   → nothing (`branch_exists`): the system does not reopen what a person
//!   closed; deleting the branch lets the next night propose again.
//!
//! The file on the default tip must still be the one at the commit the
//! bench ran on (`baseline_moved` otherwise: a person changed the baseline
//! since; the next night proposes against it).
//!
//! **Who.** The merge request is the schedule owner's (the person the
//! scheduled pipeline ran as, C12), decided exactly as theirs with their
//! current role: `MergeRequestCreate` of a draft from the candidate branch
//! (re-decided inside the open's transaction) and `RepoPush` with
//! `authorize_ref_updates` on the push path's protection rules (a create or
//! a fast-forward) — before git writes anything but unreachable objects. A
//! refusal opens nothing (`owner_not_allowed`, logged with its code).
//! Audited as actor `system` (label [`BASELINE_ACTOR_LABEL`]). The commit's
//! author and committer are the server, never a person. The merge request
//! never merges without a person: it is a draft, `evals/baselines/` is a
//! protected path in an asset repository (plan 17 §2: an approver's
//! approval), and its author cannot approve it.
//!
//! **Why no push checks run on the file.** The content is
//! `forge_core::ai_eval::render` of a report `parse` accepted: the v1
//! schema's fields only (case ids `[a-z0-9._/-]`, numbers, the models name),
//! at most 1 MiB — no free text a secret could hide in, under the push's
//! blob limit. The ref decision is the push path's.
//!
//! **Order of writes** (as the revert draft): the blob, the commit, ONE
//! compare-and-swap `update-ref`; then the merge request (if new; on a
//! failure the branch is put back with a compare-and-swap); then the push
//! is recorded (`repository.pushed`: the merge request's version and its
//! pipeline follow).

use std::time::Duration;

use forge_auth::{
    authorize_ref_updates, prove, AccountKind, Action, Credential, LoadedMergeRequest,
    LoadedProject, MergeRequestCtx, MergeRequestState, Principal, ProjectCtx, RefUpdate,
    RefUpdateCtx,
};
use forge_core::ai_eval::{baseline_branch, baseline_path};
use forge_core::{
    telemetry, ObjectId, ProjectAccessLoader, ProjectId, PushRecord, PushUpdateKind, PushedCommits,
    PushedRef, RefName, RepoId, RequestId, UserId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{self, reverts, NewMergeRequest};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_git::commit::CommitIdentity;
use forge_git::file_commit::{FileCommit, FileMode, FilePath};
use forge_git::{Git, RefChange, StorageRoot};

use crate::{account, loaded, ready};

/// The audit label of the system's baseline candidate.
pub const BASELINE_ACTOR_LABEL: &str = "forge-ai-eval";

/// The server's identity on the candidate's commits.
const SYSTEM_IDENTITY: (&str, &str, &str) =
    ("GitStation", "ai-eval@gitstation.invalid", "gitstation");

/// The bound of one git step (a ref read, a file entry, the blob, the
/// commit, the `update-ref`).
pub const BASELINE_GIT_TIMEOUT: Duration = Duration::from_secs(20);

/// What to propose (forge-pipelines' `Proposal`, which this crate cannot
/// name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineAsk {
    pub project: ProjectId,
    pub repo: RepoId,
    pub pipeline_id: i64,
    /// The schedule's owner: the merge request's author.
    pub owner: UserId,
    /// The default branch (the merge request's target).
    pub target: RefName,
    /// The commit the bench ran on.
    pub start: ObjectId,
    pub models: String,
    /// The baseline file's bytes.
    pub content: Vec<u8>,
}

/// What [`BaselineProposer::propose`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineReport {
    /// A new draft merge request `iid`.
    Opened { iid: i64 },
    /// The open merge request `iid` got the new file.
    Updated { iid: i64 },
    /// Nothing, and why (`unchanged`, `baseline_moved`, `branch_exists`,
    /// `target_missing`, `owner_not_allowed`, `branch_invalid`,
    /// `path_invalid`).
    Skipped(&'static str),
}

/// Where the candidate branch stands.
enum Branch {
    /// No branch: created on the default tip.
    Absent,
    /// An open merge request's branch at this tip: committed on it.
    Open { iid: i64, tip: ObjectId },
    /// A merged branch at this tip: moved forward to the default tip's
    /// commit.
    Merged { tip: ObjectId },
}

/// Keeps the nightly baseline candidate's merge request (module docs).
#[derive(Clone)]
pub struct BaselineProposer {
    db: Db,
    git: Git,
    storage: StorageRoot,
    browser: Option<Browser>,
}

impl std::fmt::Debug for BaselineProposer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaselineProposer").finish_non_exhaustive()
    }
}

impl BaselineProposer {
    pub fn new(db: Db, git: Git, storage: StorageRoot) -> Self {
        Self {
            db,
            git,
            storage,
            browser: None,
        }
    }

    /// The browse cache every ref write invalidates (`forge serve`).
    #[must_use]
    pub fn with_browser(mut self, browser: Browser) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Propose `ask` (module docs); logged once as
    /// `forge.ai_eval.baseline_proposed`. Errors (the job retries or dies
    /// with the code): git, storage and database failures.
    pub async fn propose(
        &self,
        ask: &BaselineAsk,
        request_id: &RequestId,
    ) -> Result<BaselineReport, ForgeError> {
        let report = self.run(ask, request_id).await?;
        let (outcome, reason, iid) = match &report {
            BaselineReport::Opened { iid } => ("opened", "", *iid),
            BaselineReport::Updated { iid } => ("updated", "", *iid),
            BaselineReport::Skipped(r) => ("skipped", *r, 0),
        };
        tracing::info!(
            target: "forge_review",
            event = "forge.ai_eval.baseline_proposed",
            request_id = %request_id,
            project_id = ask.project.get(),
            pipeline_id = ask.pipeline_id,
            models = ask.models.as_str(),
            start_sha = ask.start.as_str(),
            outcome,
            reason,
            iid,
        );
        Ok(report)
    }

    async fn run(
        &self,
        ask: &BaselineAsk,
        request_id: &RequestId,
    ) -> Result<BaselineReport, ForgeError> {
        let Ok(branch) = RefName::parse(&baseline_branch(&ask.models)) else {
            return Ok(BaselineReport::Skipped("branch_invalid"));
        };
        let Ok(path) = FilePath::parse(&baseline_path(&ask.models)) else {
            return Ok(BaselineReport::Skipped("path_invalid"));
        };
        let Some(head) = self.read(ask.repo, &ask.target).await? else {
            return Ok(BaselineReport::Skipped("target_missing"));
        };
        let state = match (
            review::open_iid_between(&self.db, ask.project, (&branch, &ask.target)).await?,
            self.read(ask.repo, &branch).await?,
        ) {
            (_, None) => Branch::Absent,
            (Some(iid), Some(tip)) => Branch::Open { iid, tip },
            (None, Some(tip)) => {
                let merged = self
                    .git
                    .is_ancestor(&self.storage, ask.repo, &tip, &head, BASELINE_GIT_TIMEOUT)
                    .await?;
                if !merged {
                    return Ok(BaselineReport::Skipped("branch_exists"));
                }
                Branch::Merged { tip }
            }
        };
        // The owner's decisions, before git writes anything reachable.
        let Some(decided) = self.decide(ask, &branch, &state, request_id).await? else {
            return Ok(BaselineReport::Skipped("owner_not_allowed"));
        };
        let parent = match &state {
            Branch::Open { tip, .. } => tip.clone(), // clone: the commit owns its parent
            Branch::Absent | Branch::Merged { .. } => {
                // A person changed the baseline since the bench's commit:
                // the next night proposes against theirs.
                let at_start = self.entry(ask.repo, &ask.start, &path).await?;
                if at_start != self.entry(ask.repo, &head, &path).await? {
                    return Ok(BaselineReport::Skipped("baseline_moved"));
                }
                head
            }
        };
        let blob = self
            .git
            .write_blob(&self.storage, ask.repo, &ask.content, BASELINE_GIT_TIMEOUT)
            .await?;
        if self
            .entry(ask.repo, &parent, &path)
            .await?
            .is_some_and(|e| e.blob == blob)
        {
            return Ok(BaselineReport::Skipped("unchanged"));
        }
        let identity = system_identity()?;
        let message = message_of(ask);
        let commit = self
            .git
            .write_file_commit(
                &self.storage,
                ask.repo,
                &FileCommit {
                    parent: &parent,
                    path: &path,
                    blob: &blob,
                    mode: FileMode::Regular,
                    message: &message,
                    author: &identity,
                    committer: &identity,
                },
                BASELINE_GIT_TIMEOUT,
            )
            .await?;
        let (change, old) = match &state {
            Branch::Absent => (
                RefChange::Create {
                    name: branch.clone(), // clone: the change owns its name
                    new: commit.clone(),  // clone: as above
                },
                None,
            ),
            Branch::Open { tip, .. } | Branch::Merged { tip } => (
                RefChange::Update {
                    name: branch.clone(), // clone: as above
                    new: commit.clone(),  // clone: as above
                    old: tip.clone(),     // clone: as above
                },
                Some(tip.clone()), // clone: the record keeps the old tip
            ),
        };
        let moved = self
            .git
            .update_refs(&self.storage, ask.repo, &[change], BASELINE_GIT_TIMEOUT)
            .await;
        self.invalidate(ask.repo);
        moved.map_err(|e| e.with_context("phase", "baseline_branch"))?;
        let ctx = system_ctx(request_id);
        let report = match (&state, decided) {
            (Branch::Open { iid, .. }, _) => BaselineReport::Updated { iid: *iid },
            (Branch::Absent | Branch::Merged { .. }, Decided::Open(create)) => {
                let new = NewMergeRequest {
                    title: format!("AI evaluation baseline candidate: {}", ask.models),
                    description: description_of(ask),
                    source_branch: branch.clone(), // clone: forge-db owns its request
                    target_branch: ask.target.clone(), // clone: as above
                    draft: true,
                };
                let wanted = create.ctx().merge_request().clone(); // clone: the fresh context owns it
                let opened = review::open_mr_decided(
                    &self.db,
                    &ctx,
                    ask.project,
                    ask.owner,
                    &new,
                    |facts| {
                        let fresh = MergeRequestCtx::new(loaded(&facts.project), wanted);
                        create.reconfirm(account(facts.account), fresh).map(drop)
                    },
                )
                .await;
                match opened {
                    Ok((opened, ())) => BaselineReport::Opened { iid: opened.iid },
                    Err(e) => {
                        self.undo(ask.repo, &branch, &commit, old.as_ref(), request_id)
                            .await;
                        return Err(e
                            .with_context("phase", "open_merge_request")
                            .with_context("project_id", ask.project.get()));
                    }
                }
            }
            (Branch::Absent | Branch::Merged { .. }, Decided::Push) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "baseline_open_undecided"))
            }
        };
        self.record_branch(&ctx, ask, &branch, (old, &commit), request_id)
            .await;
        Ok(report)
    }

    /// The owner's `MergeRequestCreate` (a new merge request only) and
    /// `RepoPush` of the branch's create or fast-forward. `None`: refused
    /// (logged).
    async fn decide(
        &self,
        ask: &BaselineAsk,
        branch: &RefName,
        state: &Branch,
        request_id: &RequestId,
    ) -> Result<Option<Decided>, ForgeError> {
        let Some(record) = self.db.load_project(ask.project, Some(ask.owner)).await? else {
            return Ok(None);
        };
        let project = LoadedProject {
            id: record.id,
            visibility: record.access.visibility,
            role: record.access.role,
            pending_deletion: record.access.pending_deletion,
        };
        let _repo = ready(ask.project, record.repository)?;
        let Some((kind, active, is_admin)) = reverts::account_facts(&self.db, ask.owner).await?
        else {
            return Ok(None);
        };
        let principal = owner_principal(ask.owner, kind, active, is_admin);
        let refused = |e: ForgeError| {
            tracing::info!(
                target: "forge_review",
                event = "forge.ai_eval.baseline_refused",
                error_code = e.code().id(),
                request_id = %request_id,
                project_id = ask.project.get(),
                owner_id = ask.owner.get(),
                error = %e,
                "the schedule's owner may not propose the baseline: nothing is written"
            );
        };
        let push = match prove(&principal, Action::RepoPush, ProjectCtx::new(project)) {
            Ok(proof) => proof,
            Err(e) => {
                refused(e);
                return Ok(None);
            }
        };
        let update = match state {
            Branch::Absent => RefUpdate::Create,
            Branch::Open { .. } | Branch::Merged { .. } => RefUpdate::FastForward,
        };
        let rules = forge_db::protected_refs::rules_for(&self.db, ask.project).await?;
        let ref_ctx = RefUpdateCtx::new(branch.clone(), update, &rules); // clone: the context owns its name
        let allowed = authorize_ref_updates(&push, std::slice::from_ref(&ref_ctx))?
            .into_iter()
            .collect::<Result<Vec<()>, ForgeError>>();
        if let Err(e) = allowed {
            refused(e);
            return Ok(None);
        }
        if let Branch::Open { .. } = state {
            return Ok(Some(Decided::Push));
        }
        let ctx = MergeRequestCtx::new(
            project,
            LoadedMergeRequest {
                iid: None,
                author: ask.owner,
                state: MergeRequestState::Open,
                draft: true,
                source_branch: branch.clone(), // clone: the context owns its names
                target_branch: ask.target.clone(), // clone: as above
            },
        );
        match prove(&principal, Action::MergeRequestCreate, ctx) {
            Ok(proof) => Ok(Some(Decided::Open(Box::new(proof)))),
            Err(e) => {
                refused(e);
                Ok(None)
            }
        }
    }

    async fn read(&self, repo: RepoId, name: &RefName) -> Result<Option<ObjectId>, ForgeError> {
        self.git
            .read_ref(&self.storage, repo, name, BASELINE_GIT_TIMEOUT)
            .await
    }

    async fn entry(
        &self,
        repo: RepoId,
        commit: &ObjectId,
        path: &FilePath,
    ) -> Result<Option<forge_git::file_commit::FileEntry>, ForgeError> {
        self.git
            .file_entry(&self.storage, repo, commit, path, BASELINE_GIT_TIMEOUT)
            .await
    }

    /// Put the branch back (the merge request could not be opened): a
    /// compare-and-swap on the new commit. A failure is logged once; the
    /// caller returns the original error.
    async fn undo(
        &self,
        repo: RepoId,
        branch: &RefName,
        commit: &ObjectId,
        old: Option<&ObjectId>,
        request_id: &RequestId,
    ) {
        let change = match old {
            None => RefChange::Delete {
                name: branch.clone(), // clone: the change owns its name
                old: commit.clone(),  // clone: as above
            },
            Some(before) => RefChange::Update {
                name: branch.clone(), // clone: as above
                new: before.clone(),  // clone: as above
                old: commit.clone(),  // clone: as above
            },
        };
        let undone = self
            .git
            .update_refs(&self.storage, repo, &[change], BASELINE_GIT_TIMEOUT)
            .await;
        self.invalidate(repo);
        if let Err(e) = undone {
            telemetry::log_failure(
                &e.with_context("phase", "baseline_branch_undo")
                    .with_context("ref", branch.as_str().to_owned()),
                request_id,
            );
        }
    }

    /// The branch's move as a push (`repository.pushed`). The branch stands
    /// whatever happens here: a failure is logged once.
    async fn record_branch(
        &self,
        ctx: &ChangeContext,
        ask: &BaselineAsk,
        branch: &RefName,
        (old, commit): (Option<ObjectId>, &ObjectId),
        request_id: &RequestId,
    ) {
        let (old, kind) = match old {
            Some(old) => (old, PushUpdateKind::FastForward),
            None => match ObjectId::parse(&"0".repeat(commit.as_str().len())) {
                Ok(zero) => (zero, PushUpdateKind::Create),
                Err(e) => {
                    telemetry::log_failure(&e.with_context("phase", "baseline_record"), request_id);
                    return;
                }
            },
        };
        let record = PushRecord {
            repository_id: ask.repo,
            updates: vec![PushedRef {
                name: branch.clone(), // clone: the record owns its name
                old,
                new: commit.clone(), // clone: as above
                kind,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: the record owns it
            complete: true,
        };
        if let Err(e) =
            forge_db::repositories::record_push_with_commits(&self.db, ctx, &record, &commits).await
        {
            telemetry::log_failure(
                &e.with_context("phase", "baseline_record")
                    .with_context("repo_id", ask.repo.get())
                    .with_context("ref", branch.as_str().to_owned()),
                request_id,
            );
        }
    }

    fn invalidate(&self, repo: RepoId) {
        if let Some(browser) = &self.browser {
            browser.invalidate(repo);
        }
    }
}

/// The owner's decisions ([`BaselineProposer::decide`]).
enum Decided {
    /// The push of the open merge request's branch only.
    Push,
    /// The push and the new merge request.
    Open(Box<forge_auth::Authorized<MergeRequestCtx>>),
}

fn message_of(ask: &BaselineAsk) -> String {
    format!(
        "AI evaluation baseline candidate: {models}\n\nThe nightly bench (pipeline #{pipeline}) \
         on {sha}.\nProposed by GitStation; a person reviews and approves it.\n",
        models = ask.models,
        pipeline = ask.pipeline_id,
        sha = ask.start.as_str()
    )
}

fn description_of(ask: &BaselineAsk) -> String {
    format!(
        "The nightly AI evaluation (pipeline #{pipeline} on `{sha}`) passed the `forge/ai-eval` \
         gates; this is the baseline `{path}` it proposes. Fixed reproduction cases are regular \
         cases from now on; `allowed` is the current baseline's.\n\n\
         Review the case and metric changes, then mark it ready and approve it (an approver of \
         the protected path). Each night updates this merge request. To stop the proposals, \
         close it and keep the branch; delete the branch to resume. Runbook: ai-eval-bench.",
        pipeline = ask.pipeline_id,
        sha = ask.start.as_str(),
        path = baseline_path(&ask.models)
    )
}

fn system_identity() -> Result<CommitIdentity, ForgeError> {
    let (name, email, username) = SYSTEM_IDENTITY;
    CommitIdentity::new(name, email, username)
}

/// The schedule's owner as a principal: their account's current flags and
/// kind, the rights of a person signed in (no token narrows them).
fn owner_principal(user: UserId, kind: AccountKind, active: bool, is_admin: bool) -> Principal {
    Principal::authenticated(user, is_admin, active, Credential::Session, kind)
}

fn system_ctx(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(BASELINE_ACTOR_LABEL.to_owned());
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_description_names_the_file_and_the_runbook() {
        let ask = BaselineAsk {
            project: ProjectId::new(1),
            repo: RepoId::new(1),
            pipeline_id: 7,
            owner: UserId::new(2),
            target: RefName::parse("refs/heads/main").expect("ref"),
            start: ObjectId::parse(&"a".repeat(40)).expect("sha"),
            models: "qwen3-14b".to_owned(),
            content: Vec::new(),
        };
        let d = description_of(&ask);
        assert!(d.contains("evals/baselines/qwen3-14b.json"), "{d}");
        assert!(d.contains("Runbook: ai-eval-bench"), "{d}");
        assert!(message_of(&ask).starts_with("AI evaluation baseline candidate: qwen3-14b\n"));
    }
}
