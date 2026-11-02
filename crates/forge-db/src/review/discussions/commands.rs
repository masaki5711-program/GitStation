//! The discussion commands, each decided inside its own transaction
//! (module docs of `review::discussions`). None of them authorizes:
//! forge-review passes `decide` = `Authorized::reconfirm` on the facts read
//! under the locks. Each returns the discussion as it stands after the
//! transaction, with its notes.

use forge_core::ids::{DiscussionId, ProjectId, UserId};
use forge_core::{
    AuditDetails, CommentAction, DedupKey, DedupPart, DiscussionKind, MergeRequestChange,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{
    attach, discussion_columns, note_columns, DiscussionPosition, DiscussionRow, NoteRow,
    StoredDiscussion, MAX_DISCUSSIONS, MAX_NOTES, MAX_NOTES_PER_DISCUSSION,
};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome, Settle,
};
use crate::review::commands::{absent, facts};
use crate::review::{MergeRequestFacts, StoredMergeRequest};
use crate::{query_error, Db};

/// The audit resource (tables.toml) and the events' aggregate.
const RESOURCE: &str = "project";
const AGGREGATE: &str = "merge_request";
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

/// `FG-REPO-0005 resource=discussion`: no such discussion on this merge
/// request (decided after the caller's permission, so it is no oracle).
fn discussion_absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "discussion")
        .with_context("reason", "absent")
}

/// The error of a discussion or note write: the version foreign key is
/// FG-REVIEW-0004 `version_unknown` (the backstop of forge-review's
/// check), a CHECK on a request value FG-WEB-0006 `param_invalid`,
/// anything else a query error.
fn write_error(query: &'static str) -> impl FnOnce(sqlx::Error) -> ForgeError {
    move |e| {
        let constraint = e
            .as_database_error()
            .and_then(|d| d.constraint())
            .map(str::to_owned);
        let refused = |param: &'static str, rule: &'static str, c: &str| {
            ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "param_invalid")
                .with_context("param", param)
                .with_context("rule", rule)
                .with_context("constraint", c.to_owned())
        };
        match constraint.as_deref() {
            Some("merge_request_discussions_version") => {
                ForgeError::new(ErrorCode::DiscussionPositionInvalid)
                    .with_context("reason", "version_unknown")
                    .with_context("backstop", "foreign_key")
            }
            Some(c @ "merge_request_notes_body_length") => refused("body", "length", c),
            Some(c @ "merge_request_discussions_path") => refused("path", "length", c),
            Some(c @ "merge_request_discussions_line_positive") => refused("line", "positive", c),
            Some(c @ "merge_request_discussions_version_positive") => {
                refused("version", "positive", c)
            }
            Some(_) | None => query_error(query, e),
        }
    }
}

/// The merge request as locked (`FG-REPO-0005` when absent).
fn locked_mr(facts: &MergeRequestFacts) -> Result<&StoredMergeRequest, ForgeError> {
    facts.merge_request.as_ref().ok_or_else(absent)
}

/// Discussion `id` of merge request `mr`, locked `FOR UPDATE`.
async fn lock_discussion(
    conn: &mut PgConnection,
    mr: i64,
    id: DiscussionId,
) -> Result<Option<StoredDiscussion>, ForgeError> {
    let row: Option<DiscussionRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_request_discussions \
         WHERE merge_request_id = $1 AND id = $2 FOR UPDATE",
        discussion_columns()
    ))
    .bind(mr)
    .bind(id.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("review.discussion_lock", e))?;
    row.map(DiscussionRow::parse).transpose()
}

/// The notes of discussion `id`, oldest first.
async fn notes_of(conn: &mut PgConnection, id: DiscussionId) -> Result<Vec<NoteRow>, ForgeError> {
    sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_request_notes WHERE discussion_id = $1 ORDER BY id LIMIT $2",
        note_columns()
    ))
    .bind(id.get())
    .bind(i64::try_from(MAX_NOTES_PER_DISCUSSION).unwrap_or(i64::MAX))
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.discussion_notes", e))
}

