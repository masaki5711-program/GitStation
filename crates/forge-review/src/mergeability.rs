//! Whether a merge request can be merged (M3 R2, ADR-0024 §3): the body of
//! the job `merge_request.mergeability` (`forge worker` runs it), its
//! end-of-life path, and the version job's request after a version.
//!
//! 1. [`forge_db::review::begin_check`]: a run whose `check_seq` is no
//!    longer the latest stops (`superseded`, nothing written — the newer
//!    job decides); a closed or merged merge request is not checked;
//!    otherwise the status is `checking`.
//! 2. With forge-git, against the merge request's RECORDED head (the last
//!    version, also when the source branch was deleted since — its hidden
//!    ref keeps it) and the target branch's current tip:
//!    - no version yet → `check_failed reason=no_version`;
//!    - the target branch is gone → `check_failed reason=target_missing`;
//!    - no common history (`merge-base` exit 1) → `check_failed
//!      reason=unrelated_histories` (never "mergeable");
//!    - `merge-tree --write-tree`: clean → `can_be_merged`, conflict (or an
//!      output past its cap) → `cannot_be_merged reason=conflict` with the
//!      paths (at most [`MAX_STORED_FILES`] and [`MAX_STORED_BYTES`];
//!      `conflicts_truncated` beyond).
//! 3. [`forge_db::review::record_check`] stores it while the seq is still
//!    the latest, the merge request open and its head the one checked; a
//!    can ⇄ cannot flip writes `merge_request.conflict_*`.
//! 4. M4 C7 (plan 04 M-1, ADR-0024 Amendment 2026-11-11), when stored:
//!    a clean merge becomes the merge result — the commit of the trial
//!    merge's tree with the parents (target tip, head), written once per
//!    (head, target tip) pair (an earlier run's row is reused, no second
//!    commit, while its commit exists — a pruned one is written again and
//!    replaces it in the row, with a pipeline on it), stored in `merge_result_refs` with the merge request
//!    pipeline on it ([`forge_db::review::record_merge_result`]), and
//!    `refs/merge-requests/<iid>/merge` pointed at it. A conflict (or no
//!    target, unrelated histories, no version) deletes the ref: no merge
//!    result. A git failure leaves the ref as it is (the stored status
//!    already blocks the merge). The ref moves only while this run's seq is
//!    still the latest (a newer run owns it otherwise), decided and written
//!    under the merge request's refs lock (`lock_version_refs`), so two
//!    runs never interleave between the two. A failure here is
//!    the job's (retried; the status is already stored, and a missing merge
//!    result never makes a check count).
//!
//! A git failure (`FG-GIT-0002` timeout, `0004` busy, `0003` process) is
//! stored as `check_failed` with `reason=git_timeout|git_busy|git_failed`
//! and returned: the worker logs it once with its code and retries with
//! backoff. A run that fails for good ([`fail`]: a permanent failure or
//! the last attempt) ends its request as `check_failed reason=job_failed`
//! and logs `FG-REVIEW-0003`; a run that never returns (timeout, crash) is
//! ended by the worker's sweep (`reap_stale_checks`, `job_stale`). A
//! decision is logged once at info, an undecided result at warn
//! (`forge.review.mergeability_checked`, with the stored — capped — paths
//! and the full count).

use std::time::{Duration, Instant};

use forge_core::{CheckReason, MergeabilityStatus, ObjectId, RefName, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{self, CheckResult, CheckStart, MergeabilityRequest, Recorded, RefsLock};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::merge_result::MergeRefChange;
use forge_git::mergeability::{MergeTree, MergeTreeLimits};
use forge_git::{Git, StorageRoot};

use crate::versions::ACTOR_LABEL;

pub(crate) use crate::after_version::after_version;

/// Conflicted paths stored at most (the log line carries the count).
pub const MAX_STORED_FILES: usize = 100;
/// Bytes of stored paths at most (one path is cut to fit, never stored
/// half; the table's CHECK allows 64 KiB).
pub const MAX_STORED_BYTES: usize = 16 * 1024;
/// The ref read and `merge-base` of a check.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The merge result's `commit-tree` and its ref update (M4 C7).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The time a git step under the refs lock may take: [`WRITE_TIMEOUT`],
/// cut to what is left of the lock's hold.
fn left(lock: &RefsLock) -> Duration {
    lock.deadline()
        .saturating_duration_since(tokio::time::Instant::now())
        .min(WRITE_TIMEOUT)
}

/// Where the check runs: git, the storage, the trial merge's limits.
#[derive(Clone)]
pub struct Mergeability {
    git: Git,
    storage: StorageRoot,
    limits: MergeTreeLimits,
}

impl std::fmt::Debug for Mergeability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mergeability")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// What a run did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// `checked`, `superseded`, `not_open` or `gone`.
    pub outcome: &'static str,
    pub status: Option<MergeabilityStatus>,
    pub reason: Option<CheckReason>,
    pub flip: Option<forge_core::EventType>,
    /// M4 C7: the merge result of the checked pair (`None`: not clean, or
    /// not stored).
    pub merge_result: Option<ObjectId>,
    /// M4 C7: what happened to `refs/merge-requests/<iid>/merge`
    /// (`None`: not touched).
    pub merge_ref: Option<MergeRefChange>,
}

