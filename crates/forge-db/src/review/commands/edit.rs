//! `edit_mr_decided`: the title, description, draft flag and target.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{MergeRequestChange, MergeRequestField, MergeRequestState, RefName};
use forge_errors::ForgeError;

use super::{
    absent, facts, record, refuse_open_duplicate, state_invariant, write_error, EditOutcome,
    MergeRequestFacts,
};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::review::mergeability::request_check_on;
use crate::review::{Row, StoredMergeRequest, COLUMNS};
use crate::Db;

/// What an edit asks for (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct MergeRequestPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub draft: Option<bool>,
    pub target_branch: Option<RefName>,
}

/// The transitions an edit of `current` by `patch` makes.
fn edits(current: &StoredMergeRequest, patch: &MergeRequestPatch) -> Vec<MergeRequestChange> {
    let mut fields = Vec::new();
    if patch.title.as_ref().is_some_and(|t| *t != current.title) {
        fields.push(MergeRequestField::Title);
    }
    if patch
        .description
        .as_ref()
        .is_some_and(|d| *d != current.description)
    {
        fields.push(MergeRequestField::Description);
    }
    let retarget = patch
        .target_branch
        .as_ref()
        .filter(|t| **t != current.target_branch);
    if retarget.is_some() {
        fields.push(MergeRequestField::TargetBranch);
    }
    let mut changes = Vec::new();
    if !fields.is_empty() {
        changes.push(MergeRequestChange::Updated {
            merge_request: current.id,
            iid: current.iid,
            fields,
            // clone: the details own the names
            target_branch: retarget.map(|to| (current.target_branch.clone(), to.clone())),
        });
    }
    if let Some(draft) = patch.draft.filter(|d| *d != current.draft) {
        changes.push(MergeRequestChange::DraftChanged {
            merge_request: current.id,
            iid: current.iid,
            draft,
        });
    }
    changes
}

/// Edit merge request `iid` of `project`: the title, description, draft
/// flag and target asked in `patch`, decided inside the transaction on the
/// row as locked. After `decide`: `FG-REPO-0005 resource=merge_request`
/// for an absent one, `FG-REVIEW-0001` when a new target collides with an
/// open one, `FG-WEB-0006` when a CHECK refuses a value (a target equal to
/// the source). One audit row and event per transition (`updated`,
/// `draft_changed`), or [`EditOutcome::Unchanged`] (nothing written); the
/// row as it stands after the transaction either way. A retarget also
/// requests a mergeability check (M3 R2: `unchecked`, the job enqueued in
/// this transaction).
pub async fn edit_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    patch: &MergeRequestPatch,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(EditOutcome, StoredMergeRequest, P), ForgeError> {
    const ACTION: &str = "merge_request.updated";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let current = facts.merge_request.ok_or_else(absent)?;
        let changes = edits(&current, patch);
        if changes.is_empty() {
            return Ok(Settle::Discard((EditOutcome::Unchanged, current, decided)));
        }
        let retargets = changes.iter().any(|c| {
            matches!(
                c,
                MergeRequestChange::Updated {
                    target_branch: Some(_),
                    ..
                }
            )
        });
        if (retargets || patch.draft.is_some_and(|d| d != current.draft))
            && current.state != MergeRequestState::Open
        {
            return Err(state_invariant("edit_not_open", &current));
        }
        let target = patch
            .target_branch
            .clone() // clone: bound into the statement below
            .unwrap_or_else(|| current.target_branch.clone());
        if retargets {
            refuse_open_duplicate(
                &mut tx,
                project,
                viewer,
                (&current.source_branch, &target),
                Some(current.id),
                ACTION,
            )
            .await?;
        }
        // clone: the patch is borrowed, the statement binds owned values
        let title = patch.title.clone().unwrap_or_else(|| current.title.clone());
        // clone: as the title
        let description = patch
            .description
            .clone()
            .unwrap_or_else(|| current.description.clone());
        let draft = patch.draft.unwrap_or(current.draft);
        let id = current.id;
        let row: Row = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_requests SET title = $2, description = $3, draft = $4, \
                     target_branch = $5, revision = revision + 1, updated_at = now() \
                     WHERE id = $1 RETURNING {COLUMNS}"
                ))
                .bind(id.get())
                .bind(&title)
                .bind(&description)
                .bind(draft)
                .bind(target.as_str())
                .fetch_one(c)
                .await
                .map_err(write_error("review.edit", project, viewer, ACTION))
            })
            .await?;
        let stored = row.parse()?;
        let types = changes
            .iter()
            .map(|c| c.transition().event_type())
            .collect();
        let record = record(project, changes, stored.revision)?;
        if retargets {
            // M3 R2 (ADR-0024 §3): the old status is about another target;
            // `unchecked` now, checked again by the job.
            let origin = ctx.request_id.clone(); // clone: the job keeps its origin request
            tx.step(|c| async move { request_check_on(c, id, project, origin, None).await })
                .await?;
        }
        Ok(Settle::Commit(
            record,
            (EditOutcome::Changed(types), stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
