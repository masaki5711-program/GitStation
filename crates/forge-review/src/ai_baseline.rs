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
//!   An open merge request of another author is left alone
//!   (`branch_exists`);
//! - an earlier candidate was merged (or the branch is in the default
//!   branch) → the branch is moved to the new commit on the default tip (a
//!   fast-forward, or a force push after a squash merge) and a new draft is
//!   opened;
//! - no merge request ever, and the tip is the system's commit → an
//!   interrupted proposal (the job was cancelled or the server stopped
//!   between the branch's move and the merge request): resumed — the file
//!   committed on it if it changed, and the draft opened;
//! - a merge request a person closed and the branch not merged → nothing
//!   (`branch_exists`): the system does not reopen what a person closed;
//!   deleting the branch lets the next night propose again. A merge request
//!   closed between the read and the compare-and-swap gets one more commit
//!   on its closed branch; the next night reads it closed and stops.
//!
//! Skipped too: a project without `ai_assets.enabled` (plan 17 §0.4,
//! `ai_assets_disabled`), and a new candidate past
//! [`MAX_OPEN_CANDIDATES`] open ones (`too_many_candidates`).
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
//! failure, unless the merge request is found open after all, the branch is
//! put back with a compare-and-swap); then the push
//! is recorded (`repository.pushed`: the merge request's version and its
//! pipeline follow).

use std::time::Duration;

use forge_auth::{
    authorize_ref_updates, prove, AccountKind, Action, Credential, LoadedMergeRequest,
    LoadedProject, MergeRequestCtx, MergeRequestState, Principal, ProjectCtx, RefUpdate,
    RefUpdateCtx,
};
use forge_core::ai_eval::{baseline_branch, baseline_path, BASELINE_BRANCH_PREFIX};
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
use forge_git::{Git, RefChange, Revision, StorageRoot};

use crate::{account, loaded, ready};

/// The audit label of the system's baseline candidate.
pub const BASELINE_ACTOR_LABEL: &str = "forge-ai-eval";

/// The server's identity on the candidate's commits.
const SYSTEM_IDENTITY: (&str, &str, &str) =
    ("GitStation", "ai-eval@gitstation.invalid", "gitstation");