impl CheckReport {
    fn skipped(outcome: &'static str) -> Self {
        Self {
            outcome,
            status: None,
            reason: None,
            flip: None,
            merge_result: None,
            merge_ref: None,
        }
    }
}

/// The stored paths of a conflict: at most [`MAX_STORED_FILES`] and
/// [`MAX_STORED_BYTES`]; `true` when some were left out.
pub fn stored_files(files: &[String]) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for f in files {
        if kept.len() >= MAX_STORED_FILES || bytes + f.len() > MAX_STORED_BYTES {
            return (kept, true);
        }
        bytes += f.len();
        kept.push(f.clone()); // clone: the stored list owns its paths
    }
    (kept, false)
}

/// The reason a git failure is stored with.
pub fn git_reason(e: &ForgeError) -> CheckReason {
    let code = e.code();
    if code == ErrorCode::GitTimeout {
        CheckReason::GitTimeout
    } else if code == ErrorCode::GitBusy {
        CheckReason::GitBusy
    } else {
        CheckReason::GitFailed
    }
}

/// What the git part decided (with how many paths git listed in all), or
/// the git failure.
enum Computed {
    /// `tree`: the clean trial merge's tree (M4 C7: the merge result's).
    Done {
        result: CheckResult,
        files: usize,
        tree: Option<ObjectId>,
    },
    Failed(CheckResult, ForgeError),
}

fn failed(reason: CheckReason, head: Option<&ObjectId>, target: Option<&ObjectId>) -> CheckResult {
    CheckResult {
        status: MergeabilityStatus::CheckFailed,
        reason: Some(reason),
        head: head.cloned(),     // clone: the result owns its ids
        target: target.cloned(), // clone: as above
        conflicts: Vec::new(),
        conflicts_truncated: false,
    }
}

fn done(result: CheckResult) -> Computed {
    Computed::Done {
        result,
        files: 0,
        tree: None,
    }
}

impl Mergeability {
    pub fn new(git: Git, storage: StorageRoot) -> Self {
        Self {
            git,
            storage,
            limits: MergeTreeLimits::default(),
        }
    }

    /// Other trial-merge limits (tests).
    #[must_use]
    pub fn with_limits(mut self, limits: MergeTreeLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The git part (module docs, step 2) for `head` against `target`.
    async fn compute(&self, repo: RepoId, head: Option<&ObjectId>, target: &RefName) -> Computed {
        let Some(head) = head else {
            return done(failed(CheckReason::NoVersion, None, None));
        };
        let tip = match self
            .git
            .read_ref(&self.storage, repo, target, READ_TIMEOUT)
            .await
        {
            Ok(tip) => tip,
            Err(e) => return Computed::Failed(failed(git_reason(&e), Some(head), None), e),
        };
        let Some(tip) = tip else {
            return done(failed(CheckReason::TargetMissing, Some(head), None));
        };
        match self
            .git
            .merge_base(&self.storage, repo, &tip, head, READ_TIMEOUT)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return done(failed(
                    CheckReason::UnrelatedHistories,
                    Some(head),
                    Some(&tip),
                ));
            }
            Err(e) => {
                return Computed::Failed(failed(git_reason(&e), Some(head), Some(&tip)), e);
            }
        }
        match self
            .git
            .merge_tree(&self.storage, repo, &tip, head, self.limits)
            .await
        {
            Ok(MergeTree::Clean { tree }) => Computed::Done {
                files: 0,
                tree: Some(tree),
                result: CheckResult {
                    status: MergeabilityStatus::CanBeMerged,
                    reason: None,
                    head: Some(head.clone()), // clone: the result owns its ids
                    target: Some(tip),
                    conflicts: Vec::new(),
                    conflicts_truncated: false,
                },
            },
            Ok(MergeTree::Conflicted {
                files,
                files_truncated,
            }) => {
                let (conflicts, cut) = stored_files(&files);
                Computed::Done {
                    tree: None,
                    files: files.len(),
                    result: CheckResult {
                        status: MergeabilityStatus::CannotBeMerged,
                        reason: Some(CheckReason::Conflict),
                        head: Some(head.clone()), // clone: as above
                        target: Some(tip),
                        conflicts,
                        conflicts_truncated: files_truncated || cut,
                    },
                }
            }
            Err(e) => Computed::Failed(failed(git_reason(&e), Some(head), Some(&tip)), e),
        }
    }
}