/// (discussions, notes) of merge request `mr` and the notes of
/// `discussion` (`0` without one), counted under the merge request's row
/// lock (every discussion write holds it).
async fn counts(
    conn: &mut PgConnection,
    mr: i64,
    discussion: Option<DiscussionId>,
) -> Result<(i64, i64, i64), ForgeError> {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM forge.merge_request_discussions WHERE merge_request_id = $1), \
                (SELECT count(*) FROM forge.merge_request_notes WHERE merge_request_id = $1), \
                (SELECT count(*) FROM forge.merge_request_notes WHERE discussion_id = $2)",
    )
    .bind(mr)
    .bind(discussion.map(DiscussionId::get))
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("review.discussion_counts", e))
}

/// `FG-REVIEW-0005` when one more discussion (`new_discussion`) or note
/// would pass a cap.
async fn refuse_full(
    tx: &mut ChangeTx,
    mr: &StoredMergeRequest,
    discussion: Option<DiscussionId>,
) -> Result<(), ForgeError> {
    let mr_id = mr.id.get();
    let (discussions, notes, in_discussion) = tx
        .step(|c| async move { counts(c, mr_id, discussion).await })
        .await?;
    let full = |reason: &'static str, limit: usize| {
        ForgeError::new(ErrorCode::DiscussionLimitReached)
            .with_context("reason", reason)
            .with_context("limit", limit)
            .with_context("project_id", mr.project_id.get())
            .with_context("iid", mr.iid)
            .with_context("merge_request_id", mr_id)
    };
    let at = |count: i64, limit: usize| usize::try_from(count).map_or(true, |n| n >= limit);
    if discussion.is_none() && at(discussions, MAX_DISCUSSIONS) {
        return Err(full("discussions_per_merge_request", MAX_DISCUSSIONS));
    }
    if at(notes, MAX_NOTES) {
        return Err(full("notes_per_merge_request", MAX_NOTES));
    }
    if let Some(d) = discussion {
        if at(in_discussion, MAX_NOTES_PER_DISCUSSION) {
            return Err(full("notes_per_discussion", MAX_NOTES_PER_DISCUSSION)
                .with_context("discussion_id", d.get()));
        }
    }
    Ok(())
}

/// Insert a note by `author` into `discussion`; its id.
async fn insert_note(
    conn: &mut PgConnection,
    discussion: &StoredDiscussion,
    project: ProjectId,
    author: UserId,
    body: &str,
) -> Result<i64, ForgeError> {
    sqlx::query_scalar(
        "INSERT INTO forge.merge_request_notes \
         (discussion_id, merge_request_id, project_id, author_id, body) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(discussion.id.get())
    .bind(discussion.merge_request.get())
    .bind(project.get())
    .bind(author.get())
    .bind(body)
    .fetch_one(conn)
    .await
    .map_err(write_error("review.note_insert"))
}

/// The one audit row and event of `change` (a `Commented`), deduplicated
/// by the note id (a note) or the discussion id and revision (a
/// resolution).
fn record(project: ProjectId, change: MergeRequestChange) -> Result<ChangeRecord, ForgeError> {
    let MergeRequestChange::Commented {
        merge_request,
        action,
        discussion,
        note,
        revision,
        ..
    } = &change
    else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "not_a_comment_change")
            .with_context("action", change.action()));
    };
    let parts = match (action, note) {
        (CommentAction::NoteAdded, Some(note)) => vec![DedupPart::Id(note.get())],
        (CommentAction::ThreadResolved | CommentAction::ThreadUnresolved, None) => {
            vec![DedupPart::Id(discussion.get()), DedupPart::Id(*revision)]
        }
        (CommentAction::NoteAdded, None)
        | (CommentAction::ThreadResolved | CommentAction::ThreadUnresolved, Some(_)) => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "comment_change_shape")
                .with_context("comment_action", action.as_str()))
        }
    };
    let dedup = DedupKey::new(action.dedup_prefix(), &parts)?;
    let aggregate = merge_request.get();
    let details = AuditDetails::MergeRequest(change);
    let event_type = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    Ok(ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: AGGREGATE,
            aggregate_id: aggregate,
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup),
        }],
    })
}

/// The discussion with its notes, read inside the transaction.
async fn whole(tx: &mut ChangeTx, mut d: StoredDiscussion) -> Result<StoredDiscussion, ForgeError> {
    let id = d.id;
    let notes = tx.step(|c| async move { notes_of(c, id).await }).await?;
    attach(std::slice::from_mut(&mut d), notes);
    Ok(d)
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
