//! The deterministic autofix worker of `forge serve` (M4 C14, plan 14 C14,
//! ADR-0017 §1 ①③④⑦, §1.2, §2.1, §2.5 and its Amendment C14, plan 04
//! C-5). No LLM here (M5): only the fixers a project registers on the
//! TARGET branch's `.forge-ci.yml` (`autofix.fixers`: `fmt`, `clippy
//! --fix`, a lock file's or a generated file's regeneration).
//!
//! **The loop** ([`Autofixer::tick`], every few seconds):
//!
//! 1. *Start* a queued attempt (the trigger subscriber recorded it: a failed
//!    `ci/pipeline` on the merge request's merge result, or a conflict).
//!    Checked first, from the database only: the merge request is open and
//!    still at the attempt's head, autofix is on, fewer than
//!    [`MAX_ATTEMPTS_PER_DAY`] fixers ran today, the author is a person —
//!    then `prove(RepoPush)` of the autofix bot on `Resource::Autofix`
//!    (unprotected source branch, author ∩ enabler Developers) BEFORE git
//!    is read. Then the fixer is chosen from the target branch's file (a
//!    `format` / `lint` fixer for a failed pipeline; for a conflict a
//!    `regenerate` fixer whose `paths` cover every conflicted path, on the
//!    server's own `merge-tree` of head and target), the input commit is
//!    written (the head's tree — or the conflicted merge — with the target
//!    branch's `.forge-ci.yml`; parents head [, target]) and the fixer's
//!    own pipeline (source `autofix`, one job, no secrets) is created.
//! 2. *Advance* an attempt whose job ended: failed → `failed`
//!    (`FG-CI-0031`); no bundle → `no_change`; a bundle → land it. Both
//!    queue the next fixer on the same head.
//! 3. *Land*: the merge request must still be at the head; the artifact's
//!    sha256 is its row's; the bundle is imported with fsck and exactly
//!    the input commit as precondition (only its tree is used); the
//!    candidate must not change `.forge-ci.yml`; the bot's tree is the
//!    candidate with the base's `.forge-ci.yml` back; `diff-tree -r
//!    --no-renames --raw` of base and bot tree is checked
//!    ([`forge_core::autofix::check_scope`]), every changed blob is read
//!    (at most 1 MiB): secrets refuse, binaries make it a proposal, and for
//!    a conflict every conflicted path must be resolved inside its
//!    conflicted hunks only (§1.2). Migrations, binaries and protected paths
//!    are a proposal only (`FG-REVIEW-0031`); anything else out of scope is
//!    refused (`FG-GIT-0037`, alert). Then the server writes the bot's
//!    commit (`commit-tree`, bot identity, fixed message and trailers),
//!    decides the bot again with fresh facts, records the commit as the
//!    bot's (no approval is carried onto it, ADR-0017 decision 7) and moves
//!    the source branch with a compare-and-swap from the head (a branch
//!    that moved is `superseded`, `FG-GIT-0038`: never a force push, never
//!    a protected branch) and records the push (`repository.pushed`: the
//!    merge request's version, its pipeline).
//!
//! Every end is one row update and the merge request's event
//! `merge_request.autofix_finished` (actor system, integration class:
//! kanflow reads it), and one log line with `attempt_id`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_auth::{prove, Action, AutofixCtx, AutofixTarget, Principal, ProjectView, RefProtection};
use forge_core::autofix::{
    check_scope, commit_message, is_binary, script_words, within_conflict_hunks, AttemptState,
    AutofixTrigger, CandidateChange, ScopePolicy, ScopeVerdict, BOT_EMAIL, BOT_NAME,
    MAX_ATTEMPTS_PER_DAY, MAX_BLOB_BYTES, MAX_BUNDLE_BYTES, MAX_CHANGED_FILES, MAX_UNPACKED_BYTES,
};
use forge_core::{
    telemetry, ObjectId, ProjectAccessLoader, PushRecord, PushUpdateKind, PushedCommits, PushedRef,
    RefName, RepoId, RequestId, Role, UserId, UserKind,
};
use forge_db::autofix::{self as db, DetailPath, Finish, RunningAttempt, StoredAttempt};
use forge_db::change::{Actor, ChangeContext};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::autofix::{ConflictMerge, ImportLimits, RootEntry, TreeChanges};
use forge_git::browse::Browser;
use forge_git::commit::{CommitIdentity, NewCommit};
use forge_git::mergeability::MergeTreeLimits;
use forge_git::{Git, RefChange, StorageRoot};

use crate::ready;