pub(crate) fn system(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    ctx
}

/// The mergeability job (module docs). Errors: forge-db's (`FG-DB-0001` /
/// `0002`), `FG-REPO-0016` for a repository not ready, and the git failure
/// after it was stored as `check_failed`; each is the job's failure (the
/// worker logs it once with the job's request id).
pub async fn check(
    db: &Db,
    checker: &Mergeability,
    request: MergeabilityRequest,
    request_id: &RequestId,
) -> Result<CheckReport, ForgeError> {
    let started = Instant::now();
    let mr = request.merge_request;
    let seq = request.check_seq;
    let subject = match review::begin_check(db, mr, seq).await? {
        CheckStart::Superseded { .. } => return Ok(CheckReport::skipped("superseded")),
        CheckStart::NotOpen(None) => return Ok(CheckReport::skipped("gone")),
        CheckStart::NotOpen(Some(_)) => return Ok(CheckReport::skipped("not_open")),
        CheckStart::Check(subject) => subject,
    };
    let Some(target) = review::refresh_target(db, mr).await? else {
        return Ok(CheckReport::skipped("gone"));
    };
    if !target.repository_ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", target.repository.get())
            .with_context("merge_request_id", mr.get()));
    }
    let computed = checker
        .compute(
            target.repository,
            subject.head.as_ref(),
            &subject.target_branch,
        )
        .await;
    let (result, files, tree, git_error) = match computed {
        Computed::Done {
            result,
            files,
            tree,
        } => (result, files, tree, None),
        Computed::Failed(result, e) => (result, 0, None, Some(e)),
    };
    let recorded = review::record_check(db, &system(request_id), mr, seq, &result).await;
    let recorded = match (recorded, git_error) {
        (Ok(_), Some(e)) => {
            return Err(e
                .with_context("merge_request_id", mr.get())
                .with_context("check_seq", seq)
                .with_context("stored", "check_failed"));
        }
        (Err(db_error), Some(e)) => {
            return Err(db_error.with_context("git_error", e.code().id()));
        }
        (Err(db_error), None) => return Err(db_error),
        (Ok(recorded), None) => recorded,
    };
    let (outcome, flip) = match recorded {
        Recorded::Superseded => ("superseded", None),
        Recorded::Stored { flip } => ("checked", flip),
    };
    let (merge_result, merge_ref) = if outcome == "checked" {
        checker
            .merge_result(
                db,
                (target.repository, mr, subject.iid, seq),
                &result,
                tree.as_ref(),
                request_id,
            )
            .await
            .map_err(|e| {
                e.with_context("merge_request_id", mr.get())
                    .with_context("check_seq", seq)
                    .with_context("stage", "merge_result")
            })?
    } else {
        (None, None)
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let reason = result.reason.map(CheckReason::as_str);
    let head_sha = result.head.as_ref().map(ObjectId::as_str);
    let target_sha = result.target.as_ref().map(ObjectId::as_str);
    if result.status.is_decided() || outcome == "superseded" {
        tracing::info!(
            target: "forge_review",
            event = "forge.review.mergeability_checked",
            request_id = %request_id,
            merge_request_id = mr.get(),
            project_id = subject.project.get(),
            iid = subject.iid,
            check_seq = seq,
            outcome,
            status = result.status.as_str(),
            reason,
            head_sha,
            target_sha,
            conflict_files = files,
            conflicts_truncated = result.conflicts_truncated,
            files = ?result.conflicts,
            flip = flip.map(forge_core::EventType::as_str),
            duration_ms,
            "merge request mergeability checked"
        );
    } else {
        // Undecided (target missing, unrelated, no version): the merge is
        // blocked on it; say why at warn.
        tracing::warn!(
            target: "forge_review",
            event = "forge.review.mergeability_checked",
            request_id = %request_id,
            merge_request_id = mr.get(),
            project_id = subject.project.get(),
            iid = subject.iid,
            check_seq = seq,
            outcome,
            status = result.status.as_str(),
            reason,
            head_sha,
            target_sha,
            duration_ms,
            "merge request mergeability could not be decided"
        );
    }
    Ok(CheckReport {
        outcome,
        status: Some(result.status),
        reason: result.reason,
        flip,
        merge_result,
        merge_ref,
    })
}

