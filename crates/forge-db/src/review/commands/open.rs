//! `open_mr_decided`: a new merge request and its per-project number.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{MergeRequestChange, RefName};
use forge_errors::ForgeError;

use super::{facts, record, refuse_open_duplicate, write_error, MergeRequestFacts};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::review::{Row, StoredMergeRequest, COLUMNS};
use crate::{query_error, Db};

/// A merge request to open; the fields are checked by forge-review (the
/// table's CHECKs are the backstop, FG-WEB-0006), the branches are
/// `refs/heads/*` and differ (authorize()).
#[derive(Debug, Clone)]
pub struct NewMergeRequest {
    pub title: String,
    pub description: String,
    pub source_branch: RefName,
    pub target_branch: RefName,
    pub draft: bool,
}

/// Open a merge request in `project` for `viewer`, deciding inside the
/// transaction (module docs of `review`): `decide` sees `merge_request:
/// None`. After it: `FG-REVIEW-0001` when an open one has the same branches
/// (under the project lock; `unique_index` as the backstop), then the next
/// `iid` of the project (its counter row, never `max(iid) + 1`), the row,
/// one `merge_request.opened` audit row and event. Errors also:
/// `FG-REPO-0005 resource=project`, `FG-WEB-0006` (a CHECK), and whatever
/// `decide` returns.
pub async fn open_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    new: &NewMergeRequest,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StoredMergeRequest, P), ForgeError> {
    const ACTION: &str = "merge_request.opened";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, None, viewer).await?;
        let decided = decide(&facts)?;
        refuse_open_duplicate(
            &mut tx,
            project,
            viewer,
            (&new.source_branch, &new.target_branch),
            None,
            ACTION,
        )
        .await?;
        let iid: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                     VALUES ($1, 1) ON CONFLICT (project_id) DO UPDATE \
                     SET last_iid = forge.merge_request_counters.last_iid + 1 RETURNING last_iid",
                )
                .bind(project.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("review.next_iid", e))
            })
            .await?;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_requests (project_id, iid, title, description, \
                     author_id, source_branch, target_branch, draft) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {COLUMNS}"
                ))
                .bind(project.get())
                .bind(iid)
                .bind(&new.title)
                .bind(&new.description)
                .bind(viewer.get())
                .bind(new.source_branch.as_str())
                .bind(new.target_branch.as_str())
                .bind(new.draft)
                .fetch_one(c)
                .await
                .map_err(write_error("review.open", project, viewer, ACTION))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Opened {
            merge_request: stored.id,
            iid: stored.iid,
            author: viewer,
            source_branch: stored.source_branch.clone(), // clone: the row is returned too
            target_branch: stored.target_branch.clone(),
            draft: stored.draft,
        };
        let record = record(project, vec![change], stored.revision)?;
        Ok(Settle::Commit(record, (stored, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