/// The audit label of the bot's changes.
pub const AUTOFIX_ACTOR_LABEL: &str = BOT_NAME;
/// The file the fixers come from (the target branch's).
const DEFINITION: &str = ".forge-ci.yml";
/// The bot's login-less username in its commit identity.
const BOT_USERNAME: &str = "forge-autofix";

/// Where the CI artifact `(project, artifact id)` lives (forge-cli passes
/// forge-pipelines' store: feature crates never depend on each other).
pub type BundleFile = Arc<dyn Fn(i64, i64) -> PathBuf + Send + Sync>;

/// The bounds of the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutofixLimits {
    /// Attempts started and advanced per tick, each.
    pub batch: i64,
    /// One git read (a tree, a blob, a ref, a diff).
    pub git: Duration,
    /// The bundle's import.
    pub import: Duration,
    /// The server's `merge-tree` of a conflict.
    pub merge_tree: MergeTreeLimits,
}

impl Default for AutofixLimits {
    fn default() -> Self {
        Self {
            batch: 10,
            git: Duration::from_secs(20),
            import: Duration::from_secs(120),
            merge_tree: MergeTreeLimits::default(),
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    pub started: usize,
    pub advanced: usize,
}

/// The worker (module docs).
#[derive(Clone)]
pub struct Autofixer {
    db: Db,
    git: Git,
    storage: StorageRoot,
    browser: Option<Browser>,
    bundle_file: BundleFile,
    limits: AutofixLimits,
}

impl std::fmt::Debug for Autofixer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Autofixer")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// An end decided by a rule, not a failure of the worker.
struct End {
    state: AttemptState,
    reason: &'static str,
    detail: Vec<DetailPath>,
    error: Option<ForgeError>,
}

fn end(state: AttemptState, reason: &'static str) -> End {
    End {
        state,
        reason,
        detail: Vec::new(),
        error: None,
    }
}

fn path_detail(path: &str, reason: &str) -> DetailPath {
    DetailPath {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

fn refused(reason: &'static str, path: Option<&str>) -> End {
    let mut e = ForgeError::new(ErrorCode::AutofixCandidateInvalid).with_context("reason", reason);
    if let Some(p) = path {
        e = e.with_context("path", p.to_owned());
    }
    End {
        state: AttemptState::Refused,
        reason,
        detail: path
            .map(|p| vec![path_detail(p, reason)])
            .unwrap_or_default(),
        error: Some(e),
    }
}

/// A failure that may pass by itself (the next tick tries again).
fn transient(e: &ForgeError) -> bool {
    matches!(
        e.code(),
        ErrorCode::GitTimeout
            | ErrorCode::GitBusy
            | ErrorCode::GitUnavailable
            | ErrorCode::DbUnavailable
            | ErrorCode::DbQueryFailed
            | ErrorCode::StorageIoFailed
            | ErrorCode::RepositoryNotReady
    )
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone()) // clone: the caller owns it
        .unwrap_or_default()
}

/// A stable reason word of a refusal of the import (its `reason` context).
fn import_reason(e: &ForgeError) -> &'static str {
    const KNOWN: &[&str] = &[
        "bundle_unreadable",
        "bundle_too_large",
        "bundle_signature",
        "bundle_capability",
        "bundle_header_unterminated",
        "bundle_header_not_utf8",
        "bundle_ref_line",
        "bundle_prerequisite",
        "bundle_refs",
        "precondition_mismatch",
        "bundle_rejected",
    ];
    let reason = reason_of(e);
    KNOWN
        .iter()
        .copied()
        .find(|k| *k == reason)
        .unwrap_or("bundle_rejected")
}

fn bot_identity() -> Result<CommitIdentity, ForgeError> {
    CommitIdentity::new(BOT_NAME, BOT_EMAIL, BOT_USERNAME)
}

fn system_ctx(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(AUTOFIX_ACTOR_LABEL.to_owned());
    ctx
}

fn branch(raw: &str) -> Result<RefName, ForgeError> {
    RefName::parse(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_branch_invalid")
    })
}

/// The base a fixer starts from and the bot's commit's parents.
struct Base {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    /// For a conflict: the conflicted paths.
    conflicted: Option<Vec<String>>,
}

impl Autofixer {
    pub fn new(db: Db, git: Git, storage: StorageRoot, bundle_file: BundleFile) -> Self {
        Self {
            db,
            git,
            storage,
            browser: None,
            bundle_file,
            limits: AutofixLimits::default(),
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
    pub fn with_limits(mut self, limits: AutofixLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> AutofixLimits {
        self.limits
    }

    /// One pass (module docs). A failure of one attempt is its own log
    /// line (and, unless it may pass by itself, its end); only listing the
    /// attempts is the tick's error.
    pub async fn tick(&self) -> Result<TickReport, ForgeError> {
        let mut report = TickReport::default();
        for a in db::queued(&self.db, self.limits.batch).await? {
            let rid = RequestId::generate();
            match self.start(&a, &rid).await {
                Ok(()) => report.started += 1,
                Err(e) => self.failed(&a, e, &rid).await,
            }
        }
        for r in db::running(&self.db, self.limits.batch).await? {
            let rid = RequestId::generate();
            let a = r.attempt.clone(); // clone: kept for the failure
            match self.advance(r, &rid).await {
                Ok(()) => report.advanced += 1,
                Err(e) => self.failed(&a, e, &rid).await,
            }
        }
        Ok(report)
    }

    /// A failure of the worker on `a`: logged once; unless it may pass by
    /// itself, the attempt ends `failed` with its code.
    async fn failed(&self, a: &StoredAttempt, e: ForgeError, rid: &RequestId) {
        let e = e
            .with_context("attempt_id", a.id)
            .with_context("merge_request_id", a.merge_request.get())
            .with_context("operation", "autofix");
        telemetry::log_failure(&e, rid);
        if transient(&e) {
            return;
        }
        let finish = Finish {
            state: AttemptState::Failed,
            reason: Some("worker_error"),
            detail: Vec::new(),
            error_code: Some(e.code()),
            commit: None,
            fixer: None,
        };
        if let Err(again) = db::finish(&self.db, &system_ctx(rid), a, &finish).await {
            telemetry::log_failure(&again.with_context("attempt_id", a.id), rid);
        }
    }

    /// End `a` as `end` (one log line, the row, the event).
    async fn close(
        &self,
        a: &StoredAttempt,
        end: End,
        fixer: Option<&str>,
        commit: Option<ObjectId>,
        rid: &RequestId,
    ) -> Result<(), ForgeError> {
        let code = end.error.as_ref().map(ForgeError::code);
        let paths: Vec<&str> = end.detail.iter().map(|d| d.path.as_str()).collect();
        let event = match end.state {
            AttemptState::Applied => "forge.autofix.applied",
            AttemptState::Refused => "forge.autofix.refused",
            AttemptState::Proposed => "forge.autofix.proposed",
            AttemptState::Failed => "forge.autofix.failed",
            AttemptState::Superseded => "forge.autofix.superseded",
            AttemptState::Skipped if code == Some(ErrorCode::AutofixPushDenied) => {
                "forge.autofix.denied"
            }
            AttemptState::Skipped
            | AttemptState::NoChange
            | AttemptState::Queued
            | AttemptState::Running => "forge.autofix.ended",
        };
        let error = end
            .error
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let error_code = code.map_or("", ErrorCode::id);
        macro_rules! line {
            ($level:ident) => {
                tracing::$level!(
                    target: "forge_review",
                    event,
                    error_code,
                    request_id = %rid,
                    attempt_id = a.id,
                    project_id = a.project.get(),
                    merge_request_id = a.merge_request.get(),
                    job_id = a.job_id.unwrap_or(0),
                    trigger = a.trigger.as_str(),
                    fixer = fixer.unwrap_or(""),
                    state = end.state.as_str(),
                    reason = end.reason,
                    paths = ?paths,
                    commit = commit.as_ref().map_or("", ObjectId::as_str),
                    error = %error,
                )
            };
        }
        // The registry's severity: a refusal alerts (FG-GIT-0037), a failed
        // fixer and a denied bot warn, everything else is information.
        match code {
            Some(ErrorCode::AutofixCandidateInvalid) => line!(error),
            Some(ErrorCode::AutofixFixerFailed | ErrorCode::AutofixPushDenied) => line!(warn),
            Some(_) | None => line!(info),
        }
        let finish = Finish {
            state: end.state,
            reason: Some(end.reason),
            detail: end.detail,
            error_code: code,
            commit,
            fixer: fixer.map(str::to_owned),
        };
        db::finish(&self.db, &system_ctx(rid), a, &finish)
            .await
            .map(drop)
    }

    /// The bot's target for `a` from the database (module docs), with the
    /// ready repository.
    async fn target(
        &self,
        a: &StoredAttempt,
    ) -> Result<Result<(AutofixTarget, RepoId, db::TargetFacts), End>, ForgeError> {
        let Some(facts) = db::target_facts(&self.db, a.project, a.merge_request).await? else {
            return Ok(Err(end(AttemptState::Superseded, "not_open")));
        };
        if !facts.mr_open {
            return Ok(Err(end(AttemptState::Superseded, "not_open")));
        }
        if facts.head_sha.as_deref() != Some(a.head_sha.as_str()) {
            return Ok(Err(End {
                error: Some(
                    ForgeError::new(ErrorCode::AutofixLandingRaced)
                        .with_context("expected_sha", a.head_sha.as_str().to_owned())
                        .with_context("actual_sha", facts.head_sha.clone().unwrap_or_default()),
                ),
                ..end(AttemptState::Superseded, "head_moved")
            }));
        }
        if !facts.enabled {
            return Ok(Err(end(AttemptState::Skipped, "disabled")));
        }
        if facts.author_kind != UserKind::Human {
            // ADR-0017 decision 5: never by default for a service account's
            // or a bot's merge request.
            return Ok(Err(end(AttemptState::Skipped, "author_not_a_person")));
        }
        let Some(record) = self.db.load_project(a.project, Some(facts.author)).await? else {
            return Ok(Err(end(AttemptState::Superseded, "not_open")));
        };
        let enabler_role = match facts.enabler {
            Some(user) => self.role_of(a, user).await?,
            None => None,
        };
        let source = branch(&facts.source_branch)?;
        let target_branch = branch(&facts.target_branch)?;
        let rules = forge_db::protected_refs::rules_for(&self.db, a.project).await?;
        let target = AutofixTarget {
            project: ProjectView {
                visibility: record.access.visibility,
                role: None,
                pending_deletion: record.access.pending_deletion,
            },
            protection: RefProtection::of(&rules, &source),
            source_branch: source,
            target_branch,
            update: forge_auth::RefUpdate::FastForward,
            mr_open: facts.mr_open,
            autofix_enabled: facts.enabled,
            author_role: record.access.role,
            author_active: facts.author_active,
            enabler_role,
            enabler_active: facts.enabler_active,
        };
        let repo = ready(a.project, record.repository)?;
        Ok(Ok((target, repo, facts)))
    }

    async fn role_of(&self, a: &StoredAttempt, user: UserId) -> Result<Option<Role>, ForgeError> {
        Ok(self
            .db
            .load_project(a.project, Some(user))
            .await?
            .and_then(|r| r.access.role))
    }

    /// `prove(RepoPush)` of the bot on `target`; a refusal is the end
    /// (`FG-AUTH-0024` with the arm's reason).
    fn decide(
        &self,
        a: &StoredAttempt,
        target: AutofixTarget,
        stage: &'static str,
    ) -> Result<(), End> {
        let ctx = AutofixCtx::new(a.project, a.iid, target);
        prove(&Principal::autofix(), Action::RepoPush, ctx)
            .map(drop)
            .map_err(|e| End {
                error: Some(
                    ForgeError::new(ErrorCode::AutofixPushDenied)
                        .with_context("stage", stage)
                        .with_context("cause_reason", reason_of(&e))
                        .with_context("cause_code", e.code().id()),
                ),
                ..end(AttemptState::Skipped, "denied")
            })
    }

    /// The target branch's `.forge-ci.yml` at `target_sha`: its definition,
    /// bytes and root entry (`Err(End)`: none, or not valid).
    async fn definition(
        &self,
        repo: RepoId,
        target_sha: &ObjectId,
    ) -> Result<Result<(forge_ci::Definition, Vec<u8>, RootEntry), End>, ForgeError> {
        let t = self.limits.git;
        let Some(entry) = self
            .git
            .root_entry(&self.storage, repo, None, target_sha, DEFINITION, t)
            .await?
        else {
            return Ok(Err(end(AttemptState::Skipped, "no_definition")));
        };
        if entry.kind != "blob" || entry.mode != "100644" {
            return Ok(Err(end(AttemptState::Skipped, "definition_invalid")));
        }
        let Some(bytes) = self
            .git
            .blob_at_most(&self.storage, repo, None, &entry.id, forge_ci::MAX_BYTES, t)
            .await?
        else {
            return Ok(Err(end(AttemptState::Skipped, "definition_invalid")));
        };
        match forge_ci::check(&bytes, &forge_ci::LintContext::pinned_by_server()) {
            Ok(def) => Ok(Ok((def, bytes, entry))),
            Err(_) => Ok(Err(end(AttemptState::Skipped, "definition_invalid"))),
        }
    }

    /// The base of `a`: the head's tree, or the server's conflicted merge.
    async fn base(&self, repo: RepoId, a: &StoredAttempt) -> Result<Result<Base, End>, ForgeError> {
        match a.trigger {
            AutofixTrigger::PipelineFailed => Ok(Ok(Base {
                tree: self
                    .git
                    .tree_of(&self.storage, repo, &a.head_sha, self.limits.git)
                    .await?,
                parents: vec![a.head_sha.clone()], // clone: the commit owns its parents
                conflicted: None,
            })),
            AutofixTrigger::Conflict => {
                let merged = self
                    .git
                    .conflict_merge(
                        &self.storage,
                        repo,
                        &a.target_sha,
                        &a.head_sha,
                        self.limits.merge_tree,
                    )
                    .await?;
                match merged {
                    ConflictMerge::Clean { .. } => {
                        Ok(Err(end(AttemptState::Superseded, "no_conflict")))
                    }
                    ConflictMerge::Conflicted {
                        files_truncated: true,
                        ..
                    } => Ok(Err(end(AttemptState::Skipped, "conflict_too_large"))),
                    ConflictMerge::Conflicted { tree, files, .. } => Ok(Ok(Base {
                        tree,
                        // clone: the commit owns its parents
                        parents: vec![a.head_sha.clone(), a.target_sha.clone()],
                        conflicted: Some(files),
                    })),
                }
            }
        }
    }

    /// Start queued attempt `a` (module docs, step 1).
    async fn start(&self, a: &StoredAttempt, rid: &RequestId) -> Result<(), ForgeError> {
        if db::fixer_runs_today(&self.db, a.merge_request).await? >= MAX_ATTEMPTS_PER_DAY {
            return self
                .close(
                    a,
                    end(AttemptState::Skipped, "max_attempts"),
                    None,
                    None,
                    rid,
                )
                .await;
        }
        let (target, repo, facts) = match self.target(a).await? {
            Ok(t) => t,
            Err(e) => return self.close(a, e, None, None, rid).await,
        };
        let source = target.source_branch.clone(); // clone: the pipeline's ref
                                                   // The bot's right to write the branch, decided before git is read.
        if let Err(e) = self.decide(a, target, "start") {
            return self.close(a, e, None, None, rid).await;
        }
        let (def, bytes, entry) = match self.definition(repo, &a.target_sha).await? {
            Ok(d) => d,
            Err(e) => return self.close(a, e, None, None, rid).await,
        };
        let base = match self.base(repo, a).await? {
            Ok(b) => b,
            Err(e) => return self.close(a, e, None, None, rid).await,
        };
        let tried = db::tried_fixers(&self.db, a).await?;
        let fresh = |f: &&forge_ci::Fixer| !tried.iter().any(|t| t == f.name());
        let fixer = match &base.conflicted {
            None => def
                .fixers()
                .iter()
                .filter(fresh)
                .find(|f| !f.kind().resolves_conflicts()),
            Some(files) => def.fixers().iter().filter(fresh).find(|f| {
                f.kind().resolves_conflicts()
                    && files.iter().all(|p| {
                        f.paths()
                            .iter()
                            .any(|g| forge_core::autofix::glob_matches(g, p))
                    })
            }),
        };
        let Some(fixer) = fixer else {
            let reason = if tried.is_empty() {
                "no_fixer"
            } else {
                "no_fix"
            };
            return self
                .close(a, end(AttemptState::Skipped, reason), None, None, rid)
                .await;
        };
        let t = self.limits.git;
        let input_tree = self
            .git
            .tree_with_root_entry(
                &self.storage,
                repo,
                None,
                &base.tree,
                DEFINITION,
                Some(&entry),
                t,
            )
            .await?;
        let bot = bot_identity()?;
        let input = self
            .git
            .commit_tree(
                &self.storage,
                repo,
                &NewCommit {
                    tree: &input_tree,
                    parents: &base.parents,
                    message: "autofix input\n",
                    author: &bot,
                    committer: &bot,
                },
                t,
            )
            .await?;
        let job = fixer.job();
        let new = forge_db::pipelines::NewPipeline {
            request: forge_db::pipelines::PipelineRequest {
                project: a.project,
                git_ref: source,
                sha: input.clone(), // clone: the attempt records it too
                source: forge_db::pipelines::Source::Autofix,
                merge_request: None,
                triggered_by: None,
            },
            request_id: rid.to_string(),
            definition_hash: Some(forge_db::pipelines::definition_hash(&bytes)),
            protected: false,
            // Measured nowhere for an input commit: agents never read its
            // outputs (fail closed, ADR-0029 decision 5).
            copy_mark: forge_db::pipelines::CopyMark::Undecided,
            planned: forge_db::pipelines::Planned::Jobs(vec![forge_db::pipelines::NewCiJob {
                name: fixer.name().to_owned(),
                stage: forge_ci::FIXER_STAGE.to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: job.image().to_owned(),
                timeout_s: i32::try_from(job.timeout_secs()).unwrap_or(i32::MAX),
                tags: Vec::new(),
                secrets: Vec::new(),
                manual: false,
            }]),
            ai_eval_job: None,
        };
        let Some((pipeline, job_id)) =
            db::start(&self.db, a.id, fixer.name(), &input, &new).await?
        else {
            return Ok(()); // started elsewhere meanwhile
        };
        tracing::info!(
            target: "forge_review",
            event = "forge.autofix.started",
            request_id = %rid,
            attempt_id = a.id,
            project_id = a.project.get(),
            merge_request_id = a.merge_request.get(),
            trigger = a.trigger.as_str(),
            fixer = fixer.name(),
            head_sha = a.head_sha.as_str(),
            input_sha = input.as_str(),
            pipeline_id = pipeline,
            job_id = job_id,
            author_id = facts.author.get(),
        );
        Ok(())
    }

    /// Advance running attempt `r` whose job ended (step 2).
    async fn advance(&self, r: RunningAttempt, rid: &RequestId) -> Result<(), ForgeError> {
        let a = &r.attempt;
        let fixer = a.fixer.as_deref();
        match (r.job_status.as_str(), &r.bundle) {
            ("succeeded", None) => {
                self.close(
                    a,
                    end(AttemptState::NoChange, "no_change"),
                    fixer,
                    None,
                    rid,
                )
                .await?;
                db::requeue(&self.db, a, rid).await.map(drop)
            }
            ("succeeded", Some((artifact, digest))) => {
                let landed = self.land(a, *artifact, digest, rid).await?;
                if landed == AttemptState::NoChange {
                    db::requeue(&self.db, a, rid).await?;
                }
                Ok(())
            }
            (status, _) => {
                let reason = if status == "canceled" {
                    "job_canceled"
                } else {
                    "job_failed"
                };
                let e = ForgeError::new(ErrorCode::AutofixFixerFailed)
                    .with_context("reason", reason)
                    .with_context("pipeline_id", a.pipeline_id.unwrap_or(0))
                    .with_context("job_id", a.job_id.unwrap_or(0));
                let failed = End {
                    error: Some(e),
                    ..end(AttemptState::Failed, reason)
                };
                self.close(a, failed, fixer, None, rid).await?;
                db::requeue(&self.db, a, rid).await.map(drop)
            }
        }
    }

    /// Land the bundle of `a` (step 3): how it ended.
    async fn land(
        &self,
        a: &StoredAttempt,
        artifact: i64,
        digest: &str,
        rid: &RequestId,
    ) -> Result<AttemptState, ForgeError> {
        let fixer_name = a.fixer.clone().unwrap_or_default(); // clone: borrowed below
        let outcome = self
            .candidate(a, &fixer_name, artifact, digest, rid)
            .await?;
        let (state, commit) = match outcome {
            Ok(Landed { commit }) => (AttemptState::Applied, Some(commit)),
            Err(e) => {
                let state = e.state;
                self.close(a, e, Some(&fixer_name), None, rid).await?;
                return Ok(state);
            }
        };
        let applied = end(state, "applied");
        self.close(a, applied, Some(&fixer_name), commit, rid)
            .await?;
        Ok(state)
    }

    /// Check, build and push the bot's commit (step 3).
    #[allow(clippy::too_many_lines)] // one ordered pass of checks
    async fn candidate(
        &self,
        a: &StoredAttempt,
        fixer_name: &str,
        artifact: i64,
        digest: &str,
        rid: &RequestId,
    ) -> Result<Result<Landed, End>, ForgeError> {
        let (target, repo, _) = match self.target(a).await? {
            Ok(t) => t,
            Err(e) => return Ok(Err(e)),
        };
        let source = target.source_branch.clone(); // clone: the ref written below
        let Some(input) = a.input_sha.clone() else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "running_without_input"));
        };
        // The stored artifact, read once (copied into the quarantine while
        // its sha256 is checked against the row).
        let file = (self.bundle_file)(a.project.get(), artifact);
        let t = self.limits.git;
        let limits = ImportLimits {
            max_bytes: MAX_BUNDLE_BYTES,
            max_blob_bytes: MAX_BLOB_BYTES as u64,
            max_unpacked_bytes: MAX_UNPACKED_BYTES,
            timeout: self.limits.import,
        };
        let imported = match self
            .git
            .import_autofix_bundle(&self.storage, repo, &file, digest, &input, limits)
            .await
        {
            Ok(i) => i,
            Err(e) if e.code() == ErrorCode::AutofixCandidateInvalid => {
                let mut r = refused(import_reason(&e), None);
                r.error = Some(e);
                return Ok(Err(r));
            }
            Err(e) => return Err(e),
        };
        // Every check below reads the candidate's objects in the quarantine.
        let q = Some(&imported.quarantine);
        let (def, _, _) = match self.definition(repo, &a.target_sha).await? {
            Ok(d) => d,
            Err(_) => return Ok(Err(refused("fixer_gone", None))),
        };
        let Some(fixer) = def.fixer(fixer_name) else {
            return Ok(Err(refused("fixer_gone", None)));
        };
        // The fixer never changes the definition it ran under.
        let input_tree = self.git.tree_of(&self.storage, repo, &input, t).await?;
        let was = self
            .git
            .root_entry(&self.storage, repo, None, &input_tree, DEFINITION, t)
            .await?;
        let now = self
            .git
            .root_entry(&self.storage, repo, q, &imported.tree, DEFINITION, t)
            .await?;
        if was != now {
            return Ok(Err(refused("definition_changed", Some(DEFINITION))));
        }
        let base = match self.base(repo, a).await? {
            Ok(b) => b,
            Err(e) => return Ok(Err(e)),
        };
        let base_entry = self
            .git
            .root_entry(&self.storage, repo, None, &base.tree, DEFINITION, t)
            .await?;
        let bot_tree = self
            .git
            .tree_with_root_entry(
                &self.storage,
                repo,
                q,
                &imported.tree,
                DEFINITION,
                base_entry.as_ref(),
                t,
            )
            .await?;
        let changes = match self
            .git
            .tree_changes(
                &self.storage,
                repo,
                q,
                (&base.tree, &bot_tree),
                MAX_CHANGED_FILES,
                t,
            )
            .await?
        {
            TreeChanges::Complete(c) => c,
            TreeChanges::TooMany => return Ok(Err(refused("too_many_files", None))),
            TreeChanges::NonUtf8 => return Ok(Err(refused("path_invalid", None))),
        };
        if changes.is_empty() {
            return Ok(Err(end(AttemptState::NoChange, "no_change")));
        }
        // The paths (ADR-0017 §2.1, §1.2), from the target branch only.
        let scripts: Vec<String> = def
            .jobs()
            .iter()
            .chain(def.fixers().iter().map(forge_ci::Fixer::job))
            .flat_map(|j| j.script().iter().cloned()) // clone: the words are collected
            .collect();
        let words = script_words(&scripts);
        let scoped: Vec<CandidateChange> = changes
            .iter()
            .map(|c| CandidateChange {
                path: c.path.clone(), // clone: the policy reads owned changes
                kind: c.kind,
                old_mode: c.old_mode,
                new_mode: c.new_mode,
            })
            .collect();
        let policy = ScopePolicy {
            fixer_paths: fixer.paths(),
            conflicted: base.conflicted.as_deref(),
            script_paths: &words,
        };
        let mut proposals: Vec<DetailPath> = match check_scope(&scoped, &policy) {
            ScopeVerdict::Refused(f) => return Ok(Err(refused(f.reason, Some(f.path.as_str())))),
            ScopeVerdict::Proposal(found) => found
                .iter()
                .map(|f| path_detail(&f.path, f.reason))
                .collect(),
            ScopeVerdict::Allowed => Vec::new(),
        };
        // The content of every changed file.
        for c in &changes {
            let Some(new_id) = &c.new_id else {
                return Ok(Err(refused("deleted", Some(c.path.as_str()))));
            };
            let Some(content) = self
                .git
                .blob_at_most(&self.storage, repo, q, new_id, MAX_BLOB_BYTES, t)
                .await?
            else {
                return Ok(Err(refused("too_large", Some(c.path.as_str()))));
            };
            if is_binary(&content) {
                proposals.push(path_detail(&c.path, "binary"));
                continue;
            }
            if let forge_git::secret_scan::SecretScan::Found(_) =
                forge_git::secret_scan::scan(&content)
            {
                return Ok(Err(refused("secret_in_output", Some(c.path.as_str()))));
            }
            if base.conflicted.is_some() {
                let marked = match &c.old_id {
                    Some(old) => {
                        self.git
                            .blob_at_most(&self.storage, repo, q, old, MAX_BLOB_BYTES, t)
                            .await?
                    }
                    None => Some(Vec::new()),
                };
                let Some(marked) = marked else {
                    return Ok(Err(refused("too_large", Some(c.path.as_str()))));
                };
                if !within_conflict_hunks(&marked, &content) {
                    return Ok(Err(refused("outside_conflict_hunk", Some(c.path.as_str()))));
                }
            }
        }
        if let Some(conflicted) = &base.conflicted {
            if let Some(left) = conflicted
                .iter()
                .find(|p| !changes.iter().any(|c| &c.path == *p))
            {
                return Ok(Err(refused("conflict_unresolved", Some(left.as_str()))));
            }
        }
        if !proposals.is_empty() {
            let reasons: Vec<&str> = proposals.iter().map(|d| d.reason.as_str()).collect();
            let e = ForgeError::new(ErrorCode::AutofixProposalOnly)
                .with_context(
                    "reason",
                    reasons.first().copied().unwrap_or("proposal").to_owned(),
                )
                .with_context("paths", proposals.len());
            let first = proposals
                .first()
                .map(|d| d.reason.clone()) // clone: the reason word outlives the list
                .unwrap_or_default();
            let reason = match first.as_str() {
                "migration" => "migration",
                "binary" => "binary",
                _ => "protected_path",
            };
            return Ok(Err(End {
                state: AttemptState::Proposed,
                reason,
                detail: proposals,
                error: Some(e),
            }));
        }
        // The bot's commit: the server's, with the bot's identity.
        let bot = bot_identity()?;
        let message = commit_message(a.trigger, fixer_name, a.id, rid.as_str());
        let commit = self
            .git
            .commit_in(
                &self.storage,
                repo,
                q,
                &NewCommit {
                    tree: &bot_tree,
                    parents: &base.parents,
                    message: &message,
                    author: &bot,
                    committer: &bot,
                },
                t,
            )
            .await?;
        // Decided again with fresh facts right before the write.
        let (fresh, _, _) = match self.target(a).await? {
            Ok(t) => t,
            Err(e) => return Ok(Err(e)),
        };
        if let Err(e) = self.decide(a, fresh, "land") {
            return Ok(Err(e));
        }
        // Every check passed: the candidate's objects (and the bot's commit)
        // join the repository now, never before.
        self.git.migrate_quarantine(imported.quarantine).await?;
        if !db::set_candidate(&self.db, a.id, &commit).await? {
            return Ok(Err(end(AttemptState::Superseded, "not_running")));
        }
        let change = RefChange::Update {
            name: source.clone(),    // clone: the change owns its name
            new: commit.clone(),     // clone: as above
            old: a.head_sha.clone(), // clone: as above
        };
        let moved = self
            .git
            .update_refs(&self.storage, repo, &[change], t)
            .await;
        self.invalidate(repo);
        if let Err(e) = moved {
            let now = self.git.read_ref(&self.storage, repo, &source, t).await?;
            if now.as_ref() != Some(&a.head_sha) {
                return Ok(Err(End {
                    error: Some(
                        ForgeError::new(ErrorCode::AutofixLandingRaced)
                            .with_context("expected_sha", a.head_sha.as_str().to_owned())
                            .with_context(
                                "actual_sha",
                                now.map(|n| n.as_str().to_owned()).unwrap_or_default(),
                            ),
                    ),
                    ..end(AttemptState::Superseded, "head_moved")
                }));
            }
            return Err(e);
        }
        self.record_push(a, repo, &source, &commit, rid).await;
        Ok(Ok(Landed { commit }))
    }