/// Whether a stored result says "no merge result" (the ref is deleted): a
/// decision that is not clean, or no target / no history / no version. A
/// git failure is not (the ref is left as it is).
fn no_merge_result(result: &CheckResult) -> bool {
    match result.status {
        MergeabilityStatus::CannotBeMerged => true,
        MergeabilityStatus::CheckFailed => matches!(
            result.reason,
            Some(
                CheckReason::TargetMissing
                    | CheckReason::UnrelatedHistories
                    | CheckReason::NoVersion
            )
        ),
        MergeabilityStatus::CanBeMerged
        | MergeabilityStatus::Unchecked
        | MergeabilityStatus::Checking => false,
    }
}

impl Mergeability {
    /// Step 4 of the module docs for a stored `result` of request `seq`;
    /// the merge result and what happened to the ref. Runs under the merge
    /// request's refs lock (`forge_db::review::lock_version_refs`, the lock
    /// of every hidden ref of a merge request): "still the latest" and the
    /// ref write are one step, so a run that loses the race never moves the
    /// ref back to an older pair (review MEDIUM). A busy lock is
    /// `FG-DB-0002` (the job retries).
    async fn merge_result(
        &self,
        db: &Db,
        subject: (RepoId, forge_core::MergeRequestId, i64, i64),
        result: &CheckResult,
        tree: Option<&ObjectId>,
        request_id: &RequestId,
    ) -> Result<(Option<ObjectId>, Option<MergeRefChange>), ForgeError> {
        let mut lock = review::lock_version_refs(db, subject.1).await?;
        let out = self
            .merge_result_locked(db, &mut lock, subject, (result, tree), request_id)
            .await;
        lock.release().await;
        out
    }