/// The bound of one git step (a ref read, a file entry, the blob, the
/// commit, the `update-ref`). One proposal runs at most 14 of them; the
/// job's bound (`forge_db::ci_ai_eval::BASELINE_CANDIDATE_TIMEOUT`) is
/// above their sum.
pub const BASELINE_GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most open baseline candidates of one project (one per models; a
/// report's `models` is the bench's word, so the count is bounded).
pub const MAX_OPEN_CANDIDATES: i64 = 10;

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
    /// `branch_missing`, `target_missing`, `owner_not_allowed`,
    /// `ai_assets_disabled`, `too_many_candidates`, `branch_invalid`,
    /// `path_invalid`).
    Skipped(&'static str),
}

/// Where the candidate branch stands.
enum Branch {
    /// No branch: created on the default tip.
    Absent,
    /// The owner's open merge request's branch at this tip: committed on it.
    Open { iid: i64, tip: ObjectId },
    /// The system's branch without any merge request (an interrupted
    /// proposal): committed on it (when the file changed) and its merge
    /// request opened.
    Orphan { tip: ObjectId },
    /// An earlier candidate's branch (merged): moved to the new commit on
    /// the default tip — a fast-forward when the tip is in the target, else
    /// decided as a force push (a squash merge).
    Replace { tip: ObjectId, fast_forward: bool },
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
        // Plan 17 §0.4: nothing of the cycle runs in a project that did not
        // opt in (review MEDIUM).
        if !forge_db::ai_assets::setting(&self.db, ask.project)
            .await?
            .enabled
        {
            return Ok(BaselineReport::Skipped("ai_assets_disabled"));
        }
        let Some(head) = self.read(ask.repo, &ask.target).await? else {
            return Ok(BaselineReport::Skipped("target_missing"));
        };
        let state = match self.state(ask, &branch, &head).await? {
            Ok(state) => state,
            Err(reason) => return Ok(BaselineReport::Skipped(reason)),
        };
        // Unbounded `models` names never open unbounded merge requests.
        if !matches!(state, Branch::Open { .. })
            && review::open_count_from_prefix(&self.db, ask.project, BASELINE_BRANCH_PREFIX).await?
                >= MAX_OPEN_CANDIDATES
        {
            return Ok(BaselineReport::Skipped("too_many_candidates"));
        }
        // The owner's decisions, before git writes anything reachable.
        let Some(decided) = self.decide(ask, &branch, &state, request_id).await? else {
            return Ok(BaselineReport::Skipped("owner_not_allowed"));
        };
        let parent = match &state {
            Branch::Open { tip, .. } | Branch::Orphan { tip } => tip.clone(), // clone: the commit owns its parent
            Branch::Absent | Branch::Replace { .. } => {
                // A person changed the baseline since the bench's commit:
                // the next night proposes against theirs.
                let at_start = self.entry(ask.repo, &ask.start, &path).await?;
                if at_start != self.entry(ask.repo, &head, &path).await? {
                    return Ok(BaselineReport::Skipped("baseline_moved"));
                }
                head.clone() // clone: the commit owns its parent
            }
        };
        let blob = self
            .git
            .write_blob(&self.storage, ask.repo, &ask.content, BASELINE_GIT_TIMEOUT)
            .await?;
        let unchanged = self
            .entry(ask.repo, &parent, &path)
            .await?
            .is_some_and(|e| e.blob == blob);
        // `moved`: the branch's tip before and after, when it moved.
        let (tip, moved) = match (&state, unchanged) {
            // An interrupted proposal's branch already holds the file: only
            // its merge request is missing.
            (Branch::Orphan { tip }, true) => (tip.clone(), None), // clone: the merge request reads it
            (_, true) => return Ok(BaselineReport::Skipped("unchanged")),
            (_, false) => {
                let commit = self.write(ask, &path, (&parent, &blob)).await?;
                let old = match &state {
                    Branch::Absent => None,
                    Branch::Open { tip, .. }
                    | Branch::Orphan { tip }
                    | Branch::Replace { tip, .. } => {
                        Some(tip.clone()) // clone: the record keeps the old tip
                    }
                };
                self.swap(ask.repo, &branch, &commit, old.as_ref()).await?;
                (commit.clone(), Some((old, commit))) // clone: both keep it
            }
        };
        let ctx = system_ctx(request_id);
        let report = match (&state, decided) {
            (Branch::Open { iid, .. }, _) => BaselineReport::Updated { iid: *iid },
            (_, Decided::Open(create)) => {
                match self.open(ask, &branch, &ctx, &create).await {
                    Ok(iid) => BaselineReport::Opened { iid },
                    Err(e) => {
                        // The merge request may have committed although the
                        // answer was lost: look before putting anything back.
                        if let Some(iid) =
                            review::open_iid_between(&self.db, ask.project, (&branch, &ask.target))
                                .await?
                        {
                            BaselineReport::Opened { iid }
                        } else {
                            if let Some((old, new)) = &moved {
                                self.undo(ask.repo, &branch, new, old.as_ref(), request_id)
                                    .await;
                            }
                            return Err(e
                                .with_context("phase", "open_merge_request")
                                .with_context("project_id", ask.project.get())
                                .with_context("tip", tip.as_str().to_owned()));
                        }
                    }
                }
            }
            (_, Decided::Push) => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "baseline_open_undecided"))
            }
        };
        if let Some((old, new)) = moved {
            self.record_branch(&ctx, ask, &branch, (old, &new), request_id)
                .await;
        }
        Ok(report)
    }

    /// Where the candidate branch stands (`Err(reason)`: leave it alone).
    async fn state(
        &self,
        ask: &BaselineAsk,
        branch: &RefName,
        head: &ObjectId,
    ) -> Result<Result<Branch, &'static str>, ForgeError> {
        let latest = review::latest_between(&self.db, ask.project, (branch, &ask.target)).await?;
        let Some(tip) = self.read(ask.repo, branch).await? else {
            return Ok(match latest {
                // An open merge request whose branch is gone: a person's
                // business (a second one would be refused anyway).
                Some(mr) if mr.state == MergeRequestState::Open => Err("branch_missing"),
                _ => Ok(Branch::Absent),
            });
        };
        let merged_in = self
            .git
            .is_ancestor(&self.storage, ask.repo, &tip, head, BASELINE_GIT_TIMEOUT)
            .await?;
        Ok(match latest {
            Some(mr) if mr.state == MergeRequestState::Open => {
                // Only the schedule owner's candidate is moved (review LOW).
                if mr.author == ask.owner {
                    Ok(Branch::Open { iid: mr.iid, tip })
                } else {
                    Err("branch_exists")
                }
            }
            // An earlier candidate was merged (a squash leaves the branch
            // off the target: replaced, decided as a force push).
            Some(mr) if mr.state == MergeRequestState::Merged => Ok(Branch::Replace {
                tip,
                fast_forward: merged_in,
            }),
            // A person closed it: never reopened, unless it is in the target.
            Some(_) if merged_in => Ok(Branch::Replace {
                tip,
                fast_forward: true,
            }),
            Some(_) => Err("branch_exists"),
            None if merged_in => Ok(Branch::Replace {
                tip,
                fast_forward: true,
            }),
            // No merge request ever: an interrupted proposal (review HIGH)
            // when the tip is the system's commit; a person's branch else.
            None => {
                if self.system_authored(ask.repo, &tip).await? {
                    Ok(Branch::Orphan { tip })
                } else {
                    Err("branch_exists")
                }
            }
        })
    }

    /// Whether `commit`'s author is the server's identity.
    async fn system_authored(&self, repo: RepoId, commit: &ObjectId) -> Result<bool, ForgeError> {
        let dir = self.storage.existing(repo)?;
        let out = self
            .git
            .command("log")
            .arg("-1")
            .arg("--format=%ae")
            .revision(Revision::object_id(commit.as_str())?)
            .repo(&dir)
            .timeout(BASELINE_GIT_TIMEOUT)
            .output()
            .await?;
        Ok(out.stdout_str().trim() == SYSTEM_IDENTITY.1)
    }

    /// The candidate's commit: the file on `parent`.
    async fn write(
        &self,
        ask: &BaselineAsk,
        path: &FilePath,
        (parent, blob): (&ObjectId, &ObjectId),
    ) -> Result<ObjectId, ForgeError> {
        let identity = system_identity()?;
        let message = message_of(ask);
        self.git
            .write_file_commit(
                &self.storage,
                ask.repo,
                &FileCommit {
                    parent,
                    path,
                    blob,
                    mode: FileMode::Regular,
                    message: &message,
                    author: &identity,
                    committer: &identity,
                },
                BASELINE_GIT_TIMEOUT,
            )
            .await
    }

    /// ONE compare-and-swap of the branch from `old` (`None`: created).
    async fn swap(
        &self,
        repo: RepoId,
        branch: &RefName,
        commit: &ObjectId,
        old: Option<&ObjectId>,
    ) -> Result<(), ForgeError> {
        let change = match old {
            None => RefChange::Create {
                name: branch.clone(), // clone: the change owns its name
                new: commit.clone(),  // clone: as above
            },
            Some(old) => RefChange::Update {
                name: branch.clone(), // clone: as above
                new: commit.clone(),  // clone: as above
                old: old.clone(),     // clone: as above
            },
        };
        let moved = self
            .git
            .update_refs(&self.storage, repo, &[change], BASELINE_GIT_TIMEOUT)
            .await;
        self.invalidate(repo);
        moved
            .map(drop)
            .map_err(|e| e.with_context("phase", "baseline_branch"))
    }

    /// The draft merge request, decided again inside its transaction.
    async fn open(
        &self,
        ask: &BaselineAsk,
        branch: &RefName,
        ctx: &ChangeContext,
        create: &forge_auth::Authorized<MergeRequestCtx>,
    ) -> Result<i64, ForgeError> {
        let new = NewMergeRequest {
            title: format!("AI evaluation baseline candidate: {}", ask.models),
            description: description_of(ask),
            source_branch: branch.clone(), // clone: forge-db owns its request
            target_branch: ask.target.clone(), // clone: as above
            draft: true,
        };
        let wanted = create.ctx().merge_request().clone(); // clone: the fresh context owns it
        let (opened, ()) =
            review::open_mr_decided(&self.db, ctx, ask.project, ask.owner, &new, |facts| {
                let fresh = MergeRequestCtx::new(loaded(&facts.project), wanted);
                create.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(opened.iid)
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
            Branch::Open { .. }
            | Branch::Orphan { .. }
            | Branch::Replace {
                fast_forward: true, ..
            } => RefUpdate::FastForward,
            Branch::Replace {
                fast_forward: false,
                ..
            } => RefUpdate::Force,
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
