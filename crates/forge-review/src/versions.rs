//! Merge request versions and their hidden refs (M3 R1b, ADR-0024 §1, §2;
//! plan 11 §11.3).
//!
//! [`refresh`] is the body of the version job `merge_request.refresh`
//! (`forge worker` runs it; the `merge-request-refresh` subscriber, an open
//! and a reopen enqueue it):
//!
//! 1. read the merge request and its repository (forge-db);
//! 2. read the source branch's tip with forge-git (`for-each-ref`, timeout);
//! 3. [`forge_db::review::record_mr_version`]: the version row FIRST, its
//!    number drawn under the merge request's lock, plus
//!    `merge_request.pushed` (dedup `merge_request_pushed:<id>:<head>`);
//! 4. [`VersionRefs::sync_locked`]: `refs/merge-requests/<iid>/v<n>` for
//!    every version that was not purged, and `…/head` at the merge
//!    request's head, written from the committed rows in ONE `update-ref`
//!    transaction.
//!
//! The whole run holds the merge request's refs lock
//! (`forge_db::review::lock_version_refs`, review: two runs could
//! otherwise interleave and move `head` back), bounded by
//! `REFS_LOCK_HOLD`: each git call gets what is left of it. A commit the
//! source branch pointed at only between two runs (pushed, then
//! force-pushed away before the job read the tip) gets no version.
//!
//! 5. M3 R2 (ADR-0024 §1, §3), for an open merge request: the head
//!    version's `base_sha` (the target's tip now) and `merge_base_sha` are
//!    recorded once ([`forge_db::review::record_version_bases`], audit
//!    only; a version recorded earlier keeps its bases), and a
//!    mergeability check is requested unless the stored status already
//!    answers this exact (head, target tip) pair.
//!
//! Every step is idempotent: a rerun finds the head already recorded
//! (nothing written, no second event) and the refs already in place (git
//! does not run). A ref that already holds the wanted value is success, not
//! a lost compare-and-swap. A `v<n>` holding another value is not claimed
//! by any row (the row for `n` is the one being written, and rows come
//! first), so the sync takes it back with a compare-and-swap on the value
//! it read and logs `forge.review.version_ref_reclaimed` once (info, not
//! FG-SYS-0001: a stopped earlier attempt can leave it). A purged version's
//! refs are deleted, never written (ADR-0024 closing table #42).
//!
//! Browse caches: the sync calls `Browser::invalidate` for the repository
//! when it holds one (`forge serve`: the purge). The worker holds no
//! browser — its process has no browse cache. The hidden refs are not in
//! the browse ref list; since M3 R2 a merge request's view reaches its own
//! version refs (`ReachRoots::merge_request`), so `forge serve` may answer
//! "not found" for a version the worker wrote within the last
//! `refs_ttl` (5 s) — never the reverse: a purge (in `forge serve`)
//! invalidates its own caches.

use std::time::Instant;

use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::review::{self, RefreshRequest, RefsLock, VersionOutcome};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use crate::version_refs::{
    plan_ref_changes, Deadline, RefPlan, SyncReport, VersionRefs, GIT_TIMEOUT,
};

/// The audit label of the version job's changes.
pub(crate) const ACTOR_LABEL: &str = "forge-worker";

/// What [`refresh`] found and did (the job's log line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    /// `added`, `restored`, `already_head`, `purged_head`, `not_open`,
    /// `source_missing` or `gone`.
    pub outcome: &'static str,
    pub version: Option<i64>,
    pub refs: Option<SyncReport>,
    /// The mergeability check this run requested (its `check_seq`).
    pub check_requested: Option<i64>,
    /// The conflict scan this run requested (its `scan_seq`; M3 X6).
    pub scan_requested: Option<i64>,
}

fn outcome_name(outcome: &VersionOutcome) -> (&'static str, Option<i64>) {
    match outcome {
        VersionOutcome::Added { version } => ("added", Some(*version)),
        VersionOutcome::Restored { version } => ("restored", Some(*version)),
        VersionOutcome::AlreadyHead { version } => ("already_head", *version),
        VersionOutcome::PurgedHead { version } => ("purged_head", Some(*version)),
        VersionOutcome::NotOpen(_) => ("not_open", None),
        VersionOutcome::Gone => ("gone", None),
    }
}

