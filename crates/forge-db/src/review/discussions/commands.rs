//! The discussion commands, each decided inside its own transaction
//! (module docs of `review::discussions`). None of them authorizes:
//! forge-review passes `decide` = `Authorized::reconfirm` on the facts read
//! under the locks. Each returns the discussion as it stands after the
//! transaction, with its notes.

use forge_core::ids::{DiscussionId, ProjectId, UserId};
use forge_core::{CommentAction, DiscussionKind, MergeRequestChange};
use forge_errors::ForgeError;

use super::steps::{
    discussion_absent, insert_note, lock_discussion, locked_mr, record, refuse_full, whole,
    write_error,
};
use super::{discussion_columns, DiscussionPosition, DiscussionRow, StoredDiscussion};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::review::commands::facts;
use crate::review::MergeRequestFacts;
use crate::Db;

/// The audit action of every discussion write (ADR-0024 §6).
const ACTION: &str = "merge_request.commented";

/// A discussion to start: its first note and, for an inline one, where.
#[derive(Debug, Clone)]
pub struct NewDiscussion {
    pub body: String,
    pub position: Option<DiscussionPosition>,
}

/// What `decide` sees for a resolution: the merge request facts and the
/// discussion as locked (`None`: the merge request has no such one).
#[derive(Debug, Clone)]
pub struct DiscussionFacts {
    pub merge_request: MergeRequestFacts,
    pub discussion: Option<StoredDiscussion>,
}

/// What a resolve / unresolve did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    Changed,
    /// Already in that state: nothing written.
    Unchanged,
}

