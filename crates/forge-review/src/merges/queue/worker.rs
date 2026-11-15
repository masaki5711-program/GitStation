//! The merge queue's worker (ADR-0030 §3–§8, §13, §14): [`MergeQueue::tick`]
//! claims one queue that is due, does one step and releases it.
//!
//! A step, from the queue's live batch (at most one):
//!
//! - **none** (and not paused): build a batch — the queued entries in
//!   order, at most the queue's `batch_limit`; an entry that must run alone
//!   (a migration, a `forge/risk` stop, a fast-forward) ends the batch
//!   before it, or is the batch when it is first. Each entry is merged on
//!   the previous step of the train exactly like a merge (merge commit or
//!   squash, the frozen message, the requester and author identities); a
//!   conflicting entry is removed. The train's tip is pointed at by
//!   `refs/merge-queue/<queue_id>/train` and its pipeline (`source =
//!   merge_queue`) is enqueued with the batch.
//! - **testing**: an entry that left (dequeued, a new head, closed) or a
//!   target that moved throws the batch away (the entries go back in
//!   order); a pipeline still running waits, past the deadline the batch is
//!   reaped (`FG-CI-0024`); red bisects (`batch_limit` halves; a red batch
//!   of one removes its entry, `FG-CI-0023`); green lands unless paused.
//! - **landing** (a worker stopped mid-landing): settled by asking git.
//!
//! The landing holds the repository's merge lock (the normal merges' too)
//! from the first decision to the record; every git call is bounded.

use std::time::Duration;

use forge_auth::Action;
use forge_core::merge_queue::{bisected_limit, BatchState, EntryState, Isolation, MAX_REAPS};
use forge_core::{
    telemetry, CheckConclusion, CheckName, CheckRun, MergeMethod, MergeRequestState, MergeState,
    ObjectId, RefName, RepoId, RequestId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{create_job, PipelineRequest, PipelineStatus, Source};
use forge_db::review::merge_queue::{
    self as db, BatchEnd, BatchMember, Claimed, NewBatch, StoredBatch, StoredEntry, TrainPipeline,
};
use forge_db::review::merges::{self, MergeLock, QueueStep, StoredMerge};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::commit::NewCommit;
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::review_inputs::ChangedPaths;
use forge_git::RefChange;

use super::super::execute::{is_refusal, Subject};
use super::super::{account_commit_identity, Merger};
use super::{decide_queued, train_run, Asked, TrainRuns};

/// The bounds of the queue's worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    /// How long a claimed queue is the worker's (an expired lease is
    /// claimable again: a stopped worker's queue is taken over).
    pub lease: Duration,
    /// How long a batch may test before it is reaped (`FG-CI-0024`).
    pub test_timeout: Duration,
    /// The next step of a queue that waits (a pipeline running, paused).
    pub poll: Duration,
    /// The most changed paths read to find a migration (more: alone).
    pub max_paths: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(300),
            test_timeout: Duration::from_secs(2 * 3600),
            poll: Duration::from_secs(10),
            max_paths: 5000,
        }
    }
}

/// What one tick did (the worker's log and the tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub queue_id: i64,
    /// `idle`, `paused`, `built`, `waiting`, `landed`, `bisected`,
    /// `removed`, `abandoned`, `reaped`, `raced`, `recovered`.
    pub outcome: &'static str,
    pub batch_id: Option<i64>,
}

/// The merge queue's worker (module docs).
#[derive(Debug, Clone)]
pub struct MergeQueue {
    merger: Merger,
    limits: QueueLimits,
    worker: String,
    /// Tests only: stop a landing right after the target moved, as a
    /// worker that died there would (the recovery's test).
    crash_after_cas: bool,
}