/// The version job (module docs). The whole run holds the merge request's
/// refs lock, so two runs never interleave (tip read, version row and
/// sync are one step for every other run). Errors: forge-git's
/// (`FG-GIT-0002` / `0003` / `0004`, `FG-REPO-0003` storage), forge-db's
/// (`FG-DB-0001` / `0002`, a lock held longer than `REFS_LOCK_WAIT`),
/// `FG-REPO-0016` for a repository not ready yet; each is the job's
/// failure (the worker logs it once with the job's request id).
pub async fn refresh(
    db: &Db,
    refs: &VersionRefs,
    request: RefreshRequest,
    request_id: &RequestId,
) -> Result<RefreshReport, ForgeError> {
    let mut lock = review::lock_version_refs(db, request.merge_request).await?;
    let report = refresh_locked(db, refs, &mut lock, request, request_id).await;
    lock.release().await;
    report
}

async fn refresh_locked(
    db: &Db,
    refs: &VersionRefs,
    lock: &mut RefsLock,
    request: RefreshRequest,
    request_id: &RequestId,
) -> Result<RefreshReport, ForgeError> {
    let started = Instant::now();
    let mr = request.merge_request;
    let Some(target) = review::refresh_target(db, mr).await? else {
        return Ok(RefreshReport {
            outcome: "gone",
            version: None,
            refs: None,
            check_requested: None,
            scan_requested: None,
        });
    };
    if !target.repository_ready {
        return Err(ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("repo_id", target.repository.get())
            .with_context("merge_request_id", mr.get()));
    }
    let repo = target.repository;
    let stored = &target.merge_request;
    let (outcome, version) = if stored.state == forge_core::MergeRequestState::Open {
        match refs
            .branch_tip(repo, &stored.source_branch, lock.deadline())
            .await?
        {
            None => ("source_missing", None),
            Some(tip) => {
                let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
                ctx.actor_label = Some(ACTOR_LABEL.to_owned());
                // M3 R6 (ADR-0024 §8.2): may the new head carry the approvals?
                let carry = crate::versions_carry::carry_plan(
                    db,
                    refs,
                    lock.deadline(),
                    repo,
                    stored,
                    &tip,
                    request_id,
                )
                .await?;
                let done = review::record_mr_version_carrying(
                    db,
                    &ctx,
                    mr,
                    &tip,
                    request.pushed_by,
                    carry.as_ref(),
                )
                .await?;
                if let VersionOutcome::PurgedHead { version } = done {
                    tracing::warn!(
                        target: "forge_review",
                        event = "forge.review.purged_head_pushed",
                        request_id = %request_id,
                        merge_request_id = mr.get(),
                        project_id = stored.project_id.get(),
                        iid = stored.iid,
                        version,
                        "the source branch was pushed back to a purged version; it stays out (runbook secret-removal)"
                    );
                }
                outcome_name(&done)
            }
        }
    } else {
        ("not_open", None)
    };
    // Also for a closed one: refs of versions recorded by an earlier
    // attempt that stopped before its sync are written now.
    let synced = refs.sync_locked(lock, repo, request_id).await?;
    let check_requested = if stored.state == forge_core::MergeRequestState::Open {
        crate::mergeability::after_version(db, refs, lock, mr, repo, request_id).await?
    } else {
        None
    };
    // M3 X6: the conflict scan of the (new) head, drafts included; never
    // fails the version job (review HIGH: the sweep covers a failed request).
    let scan = if stored.state == forge_core::MergeRequestState::Open {
        crate::conflicts::after_version(db, mr, request_id).await
    } else {
        crate::conflicts::ScanAsked {
            outcome: "not_open",
            seq: None,
        }
    };
    let scan_requested = scan.seq;
    tracing::info!(
        target: "forge_review",
        event = "forge.review.version_refreshed",
        request_id = %request_id,
        merge_request_id = mr.get(),
        project_id = stored.project_id.get(),
        iid = stored.iid,
        outcome,
        version,
        ref_changes = synced.map_or(0, |s| s.changes),
        check_requested,
        scan_requested,
        scan = scan.outcome,
        duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "merge request versions refreshed"
    );
    Ok(RefreshReport {
        outcome,
        version,
        refs: synced,
        check_requested,
        scan_requested,
    })
}

#[cfg(test)]
#[path = "versions_tests.rs"]
mod tests;