/// Start a discussion on merge request `iid` of `project` by `viewer`
/// with its first note (`merge_request.commented`, `note_added`,
/// `discussion_started: true`), decided inside the transaction. After
/// `decide`: `FG-REPO-0005 resource=merge_request` for an absent one;
/// FG-REVIEW-0004 `version_unknown` (backstop) for a position on a
/// version the merge request does not have; FG-WEB-0006 for a value a
/// CHECK refuses.
pub async fn start_discussion_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    viewer: UserId,
    new: &NewDiscussion,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StoredDiscussion, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let mr = locked_mr(&facts)?;
        let (mr_id, mr_iid) = (mr.id, mr.iid);
        refuse_full(&mut tx, mr, None).await?;
        let kind = match new.position {
            None => DiscussionKind::General,
            Some(_) => DiscussionKind::Inline,
        };
        let position = new.position.clone(); // clone: moved into the step
        let row: DiscussionRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "INSERT INTO forge.merge_request_discussions \
                     (merge_request_id, project_id, kind, version_no, path, side, line, \
                      started_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {}",
                    discussion_columns()
                ))
                .bind(mr_id.get())
                .bind(project.get())
                .bind(kind.as_str())
                .bind(position.as_ref().map(|p| p.version))
                .bind(position.as_ref().map(|p| p.path.as_str()))
                .bind(position.as_ref().map(|p| p.side.as_str()))
                .bind(position.as_ref().map(|p| p.line))
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(write_error("review.discussion_insert"))
            })
            .await?;
        let discussion = row.parse()?;
        let body = new.body.clone(); // clone: moved into the step
        let started = discussion.clone(); // clone: moved into the step
        let note = tx
            .step(|c| async move { insert_note(c, &started, project, viewer, &body).await })
            .await?;
        let change = MergeRequestChange::Commented {
            merge_request: mr_id,
            iid: mr_iid,
            action: CommentAction::NoteAdded,
            discussion: discussion.id,
            kind,
            note: Some(forge_core::NoteId::new(note)),
            discussion_started: true,
            revision: discussion.revision,
        };
        let record = record(project, change)?;
        let discussion = whole(&mut tx, discussion).await?;
        Ok(Settle::Commit(record, (discussion, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Add a note by `viewer` to discussion `discussion` of merge request
/// `iid` (`note_added`, `discussion_started: false`), decided inside the
/// transaction on the merge request (commenting is decided on it). After
/// `decide`: `FG-REPO-0005 resource=merge_request` / `resource=discussion`
/// for an absent one. A reply does not change whether the thread is
/// resolved.
#[allow(clippy::too_many_arguments)] // the ids of the path, the caller, the body and the decision
pub async fn add_note_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    discussion: DiscussionId,
    viewer: UserId,
    body: &str,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(StoredDiscussion, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let decided = decide(&facts)?;
        let mr = locked_mr(&facts)?;
        let (mr_id, mr_iid) = (mr.id.get(), mr.iid);
        let current = tx
            .step(|c| async move { lock_discussion(c, mr_id, discussion).await })
            .await?
            .ok_or_else(discussion_absent)?;
        refuse_full(&mut tx, mr, Some(current.id)).await?;
        let body = body.to_owned();
        let target = current.clone(); // clone: moved into the step
        let note = tx
            .step(|c| async move { insert_note(c, &target, project, viewer, &body).await })
            .await?;
        let change = MergeRequestChange::Commented {
            merge_request: current.merge_request,
            iid: mr_iid,
            action: CommentAction::NoteAdded,
            discussion: current.id,
            kind: current.kind,
            note: Some(forge_core::NoteId::new(note)),
            discussion_started: false,
            revision: current.revision,
        };
        let record = record(project, change)?;
        let discussion = whole(&mut tx, current).await?;
        Ok(Settle::Commit(record, (discussion, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Resolve (`resolved: true`) or unresolve discussion `discussion` of
/// merge request `iid` by `viewer`, decided inside the transaction on the
/// discussion as locked (`decide` gets it: who started it). After
/// `decide`: `FG-REPO-0005` for an absent merge request or discussion;
/// [`ResolveOutcome::Unchanged`] when it already is in that state (nothing
/// written). A change adds one to the revision (`thread_resolved` /
/// `thread_unresolved`, deduplicated by id and revision).
#[allow(clippy::too_many_arguments)] // the ids of the path, the caller, the state and the decision
pub async fn resolve_discussion_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    iid: i64,
    discussion: DiscussionId,
    viewer: UserId,
    resolved: bool,
    decide: impl FnOnce(&DiscussionFacts) -> Result<P, ForgeError>,
) -> Result<(ResolveOutcome, StoredDiscussion, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, Some(iid), viewer).await?;
        let locked = match &facts.merge_request {
            Some(mr) => {
                let mr_id = mr.id.get();
                tx.step(|c| async move { lock_discussion(c, mr_id, discussion).await })
                    .await?
            }
            None => None,
        };
        let facts = DiscussionFacts {
            merge_request: facts,
            discussion: locked,
        };
        let decided = decide(&facts)?;
        let mr_iid = locked_mr(&facts.merge_request)?.iid;
        let current = facts.discussion.ok_or_else(discussion_absent)?;
        if current.resolved == resolved {
            let current = whole(&mut tx, current).await?;
            return Ok(Settle::Discard((
                ResolveOutcome::Unchanged,
                current,
                decided,
            )));
        }
        let id = current.id;
        let row: DiscussionRow = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "UPDATE forge.merge_request_discussions SET resolved = $2, \
                     resolved_by = CASE WHEN $2 THEN $3 END, \
                     resolved_at = CASE WHEN $2 THEN now() END, revision = revision + 1 \
                     WHERE id = $1 RETURNING {}",
                    discussion_columns()
                ))
                .bind(id.get())
                .bind(resolved)
                .bind(viewer.get())
                .fetch_one(c)
                .await
                .map_err(write_error("review.discussion_resolve"))
            })
            .await?;
        let stored = row.parse()?;
        let change = MergeRequestChange::Commented {
            merge_request: stored.merge_request,
            iid: mr_iid,
            action: if resolved {
                CommentAction::ThreadResolved
            } else {
                CommentAction::ThreadUnresolved
            },
            discussion: stored.id,
            kind: stored.kind,
            note: None,
            discussion_started: false,
            revision: stored.revision,
        };
        let record = record(project, change)?;
        let stored = whole(&mut tx, stored).await?;
        Ok(Settle::Commit(
            record,
            (ResolveOutcome::Changed, stored, decided),
        ))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