/// The one-line `reason` of a refusal (its own when it is a plain word).
fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone()) // clone: the row keeps its own copy
        .filter(|v| {
            !v.is_empty()
                && v.len() <= 64
                && v.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && v.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or_else(|| "refused".to_owned())
}

/// How a train step could not be built for an entry.
enum StepRefused {
    /// Removed with this reason and code; the train goes on without it.
    Remove(&'static str, ErrorCode),
}

impl MergeQueue {
    /// A worker named `worker` (its lease owner) on `merger`'s database,
    /// git and storage.
    pub fn new(merger: Merger, worker: impl Into<String>) -> Self {
        Self {
            merger,
            limits: QueueLimits::default(),
            worker: worker.into(),
            crash_after_cas: false,
        }
    }

    /// Tests only: every landing stops right after its compare-and-swap
    /// (`FG-SYS-0001 reason=test_crash_after_cas`), leaving the batch
    /// `landing` with its attempts `executing` for the recovery.
    #[doc(hidden)]
    #[must_use]
    pub fn with_crash_after_cas(mut self) -> Self {
        self.crash_after_cas = true;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, limits: QueueLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> QueueLimits {
        self.limits
    }

    /// Claim one due queue, do one step, release it (module docs). `None`:
    /// no queue to work now. A failure is returned after the lease is
    /// released (the queue is tried again after `poll`).
    pub async fn tick(&self) -> Result<Option<TickReport>, ForgeError> {
        let Some(claimed) = db::claim(&self.merger.db, &self.worker, self.limits.lease).await?
        else {
            return Ok(None);
        };
        let stepped = self.step(&claimed).await;
        let next_in = match &stepped {
            Ok(report) if matches!(report.outcome, "waiting" | "paused" | "idle") => {
                self.limits.poll
            }
            Ok(_) => Duration::ZERO,
            Err(_) => self.limits.poll,
        };
        let released = db::release(&self.merger.db, &claimed, next_in).await;
        let report = stepped?;
        if !released? {
            tracing::warn!(
                target: "forge_review",
                event = "forge.merge_queue.lease_lost",
                queue_id = claimed.queue.id,
                worker = %self.worker,
                outcome = report.outcome,
                "the queue's lease expired during the step; another worker owns it now"
            );
        }
        Ok(Some(report))
    }

    async fn step(&self, claimed: &Claimed) -> Result<TickReport, ForgeError> {
        let queue = &claimed.queue;
        let report = |outcome, batch_id| TickReport {
            queue_id: queue.id,
            outcome,
            batch_id,
        };
        let Some(batch) = db::live_batch(&self.merger.db, queue.id).await? else {
            if queue.paused {
                return Ok(report("paused", None));
            }
            return self.build(claimed).await;
        };
        let outcome = match batch.state {
            BatchState::Testing => self.poll(claimed, &batch).await?,
            BatchState::Landing => self.recover(claimed, &batch).await?,
            BatchState::Landed | BatchState::Failed | BatchState::Abandoned => "idle",
        };
        Ok(report(outcome, Some(batch.id)))
    }

    /// The repository of the queue's project (through an entry's merge
    /// request); `None`: the project is gone.
    async fn repo_of(&self, entry: &StoredEntry) -> Result<Option<RepoId>, ForgeError> {
        let Some(target) =
            forge_db::review::refresh_target(&self.merger.db, entry.merge_request).await?
        else {
            return Ok(None);
        };
        if !target.repository_ready {
            return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
                .with_context("repo_id", target.repository.get())
                .with_context("queue_id", entry.queue_id));
        }
        Ok(Some(target.repository))
    }

    async fn read_tip(&self, repo: RepoId, name: &RefName) -> Result<Option<ObjectId>, ForgeError> {
        self.merger
            .git
            .read_ref(&self.merger.storage, repo, name, self.merger.timeouts.read)
            .await
    }

    /// Remove a queued entry the worker cannot batch, and log it.
    async fn drop_queued(
        &self,
        entry: &StoredEntry,
        reason: &str,
        code: ErrorCode,
    ) -> Result<(), ForgeError> {
        if db::remove_queued(&self.merger.db, entry.id, reason, code).await? {
            log_removed(entry, None, reason, code, &entry.request_id);
        }
        Ok(())
    }

    // ── building a batch ──

    async fn build(&self, claimed: &Claimed) -> Result<TickReport, ForgeError> {
        let queue = &claimed.queue;
        let report = |outcome, batch_id| TickReport {
            queue_id: queue.id,
            outcome,
            batch_id,
        };
        let queued = db::entries(&self.merger.db, queue.id, &[EntryState::Queued]).await?;
        let Some(first) = queued.first() else {
            return Ok(report("idle", None));
        };
        let Some(repo) = self.repo_of(first).await? else {
            return Ok(report("idle", None));
        };
        let Some(base) = self.read_tip(repo, &queue.target_branch).await? else {
            for entry in &queued {
                self.drop_queued(entry, "target_missing", ErrorCode::MergeQueueEntryRefused)
                    .await?;
            }
            return Ok(report("removed", None));
        };
        let mut members: Vec<(StoredEntry, ObjectId, Option<Isolation>)> = Vec::new();
        let mut prev = base.clone(); // clone: the train grows from the base
        let mut removed = false;
        for entry in &queued {
            if members.len() >= queue.batch_limit {
                break;
            }
            let Some(target) =
                forge_db::review::refresh_target(&self.merger.db, entry.merge_request).await?
            else {
                self.drop_queued(entry, "merge_request_gone", ErrorCode::NotFound)
                    .await?;
                removed = true;
                continue;
            };
            let mr = &target.merge_request;
            if mr.state != MergeRequestState::Open {
                self.drop_queued(entry, "not_open", ErrorCode::MergeQueueEntryRefused)
                    .await?;
                removed = true;
                continue;
            }
            if mr.head_sha.as_deref() != Some(entry.head_sha.as_str()) {
                self.drop_queued(entry, "head_changed", ErrorCode::HeadChanged)
                    .await?;
                removed = true;
                continue;
            }
            if mr.target_branch != queue.target_branch {
                self.drop_queued(entry, "retargeted", ErrorCode::MergeQueueEntryRefused)
                    .await?;
                removed = true;
                continue;
            }
            // M4 C9: the auto-merge principal's entry waits while an
            // emergency stop is on (it stays queued: paused) and leaves when
            // its project's mode no longer acts. People's entries go on.
            if entry.credential.is_auto_merge() {
                let auto =
                    forge_db::review::auto_merge::settings(&self.merger.db, entry.project_id)
                        .await?;
                let decided =
                    super::auto_proof(&self.merger.db, mr, &auto, Action::MergeRequestMerge).await;
                if let Err(e) = decided {
                    match super::auto_merge_paused(&e) {
                        // The emergency stop holds it: it stays queued.
                        Some("auto_merge_stopped") => continue,
                        Some(_) => {
                            self.drop_queued(
                                entry,
                                "auto_merge_off",
                                ErrorCode::MergeQueueEntryRefused,
                            )
                            .await?;
                            removed = true;
                            continue;
                        }
                        None if is_refusal(&e) => {
                            let reason = reason_of(&e);
                            self.drop_queued(entry, &reason, e.code()).await?;
                            removed = true;
                            continue;
                        }
                        None => return Err(e),
                    }
                }
            }
            let isolate = self.isolation(entry, repo, &base).await?;
            if isolate.is_some() && !members.is_empty() {
                break;
            }
            match self.train_step(entry, mr, repo, &prev).await? {
                Ok(commit) => {
                    prev = commit.clone(); // clone: the next step's parent
                    members.push((entry.clone(), commit, isolate)); // clone: the batch owns its view
                }
                Err(StepRefused::Remove(reason, code)) => {
                    self.drop_queued(entry, reason, code).await?;
                    removed = true;
                    continue;
                }
            }
            if isolate.is_some() {
                break;
            }
        }
        if members.is_empty() {
            return Ok(report(if removed { "removed" } else { "idle" }, None));
        }
        let train_ref = forge_git::hidden_refs::merge_queue_train_ref(queue.id)?;
        let current = self.read_tip(repo, &train_ref).await?;
        let change = match current {
            Some(old) if old == prev => None,
            Some(old) => Some(RefChange::Update {
                name: train_ref.clone(), // clone: the change owns its name
                new: prev.clone(),       // clone: as above
                old,
            }),
            None => Some(RefChange::Create {
                name: train_ref.clone(), // clone: as above
                new: prev.clone(),       // clone: as above
            }),
        };
        if let Some(change) = change {
            self.merger
                .git
                .update_refs(
                    &self.merger.storage,
                    repo,
                    &[change],
                    self.merger.timeouts.update_ref,
                )
                .await?;
        }
        let request_id = RequestId::generate();
        let pipeline = create_job(
            &PipelineRequest {
                project: queue.project_id,
                git_ref: queue.target_branch.clone(), // clone: the request owns its name
                sha: prev.clone(),                    // clone: as above
                source: Source::MergeQueue,
                merge_request: None,
                triggered_by: members.first().map(|(e, _, _)| e.enqueued_by),
            },
            request_id.clone(), // clone: the job keeps its origin
            None,
        )?;
        let new = NewBatch {
            queue: queue.id,
            project: queue.project_id,
            base: base.clone(),  // clone: the batch owns its ids
            train: prev.clone(), // clone: as above
            members: members
                .iter()
                .map(|(e, commit, isolate)| BatchMember {
                    entry: e.id,
                    train_sha: commit.clone(), // clone: as above
                    isolate: *isolate,
                })
                .collect(),
            request_id: request_id.clone(), // clone: the log line below
            deadline: self.limits.test_timeout,
            pipeline,
        };
        let Some(batch) = db::create_batch(&self.merger.db, &new).await? else {
            return Ok(report("raced", None));
        };
        let ids: Vec<String> = members.iter().map(|(e, _, _)| e.id.to_string()).collect();
        tracing::info!(
            target: "forge_review",
            event = "forge.merge_queue.batch_built",
            request_id = %request_id,
            queue_id = queue.id,
            batch_id = batch,
            project_id = queue.project_id.get(),
            target = queue.target_branch.as_str(),
            base_sha = base.as_str(),
            train_sha = prev.as_str(),
            size = members.len(),
            entries = %ids.join(","),
            isolate = members.first().and_then(|(_, _, i)| i.map(Isolation::as_str)),
        );
        Ok(report("built", Some(batch)))
    }

    /// Why `entry` must run alone (ADR-0030 §4): a fast-forward, a
    /// `forge/risk` stop on its head, or a migration in its diff (from the
    /// merge base with `base`; more paths than the bound, or a path that
    /// is no UTF-8, counts as one — fail closed).
    async fn isolation(
        &self,
        entry: &StoredEntry,
        repo: RepoId,
        base: &ObjectId,
    ) -> Result<Option<Isolation>, ForgeError> {
        if entry.method == MergeMethod::FastForward {
            return Ok(Some(Isolation::FastForward));
        }
        let runs =
            forge_db::checks::runs_for(&self.merger.db, entry.project_id, &entry.head_sha).await?;
        let risk = CheckName::parse(forge_core::checks::RISK_CHECK).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "risk_check_name")
        })?;
        // C9 review: a head forge/risk did not clear (a stop, an error, a
        // warning or a partial assessment) runs alone too.
        if runs.run(&entry.head_sha, &risk).is_some_and(|r| {
            r.conclusion.is_some() && r.conclusion != Some(CheckConclusion::Success)
        }) {
            return Ok(Some(Isolation::RiskStop));
        }
        let git = &self.merger.git;
        let read = self.merger.timeouts.read;
        let merge_base = git
            .merge_base(&self.merger.storage, repo, base, &entry.head_sha, read)
            .await?;
        let Some(merge_base) = merge_base else {
            return Ok(Some(Isolation::Migration));
        };
        let paths = git
            .changed_paths(
                &self.merger.storage,
                repo,
                &merge_base,
                &entry.head_sha,
                self.limits.max_paths,
                read,
            )
            .await?;
        Ok(match paths {
            ChangedPaths::Complete(paths) => paths
                .iter()
                .any(|p| forge_core::risk::is_migration_path(p))
                .then_some(Isolation::Migration),
            ChangedPaths::TooMany | ChangedPaths::NonUtf8 => Some(Isolation::Migration),
        })
    }

    /// The train's next commit: `entry` merged on `prev` as its method
    /// merges (the merge job's identities and the frozen message).
    async fn train_step(
        &self,
        entry: &StoredEntry,
        mr: &forge_db::review::StoredMergeRequest,
        repo: RepoId,
        prev: &ObjectId,
    ) -> Result<Result<ObjectId, StepRefused>, ForgeError> {
        let git = &self.merger.git;
        let storage = &self.merger.storage;
        match entry.method {
            MergeMethod::FastForward => {
                if prev == &entry.head_sha {
                    return Ok(Err(StepRefused::Remove(
                        "nothing_to_merge",
                        ErrorCode::MergeBlocked,
                    )));
                }
                let ff = git
                    .is_ancestor(
                        storage,
                        repo,
                        prev,
                        &entry.head_sha,
                        self.merger.timeouts.read,
                    )
                    .await?;
                if ff {
                    Ok(Ok(entry.head_sha.clone())) // clone: the train's step is the head
                } else {
                    Ok(Err(StepRefused::Remove(
                        "ff_not_possible",
                        ErrorCode::MergeBlocked,
                    )))
                }
            }
            MergeMethod::MergeCommit | MergeMethod::Squash => {
                let merged = git
                    .merge_tree(
                        storage,
                        repo,
                        prev,
                        &entry.head_sha,
                        MergeTreeLimits::default(),
                    )
                    .await?;
                let tree = match merged {
                    MergeTree::Conflicted { .. } => {
                        return Ok(Err(StepRefused::Remove(
                            "conflict",
                            ErrorCode::MergeRequestConflict,
                        )))
                    }
                    MergeTree::Clean { tree } => tree,
                };
                let merger = account_commit_identity(&self.merger.db, entry.enqueued_by).await?;
                let (author, parents) = match entry.method {
                    MergeMethod::Squash => (
                        account_commit_identity(&self.merger.db, mr.author).await?,
                        vec![prev.clone()], // clone: the commit owns its parents
                    ),
                    MergeMethod::MergeCommit | MergeMethod::FastForward => (
                        merger.clone(), // clone: author and committer are both the merger
                        vec![prev.clone(), entry.head_sha.clone()],
                    ),
                };
                let commit = git
                    .commit_tree(
                        storage,
                        repo,
                        &NewCommit {
                            tree: &tree,
                            parents: &parents,
                            message: &entry.message,
                            author: &author,
                            committer: &merger,
                        },
                        self.merger.timeouts.commit,
                    )
                    .await;
                match commit {
                    Ok(commit) => Ok(Ok(commit)),
                    // An identity git would misread (FG-REVIEW-0012).
                    Err(e) if is_refusal(&e) => {
                        Ok(Err(StepRefused::Remove("identity_invalid", e.code())))
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    // ── a testing batch ──

    async fn poll(
        &self,
        claimed: &Claimed,
        batch: &StoredBatch,
    ) -> Result<&'static str, ForgeError> {
        let entries = db::batch_entries(&self.merger.db, batch.id).await?;
        let testing: Vec<StoredEntry> = entries
            .into_iter()
            .filter(|e| e.state == EntryState::Testing)
            .collect();
        if testing.len() != batch.size {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "entry_left",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
        }
        let Some(first) = testing.first() else {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "entry_left",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
        };
        let Some(repo) = self.repo_of(first).await? else {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "project_gone",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
        };
        let mut gone = Vec::new();
        for entry in &testing {
            let target =
                forge_db::review::refresh_target(&self.merger.db, entry.merge_request).await?;
            let why = match target.as_ref().map(|t| &t.merge_request) {
                None => Some(("merge_request_gone", ErrorCode::NotFound)),
                Some(mr) if mr.state != MergeRequestState::Open => {
                    Some(("not_open", ErrorCode::MergeQueueEntryRefused))
                }
                Some(mr) if mr.head_sha.as_deref() != Some(entry.head_sha.as_str()) => {
                    Some(("head_changed", ErrorCode::HeadChanged))
                }
                Some(_) => None,
            };
            if let Some((reason, code)) = why {
                gone.push((entry.id, reason.to_owned(), code));
            }
        }
        if !gone.is_empty() {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "entry_left",
                    gone,
                    None,
                    Some(repo),
                )
                .await;
        }
        let tip = self.read_tip(repo, &claimed.queue.target_branch).await?;
        if tip.as_ref() != Some(&batch.base_sha) {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "target_moved",
                    Vec::new(),
                    None,
                    Some(repo),
                )
                .await;
        }
        let verdict =
            db::train_pipeline(&self.merger.db, batch.project_id, &batch.train_sha).await?;
        let status = match verdict {
            TrainPipeline::Planning => None,
            TrainPipeline::None => Some((None, PipelineStatus::Skipped)),
            TrainPipeline::Found { id, status } => Some((Some(id), status)),
        };
        match status {
            None | Some((_, PipelineStatus::Pending)) | Some((_, PipelineStatus::Running)) => {
                if batch.past_deadline {
                    return self
                        .reap(batch, &testing, status.and_then(|(id, _)| id), repo)
                        .await;
                }
                Ok("waiting")
            }
            Some((id, PipelineStatus::Failed))
            | Some((id, PipelineStatus::Invalid))
            | Some((id, PipelineStatus::Error))
            | Some((id, PipelineStatus::Canceled)) => {
                let status = status.map_or(PipelineStatus::Failed, |(_, s)| s);
                self.red(batch, &testing, id, status, repo).await
            }
            Some((id, green @ (PipelineStatus::Succeeded | PipelineStatus::Skipped))) => {
                let paused = db::queue_now(&self.merger.db, claimed.queue.id)
                    .await?
                    .is_some_and(|q| q.paused);
                if paused {
                    return Ok("paused");
                }
                // No pipeline row at all (no `.forge-ci.yml`): the landing's
                // decision finds `ci/pipeline` missing if it is required.
                let run = train_run(id.map(|_| green));
                self.land(claimed, batch, &testing, repo, run).await
            }
        }
    }

    /// Throw the batch away without a verdict, from `from` only (review
    /// HIGH 2: a `landing` batch is ended only under the merge lock, by the
    /// landing or the recovery); `removed` leave the queue (logged once, at
    /// warn, with their code and `context`), the rest go back in order. With
    /// `repo`, the train ref is deleted at the train.
    async fn abandon(
        &self,
        batch: &StoredBatch,
        from: BatchState,
        reason: &'static str,
        removed: Vec<(i64, String, ErrorCode)>,
        context: Option<(&StoredEntry, String)>,
        repo: Option<RepoId>,
    ) -> Result<&'static str, ForgeError> {
        let end = BatchEnd {
            from,
            state: BatchState::Abandoned,
            reason,
            error_code: None,
            removed,
            reaped: false,
            batch_limit: None,
        };
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            return Ok("raced");
        }
        {
            for (entry, why, code) in &end.removed {
                let (merge_request, detail) = match &context {
                    Some((e, detail)) if e.id == *entry => {
                        (Some(e.merge_request.get()), detail.as_str())
                    }
                    _ => (None, ""),
                };
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.merge_queue.entry_removed",
                    error_code = code.id(),
                    request_id = %batch.request_id,
                    queue_id = batch.queue_id,
                    batch_id = batch.id,
                    entry_id = entry,
                    merge_request_id = merge_request,
                    reason = %why,
                    context = detail,
                    "a merge request left the merge queue"
                );
            }
            tracing::info!(
                target: "forge_review",
                event = "forge.merge_queue.batch_abandoned",
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                reason,
                train_sha = batch.train_sha.as_str(),
            );
        }
        if let Some(repo) = repo {
            self.retire_train(repo, batch).await;
        }
        Ok("abandoned")
    }

    /// Delete `refs/merge-queue/<queue_id>/train` if it still points at
    /// `batch`'s train (a batch that ended; review MEDIUM). A failure is
    /// logged with its code: the maintenance sweep deletes it later.
    async fn retire_train(&self, repo: RepoId, batch: &StoredBatch) {
        let name = match forge_git::hidden_refs::merge_queue_train_ref(batch.queue_id) {
            Ok(name) => name,
            Err(e) => {
                telemetry::log_failure(&e, &batch.request_id);
                return;
            }
        };
        let gone = [forge_git::MergeQueueRef {
            queue_id: batch.queue_id,
            name,
            at: batch.train_sha.clone(), // clone: the deletion owns its id
        }];
        let deleted = self
            .merger
            .git
            .delete_merge_queue_refs(
                &self.merger.storage,
                repo,
                &gone,
                self.merger.timeouts.update_ref,
            )
            .await;
        if let Err(e) = deleted {
            // Moved by the next train, or already gone: nothing to do; any
            // other failure is the sweep's.
            tracing::warn!(
                target: "forge_review",
                event = "forge.merge_queue.train_ref_kept",
                error_code = e.code().id(),
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                train_sha = batch.train_sha.as_str(),
                "the train ref was not deleted (the maintenance sweep deletes a stale one)"
            );
        }
    }

    async fn red(
        &self,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        pipeline: Option<i64>,
        status: PipelineStatus,
        repo: RepoId,
    ) -> Result<&'static str, ForgeError> {
        let alone = testing.len() == 1;
        let removed: Vec<(i64, String, ErrorCode)> = if alone {
            testing
                .iter()
                .map(|e| {
                    (
                        e.id,
                        "checks_failed".to_owned(),
                        ErrorCode::MergeQueueChecksFailed,
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let limit = bisected_limit(testing.len());
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Failed,
            reason: "checks_failed",
            error_code: Some(ErrorCode::MergeQueueChecksFailed),
            removed,
            reaped: false,
            batch_limit: (!alone).then_some(limit),
        };
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            return Ok("raced");
        }
        self.retire_train(repo, batch).await;
        if alone {
            for entry in testing {
                log_removed(
                    entry,
                    Some((batch, pipeline, status)),
                    "checks_failed",
                    ErrorCode::MergeQueueChecksFailed,
                    &batch.request_id,
                );
            }
            return Ok("removed");
        }
        tracing::info!(
            target: "forge_review",
            event = "forge.merge_queue.batch_failed",
            error_code = ErrorCode::MergeQueueChecksFailed.id(),
            request_id = %batch.request_id,
            queue_id = batch.queue_id,
            batch_id = batch.id,
            size = testing.len(),
            next_batch_limit = limit,
            pipeline_id = pipeline,
            pipeline_status = status.as_str(),
            train_sha = batch.train_sha.as_str(),
        );
        Ok("bisected")
    }

    async fn reap(
        &self,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        pipeline: Option<i64>,
        repo: RepoId,
    ) -> Result<&'static str, ForgeError> {
        let end = BatchEnd {
            from: BatchState::Testing,
            state: BatchState::Abandoned,
            reason: "timeout",
            error_code: Some(ErrorCode::MergeQueueStalled),
            removed: Vec::new(),
            reaped: true,
            batch_limit: None,
        };
        if !db::end_batch(&self.merger.db, batch.id, &end).await? {
            return Ok("raced");
        }
        self.retire_train(repo, batch).await;
        let entries: Vec<String> = testing.iter().map(|e| e.id.to_string()).collect();
        telemetry::log_failure(
            &ForgeError::new(ErrorCode::MergeQueueStalled)
                .with_context("operation", "merge_queue_reap")
                .with_context("queue_id", batch.queue_id)
                .with_context("batch_id", batch.id)
                .with_context("train_sha", batch.train_sha.as_str().to_owned())
                .with_context("deadline", batch.deadline_at.clone()) // clone: the context owns it
                .with_context(
                    "pipeline_id",
                    pipeline.map_or_else(|| "none".to_owned(), |id| id.to_string()),
                )
                .with_context("entries", entries.join(",")),
            &batch.request_id,
        );
        for entry in testing.iter().filter(|e| e.reaps + 1 >= MAX_REAPS) {
            log_removed(
                entry,
                None,
                "stalled",
                ErrorCode::MergeQueueStalled,
                &batch.request_id,
            );
        }
        Ok("reaped")
    }

    // ── landing ──

    async fn land(
        &self,
        claimed: &Claimed,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        repo: RepoId,
        run: Option<CheckRun>,
    ) -> Result<&'static str, ForgeError> {
        let mut lock =
            merges::lock_merges(&self.merger.db, repo, self.merger.timeouts.lock_wait).await?;
        let landed = self
            .land_locked(&mut lock, claimed, batch, testing, repo, run)
            .await;
        lock.release().await;
        landed
    }

    async fn land_locked(
        &self,
        lock: &mut MergeLock,
        claimed: &Claimed,
        batch: &StoredBatch,
        testing: &[StoredEntry],
        repo: RepoId,
        run: Option<CheckRun>,
    ) -> Result<&'static str, ForgeError> {
        let target = &claimed.queue.target_branch;
        // Review MEDIUM: the emergency stop is read again under the lock.
        if db::queue_now(&self.merger.db, claimed.queue.id)
            .await?
            .is_some_and(|q| q.paused)
        {
            return Ok("paused");
        }
        let tip = self.read_tip(repo, target).await?;
        if tip.as_ref() != Some(&batch.base_sha) {
            return self
                .abandon(
                    batch,
                    BatchState::Testing,
                    "target_moved",
                    Vec::new(),
                    None,
                    Some(repo),
                )
                .await;
        }
        if !db::begin_landing(&self.merger.db, batch.id).await? {
            return Ok("raced");
        }
        let ctx = ChangeContext::new(Actor::System, batch.request_id.clone()); // clone: the context owns it
        let mut prev = batch.base_sha.clone(); // clone: the chain of the train
        let mut decided: Vec<(StoredEntry, StoredMerge, Subject)> = Vec::new();
        for entry in testing {
            let Some(train_sha) = entry.train_sha.clone() else {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "merge_queue_entry_without_train")
                    .with_context("entry_id", entry.id));
            };
            let step = QueueStep {
                expected_old: prev.clone(), // clone: the step owns its ids
                new_sha: train_sha.clone(), // clone: as above
            };
            let one = self
                .decide_entry(lock, &ctx, (entry, &step), (batch, target), repo, run)
                .await;
            match one {
                Ok((attempt, subject)) => {
                    decided.push((entry.clone(), attempt, subject)); // clone: the record needs it
                    prev = train_sha;
                }
                Err(refusal) => {
                    self.end_attempts(&decided, "queue_rebuild").await;
                    // M4 C9: an emergency stop turned on while the batch
                    // tested holds the auto-merge entries (back in the
                    // queue, paused); a mode that no longer acts removes it.
                    match super::auto_merge_paused(&refusal) {
                        Some("auto_merge_stopped") => {
                            return self
                                .abandon(
                                    batch,
                                    BatchState::Landing,
                                    "auto_merge_stopped",
                                    Vec::new(),
                                    None,
                                    Some(repo),
                                )
                                .await;
                        }
                        Some(_) => {
                            let removed = vec![(
                                entry.id,
                                "auto_merge_off".to_owned(),
                                ErrorCode::MergeQueueEntryRefused,
                            )];
                            return self
                                .abandon(
                                    batch,
                                    BatchState::Landing,
                                    "entry_refused",
                                    removed,
                                    None,
                                    Some(repo),
                                )
                                .await;
                        }
                        None => {}
                    }
                    if is_refusal(&refusal) {
                        let removed = vec![(entry.id, reason_of(&refusal), refusal.code())];
                        let detail = refusal_context(&refusal);
                        return self
                            .abandon(
                                batch,
                                BatchState::Landing,
                                "entry_refused",
                                removed,
                                Some((entry, detail)),
                                Some(repo),
                            )
                            .await;
                    }
                    self.abandon(
                        batch,
                        BatchState::Landing,
                        "landing_failed",
                        Vec::new(),
                        None,
                        Some(repo),
                    )
                    .await?;
                    return Err(refusal);
                }
            }
        }
        // Review MEDIUM (git): the steps decided must chain to the tested
        // train exactly, or the target would move to something else.
        if prev != batch.train_sha {
            self.end_attempts(&decided, "queue_rebuild").await;
            self.abandon(
                batch,
                BatchState::Landing,
                "train_mismatch",
                Vec::new(),
                None,
                Some(repo),
            )
            .await?;
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "merge_queue_train_mismatch")
                .with_context("queue_batch_id", batch.id)
                .with_context("chained", prev.as_str().to_owned())
                .with_context("train_sha", batch.train_sha.as_str().to_owned()));
        }
        // The fence: every attempt `executing` before the ref moves.
        for (_, attempt, _) in &decided {
            if !merges::fence_merge(&self.merger.db, attempt.id).await? {
                self.end_attempts(&decided, "queue_rebuild").await;
                return self
                    .abandon(
                        batch,
                        BatchState::Landing,
                        "landing_raced",
                        Vec::new(),
                        None,
                        Some(repo),
                    )
                    .await;
            }
        }
        if let Err(e) = lock.alive().await {
            self.end_attempts(&decided, "queue_rebuild").await;
            self.abandon(
                batch,
                BatchState::Landing,
                "landing_failed",
                Vec::new(),
                None,
                Some(repo),
            )
            .await?;
            return Err(e);
        }
        let change = RefChange::Update {
            name: target.clone(),         // clone: the change owns its name
            new: batch.train_sha.clone(), // clone: as above
            old: batch.base_sha.clone(),  // clone: as above
        };
        let timeout = self.merger.bounded(self.merger.timeouts.update_ref, lock)?;
        let moved = self
            .merger
            .git
            .update_refs(&self.merger.storage, repo, &[change], timeout)
            .await;
        // Every outcome may have changed the ref: drop the browse caches.
        self.merger.invalidate(repo);
        if let Err(cause) = moved {
            let now = self.read_tip(repo, target).await?;
            if now.as_ref() != Some(&batch.train_sha) {
                self.end_attempts(&decided, "not_applied").await;
                let reason = if now.as_ref() == Some(&batch.base_sha) {
                    "landing_failed"
                } else {
                    "target_moved"
                };
                self.abandon(
                    batch,
                    BatchState::Landing,
                    reason,
                    Vec::new(),
                    None,
                    Some(repo),
                )
                .await?;
                if reason == "target_moved" {
                    return Ok("abandoned");
                }
                return Err(cause.with_context("queue_batch_id", batch.id));
            }
        }
        if self.crash_after_cas {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "test_crash_after_cas")
                .with_context("queue_batch_id", batch.id));
        }
        self.record_all(lock, batch, &decided, repo).await?;
        Ok("landed")
    }

    /// One entry's landing decision and its `prepared` attempt.
    async fn decide_entry(
        &self,
        lock: &MergeLock,
        ctx: &ChangeContext,
        (entry, step): (&StoredEntry, &QueueStep),
        (batch, queue_target): (&StoredBatch, &RefName),
        repo: RepoId,
        run: Option<CheckRun>,
    ) -> Result<(StoredMerge, Subject), ForgeError> {
        let Some(target) =
            forge_db::review::refresh_target(&self.merger.db, entry.merge_request).await?
        else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_request")
                .with_context("reason", "merge_request_gone"));
        };
        let mr = target.merge_request;
        let read = self.merger.bounded(self.merger.timeouts.read, lock)?;
        let source = self
            .merger
            .git
            .read_ref(&self.merger.storage, repo, &mr.source_branch, read)
            .await?;
        let review_git = crate::approvals::ReviewGit::new(
            self.merger.git.clone(),     // clone: Arc handles
            self.merger.storage.clone(), // clone: the root path
        );
        let asker = crate::approvals::Asker {
            request_id: &batch.request_id,
            project: entry.project_id,
            repo,
        };
        let approvals = review_git
            .facts(
                &self.merger.db,
                asker,
                &mr,
                &entry.head_sha,
                Some(&batch.base_sha),
                &mut crate::approvals::Budget::until(
                    std::time::Instant::now() + self.merger.bounded(lock.left(), lock)?,
                    crate::approvals::JOB_CALLS,
                ),
            )
            .await?;
        let ff = (entry.method == MergeMethod::FastForward).then_some(true);
        let (attempt, ()) =
            merges::queue_merge_mr_decided(&self.merger.db, ctx, entry, step, |gate| {
                let asked = Asked {
                    requested_by: entry.enqueued_by,
                    head: &entry.head_sha,
                    method: entry.method,
                    target: queue_target,
                    source_tip: source.as_ref(),
                    target_tip: Some(&batch.base_sha),
                    approvals,
                };
                let runs = TrainRuns {
                    inner: &gate.check_runs,
                    train: run,
                };
                decide_queued(
                    gate,
                    &asked,
                    Some(super::super::evaluate::Mergeable::Clean),
                    ff,
                    &runs,
                )
            })
            .await?;
        Ok((attempt, Subject { mr, repo }))
    }

    /// End the attempts a failed landing wrote (never moved the ref).
    async fn end_attempts(&self, decided: &[(StoredEntry, StoredMerge, Subject)], reason: &str) {
        for (entry, attempt, _) in decided {
            let ended = merges::end_merge(
                &self.merger.db,
                attempt.id,
                &[MergeState::Prepared, MergeState::Executing],
                MergeState::Superseded,
                reason,
                None,
            )
            .await;
            if let Err(e) = ended {
                // The reconciliation settles a live attempt later (it asks git).
                telemetry::log_failure(
                    &e.with_context("operation", "merge_queue_end_attempt")
                        .with_context("merge_id", attempt.id)
                        .with_context("entry_id", entry.id),
                    &attempt.request_id,
                );
            }
        }
    }

    /// The normal merge record per entry, in train order (ADR-0030 §6.5),
    /// then the entries `merged` and the batch `landed`.
    async fn record_all(
        &self,
        lock: &MergeLock,
        batch: &StoredBatch,
        decided: &[(StoredEntry, StoredMerge, Subject)],
        repo: RepoId,
    ) -> Result<(), ForgeError> {
        for (entry, attempt, subject) in decided {
            let executing = StoredMerge {
                state: MergeState::Executing,
                ..attempt.clone() // clone: the executing view of the same attempt
            };
            let report = self.merger.record(lock, &executing, subject, &[]).await?;
            if !db::entry_merged(&self.merger.db, entry.id).await? {
                // Review MEDIUM: the merge is recorded (the merge request is
                // merged) but the entry was not `testing` with its attempt —
                // a broken invariant, logged with its code for a person.
                telemetry::log_failure(
                    &ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "merge_queue_entry_not_marked")
                        .with_context("entry_id", entry.id)
                        .with_context("merge_id", attempt.id)
                        .with_context("queue_batch_id", batch.id),
                    &batch.request_id,
                );
            }
            tracing::info!(
                target: "forge_review",
                event = "forge.merge_queue.entry_merged",
                request_id = %batch.request_id,
                queue_id = batch.queue_id,
                batch_id = batch.id,
                entry_id = entry.id,
                merge_id = attempt.id,
                merge_request_id = entry.merge_request.get(),
                outcome = report.outcome,
                new_sha = report.new_sha.as_ref().map(ObjectId::as_str),
            );
        }
        if !db::batch_landed(&self.merger.db, batch.id).await? {
            telemetry::log_failure(
                &ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "merge_queue_batch_not_landing")
                    .with_context("queue_batch_id", batch.id),
                &batch.request_id,
            );
        }
        self.retire_train(repo, batch).await;
        tracing::info!(
            target: "forge_review",
            event = "forge.merge_queue.batch_landed",
            request_id = %batch.request_id,
            queue_id = batch.queue_id,
            batch_id = batch.id,
            size = decided.len(),
            base_sha = batch.base_sha.as_str(),
            train_sha = batch.train_sha.as_str(),
        );
        Ok(())
    }

    /// A batch left `landing` by a worker that stopped (ADR-0030 §14):
    /// under the merge lock, the target at the train records what is left;
    /// anything else ends the attempts and rebuilds.
    async fn recover(
        &self,
        claimed: &Claimed,
        batch: &StoredBatch,
    ) -> Result<&'static str, ForgeError> {
        let entries = db::batch_entries(&self.merger.db, batch.id).await?;
        let Some(first) = entries.first() else {
            return self
                .abandon(
                    batch,
                    BatchState::Landing,
                    "entry_left",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
        };
        let Some(repo) = self.repo_of(first).await? else {
            return self
                .abandon(
                    batch,
                    BatchState::Landing,
                    "project_gone",
                    Vec::new(),
                    None,
                    None,
                )
                .await;
        };
        let lock = match merges::try_lock_merges(&self.merger.db, repo).await? {
            Some(lock) => lock,
            // A landing still runs (its worker holds the lock): later.
            None => return Ok("waiting"),
        };
        let settled = self
            .recover_locked(&lock, claimed, batch, &entries, repo)
            .await;
        lock.release().await;
        settled
    }

    async fn recover_locked(
        &self,
        lock: &MergeLock,
        claimed: &Claimed,
        batch: &StoredBatch,
        entries: &[StoredEntry],
        repo: RepoId,
    ) -> Result<&'static str, ForgeError> {
        let tip = self.read_tip(repo, &claimed.queue.target_branch).await?;
        let mut attempts = Vec::new();
        // Review HIGH 1: every entry of the batch counts, whatever its state
        // now — an attempt that moved the target is recorded.
        for entry in entries {
            let attempt = match entry.merge_id {
                Some(id) => merges::merge_attempt(&self.merger.db, id).await?,
                None => None,
            };
            attempts.push((entry, attempt));
        }
        let landed = tip.as_ref() == Some(&batch.train_sha)
            && attempts.len() == batch.size
            && attempts.iter().all(|(_, a)| a.is_some());
        if !landed {
            for (entry, attempt) in &attempts {
                let Some(attempt) = attempt else { continue };
                let ended = merges::end_merge(
                    &self.merger.db,
                    attempt.id,
                    &[MergeState::Prepared, MergeState::Executing],
                    MergeState::Superseded,
                    "queue_rebuild",
                    None,
                )
                .await;
                if let Err(e) = ended {
                    telemetry::log_failure(
                        &e.with_context("operation", "merge_queue_end_attempt")
                            .with_context("entry_id", entry.id),
                        &batch.request_id,
                    );
                }
            }
            let reason = if tip.as_ref() == Some(&batch.base_sha) {
                "landing_interrupted"
            } else {
                "target_moved"
            };
            self.abandon(
                batch,
                BatchState::Landing,
                reason,
                Vec::new(),
                None,
                Some(repo),
            )
            .await?;
            return Ok("recovered");
        }
        self.merger.invalidate(repo);
        let mut decided = Vec::new();
        for (entry, attempt) in attempts {
            let Some(attempt) = attempt else { continue };
            if attempt.state == MergeState::Prepared
                && !merges::fence_merge(&self.merger.db, attempt.id).await?
            {
                return Ok("raced");
            }
            let subject = forge_db::review::refresh_target(&self.merger.db, entry.merge_request)
                .await?
                .map(|t| Subject {
                    mr: t.merge_request,
                    repo,
                });
            let Some(subject) = subject else { continue };
            if attempt.state == MergeState::Applied {
                let _marked = db::entry_merged(&self.merger.db, entry.id).await?;
                continue;
            }
            decided.push((entry.clone(), attempt, subject)); // clone: the record needs it
        }
        self.record_all(lock, batch, &decided, repo).await?;
        Ok("recovered")
    }
}

fn log_removed(
    entry: &StoredEntry,
    pipeline: Option<(&StoredBatch, Option<i64>, PipelineStatus)>,
    reason: &str,
    code: ErrorCode,
    request_id: &RequestId,
) {
    tracing::warn!(
        target: "forge_review",
        event = "forge.merge_queue.entry_removed",
        error_code = code.id(),
        request_id = %request_id,
        queue_id = entry.queue_id,
        batch_id = pipeline.map(|(b, _, _)| b.id),
        entry_id = entry.id,
        merge_request_id = entry.merge_request.get(),
        reason,
        train_sha = pipeline.map(|(b, _, _)| b.train_sha.as_str()),
        pipeline_id = pipeline.and_then(|(_, id, _)| id),
        pipeline_status = pipeline.map(|(_, _, s)| s.as_str()),
    );
}

/// A refusal's context as one `key=value` line (the removal's log).
fn refusal_context(refusal: &ForgeError) -> String {
    refusal
        .context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}
