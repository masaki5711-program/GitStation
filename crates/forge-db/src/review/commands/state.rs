//! `close_mr_decided` and `reopen_mr_decided`: open ⇄ closed.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{MergeRequestChange, MergeRequestState};
use forge_errors::ForgeError;

use super::{
    absent, facts, record, refuse_open_duplicate, state_invariant, write_error, MergeRequestFacts,
    StateOutcome,
};
use crate::change::{ChangeContext, ChangeTx, Settle};
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
/// is a broken invariant (`FG-SYS-0001 reopen_merged`); `FG-REVIEW-0001`
/// when another open one has its branches. The row as it stands after the
/// transaction either way.
pub async fn reopen_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
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
        Ok(Settle::Commit(
            record,
            (StateOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
