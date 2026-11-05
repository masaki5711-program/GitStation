//! `close_mr_decided` and `reopen_mr_decided`: open ⇄ closed.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{MergeRequestChange, MergeRequestState, ObjectId};
use forge_errors::ForgeError;

use super::{
    absent, facts, record, refuse_open_duplicate, state_invariant, write_error, MergeRequestFacts,
    StateOutcome,
};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::review::conflicts::request_scan_on;
use crate::review::version_refs::precondition;
use crate::review::versions::{refresh_job, versions_on, RefreshRequest, RefreshTrigger};
use crate::review::{Row, StoredMergeRequest, COLUMNS};
use crate::Db;

/// Close merge request `iid` of `project` (`closed_at`, `closed_by` =
/// `viewer`), decided inside the transaction. After `decide`:
/// `FG-REPO-0005 resource=merge_request` for an absent one;
/// [`StateOutcome::Unchanged`] for a closed one; a merged one is a broken
/// invariant (`authorize()` denies it, `FG-SYS-0001 close_merged`). The
/// row as it stands after the transaction either way.
pub async fn close_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StateOutcome, StoredMergeRequest, P), ForgeError> {
    const ACTION: &str = "merge_request.closed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        match current.state {
            MergeRequestState::Closed => {
                return Ok(Settle::Discard((StateOutcome::Unchanged, current, decided)))
            }
            MergeRequestState::Merged => return Err(state_invariant("close_merged", &current)),
            MergeRequestState::Open => {}
        }
        // M3 R4 (ADR-0024 §4.2): a close waits for a merge in progress.
        crate::review::merges::refuse_live_merge(&mut tx, current.id, ACTION).await?;
        let id = current.id;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_requests SET state = 'closed', closed_at = now(), \
                     closed_by = $2, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ))
                .bind(id.get())
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(write_error("review.close", project, viewer, ACTION))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Closed {
            merge_request: id,
            iid: stored.iid,
        };
        let record = record(project, vec![change], stored.revision)?;
        Ok(Settle::Commit(
            record,
            (StateOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Reopen merge request `iid` of `project`, decided inside the
/// transaction. After `decide`: `FG-REPO-0005 resource=merge_request` for
/// an absent one; [`StateOutcome::Unchanged`] for an open one; a merged one
/// is a broken invariant (`FG-SYS-0001 reopen_merged`); `FG-REVIEW-0002
/// reason=reopen_at_purged_head` when `source_head` (the source branch's
/// tip as forge-review read it) is a version whose refs were purged
/// (ADR-0024 closing table #43); `FG-REVIEW-0001` when another open one has
/// its branches. A reopen enqueues the version job (M3 R1b). The row as it
/// stands after the transaction either way.
pub async fn reopen_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    source_head: Option<&ObjectId>,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StateOutcome, StoredMergeRequest, P), ForgeError> {
    const ACTION: &str = "merge_request.reopened";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        match current.state {
            MergeRequestState::Open => {
                return Ok(Settle::Discard((StateOutcome::Unchanged, current, decided)))
            }
            MergeRequestState::Merged => return Err(state_invariant("reopen_merged", &current)),
            MergeRequestState::Closed => {}
        }
        if let Some(head) = source_head {
            let mr = current.id;
            let known = tx
                .step(|c| async move { versions_on(c, mr, "", "review.reopen_versions").await })
                .await?;
            if let Some(v) = known.iter().find(|v| v.refs_purged && v.head_sha == *head) {
                return Err(precondition(
                    "reopen_at_purged_head",
                    &current,
                    v.version_no,
                ));
            }
        }
        refuse_open_duplicate(
            &mut tx,
            project,
            viewer,
            (&current.source_branch, &current.target_branch),
            Some(current.id),
            ACTION,
        )
        .await?;
        let id = current.id;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_requests SET state = 'open', closed_at = NULL, \
                     closed_by = NULL, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ))
                .bind(id.get())
                .fetch_one(c)
                .await
                .map_err(write_error("review.reopen", project, viewer, ACTION))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Reopened {
            merge_request: id,
            iid: stored.iid,
        };
        let record = record(project, vec![change], stored.revision)?;
        // M3 R1b: the source may have moved while it was closed.
        let job = refresh_job(
            RefreshRequest {
                merge_request: id,
                pushed_by: None,
            },
            RefreshTrigger::Reopened {
                revision: stored.revision,
            },
            ctx.request_id.clone(), // clone: the job keeps its origin request
            None,
        )?;
        tx.enqueue_job(&job).await?;
        // M3 X6: other merge requests opened or moved while it was closed;
        // the scan of the pairs it had is no longer current.
        let origin = ctx.request_id.clone(); // clone: the job keeps its origin request
        tx.step(|c| async move { request_scan_on(c, id, project, origin, None).await })
            .await?;
        Ok(Settle::Commit(
            record,
            (StateOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