    /// [`Self::merge_result`] under `lock`.
    async fn merge_result_locked(
        &self,
        db: &Db,
        lock: &mut RefsLock,
        (repo, mr, iid, seq): (RepoId, forge_core::MergeRequestId, i64, i64),
        (result, tree): (&CheckResult, Option<&ObjectId>),
        request_id: &RequestId,
    ) -> Result<(Option<ObjectId>, Option<MergeRefChange>), ForgeError> {
        let clean = match (result.status, tree, &result.head, &result.target) {
            (MergeabilityStatus::CanBeMerged, Some(tree), Some(head), Some(tip)) => {
                Some((tree, head, tip))
            }
            _ => None,
        };
        let Some((tree, head, tip)) = clean else {
            if !no_merge_result(result) || !self.still_latest(db, mr, seq).await? {
                return Ok((None, None));
            }
            lock.alive().await?;
            let change = self
                .git
                .set_merge_result_ref(&self.storage, repo, iid, None, left(lock))
                .await?;
            if let MergeRefChange::Deleted { from } = &change {
                tracing::info!(
                    target: "forge_review",
                    event = "forge.review.merge_result_removed",
                    request_id = %request_id,
                    merge_request_id = mr.get(),
                    iid,
                    check_seq = seq,
                    status = result.status.as_str(),
                    reason = result.reason.map(CheckReason::as_str),
                    previous_sha = from.as_str(),
                    "no merge result: the merge ref was deleted"
                );
            }
            return Ok((None, Some(change)));
        };
        // Once per pair: an earlier run's result is reused (no commit) —
        // while its commit exists (`gc` may have pruned it once the ref moved
        // on; then a fresh one replaces it in the row, review MEDIUM).
        let reused = review::merge_result_of(db, mr, head, tip).await?;
        let recorded = match reused {
            Some(sha)
                if self
                    .git
                    .merge_result_exists(&self.storage, repo, &sha, left(lock))
                    .await? =>
            {
                review::record_merge_result(db, request_id, mr, (head, tip), &sha).await?
            }
            Some(pruned) => {
                let fresh = self
                    .git
                    .write_merge_result(&self.storage, repo, iid, (tree, tip, head), left(lock))
                    .await?;
                tracing::warn!(
                    target: "forge_review",
                    event = "forge.review.merge_result_rewritten",
                    request_id = %request_id,
                    merge_request_id = mr.get(),
                    iid,
                    check_seq = seq,
                    head_sha = head.as_str(),
                    target_sha = tip.as_str(),
                    pruned_sha = pruned.as_str(),
                    result_sha = fresh.as_str(),
                    "a stored merge result was pruned; wrote it again"
                );
                review::replace_merge_result(db, request_id, mr, (head, tip), (&pruned, &fresh))
                    .await?
            }
            None => {
                let written = self
                    .git
                    .write_merge_result(&self.storage, repo, iid, (tree, tip, head), left(lock))
                    .await?;
                review::record_merge_result(db, request_id, mr, (head, tip), &written).await?
            }
        };
        let Some(recorded) = recorded else {
            return Ok((None, None));
        };
        if !self.still_latest(db, mr, seq).await? {
            // A newer run owns the ref; the row (and pipeline) stay valid
            // for their pair.
            return Ok((Some(recorded.result), None));
        }
        lock.alive().await?;
        let change = self
            .git
            .set_merge_result_ref(&self.storage, repo, iid, Some(&recorded.result), left(lock))
            .await?;
        if recorded.created || change != MergeRefChange::Unchanged {
            tracing::info!(
                target: "forge_review",
                event = "forge.review.merge_result_updated",
                request_id = %request_id,
                merge_request_id = mr.get(),
                iid,
                check_seq = seq,
                head_sha = head.as_str(),
                target_sha = tip.as_str(),
                result_sha = recorded.result.as_str(),
                created = recorded.created,
                pipeline_enqueued = recorded.pipeline_enqueued,
                merge_ref = change.as_str(),
                "merge result written (refs/merge-requests/<iid>/merge)"
            );
        }
        Ok((Some(recorded.result), Some(change)))
    }

    /// Whether `seq` is still `mr`'s latest request (the ref belongs to it).
    async fn still_latest(
        &self,
        db: &Db,
        mr: forge_core::MergeRequestId,
        seq: i64,
    ) -> Result<bool, ForgeError> {
        Ok(review::mergeability_written(db, mr)
            .await?
            .is_some_and(|s| s.check_seq == seq))
    }
}

/// The job failed for good (`cause`: its last error): end its request as
/// `check_failed reason=job_failed` and log `FG-REVIEW-0003` once (the
/// worker logs `cause` itself). A request already superseded or decided is
/// left alone. A failure to write is logged here and not returned: the
/// sweep's reaper ends the row later (`job_stale`).
pub async fn fail(
    db: &Db,
    request: MergeabilityRequest,
    request_id: &RequestId,
    cause: &ForgeError,
) {
    let MergeabilityRequest {
        merge_request: mr,
        check_seq: seq,
    } = request;
    match review::fail_check(db, mr, seq).await {
        Ok(true) => {
            let e = review::not_checked("job_failed");
            tracing::warn!(
                target: "forge_review",
                event = "forge.review.mergeability_not_checked",
                error_code = e.code().id(),
                request_id = %request_id,
                reason = "job_failed",
                merge_request_id = mr.get(),
                check_seq = seq,
                cause_code = cause.code().id(),
                "the mergeability job failed for good; stored check_failed"
            );
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(
            target: "forge_review",
            event = "forge.review.mergeability_fail_unrecorded",
            error_code = e.code().id(),
            request_id = %request_id,
            merge_request_id = mr.get(),
            check_seq = seq,
            error = %e,
            "could not store job_failed; the sweep ends the row as job_stale"
        ),
    }
}

#[cfg(test)]
#[path = "mergeability_tests.rs"]
mod tests;