    /// The branch's move as a push (`repository.pushed`: the merge
    /// request's version, its pipeline). The branch stands whatever happens
    /// here: a failure is logged once.
    async fn record_push(
        &self,
        a: &StoredAttempt,
        repo: RepoId,
        source: &RefName,
        commit: &ObjectId,
        rid: &RequestId,
    ) {
        let record = PushRecord {
            repository_id: repo,
            updates: vec![PushedRef {
                name: source.clone(),    // clone: the record owns its name
                old: a.head_sha.clone(), // clone: as above
                new: commit.clone(),     // clone: as above
                kind: PushUpdateKind::FastForward,
                commits: vec![commit.clone()], // clone: as above
                total_commits: 1,
                total_commits_partial: false,
            }],
        };
        let commits = PushedCommits {
            commits: vec![commit.clone()], // clone: as above
            complete: true,
        };
        if let Err(e) = forge_db::repositories::record_push_with_commits(
            &self.db,
            &system_ctx(rid),
            &record,
            &commits,
        )
        .await
        {
            telemetry::log_failure(
                &e.with_context("phase", "autofix_push_record")
                    .with_context("attempt_id", a.id)
                    .with_context("repo_id", repo.get())
                    .with_context("ref", source.as_str().to_owned()),
                rid,
            );
        }
    }

    fn invalidate(&self, repo: RepoId) {
        if let Some(browser) = &self.browser {
            browser.invalidate(repo);
        }
    }
}

/// The bot's commit on the source branch.
struct Landed {
    commit: ObjectId,
}
