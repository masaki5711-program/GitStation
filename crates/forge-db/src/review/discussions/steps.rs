//! The steps the discussion commands share (`commands.rs`): the locks
//! and reads under them, the caps (FG-REVIEW-0005), the note insert, the
//! one audit row and event of a change, and the error of a write.

use forge_core::ids::{DiscussionId, ProjectId, UserId};
use forge_core::{AuditDetails, CommentAction, DedupKey, DedupPart, MergeRequestChange};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{
    attach, discussion_columns, note_columns, DiscussionRow, NoteRow, StoredDiscussion,
    MAX_DISCUSSIONS, MAX_NOTES, MAX_NOTES_PER_DISCUSSION,
};
use crate::change::{AuditEntry, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::query_error;
use crate::review::commands::absent;
use crate::review::{MergeRequestFacts, StoredMergeRequest};

/// The audit resource (tables.toml) and the events' aggregate.
const RESOURCE: &str = "project";
const AGGREGATE: &str = "merge_request";

/// `FG-REPO-0005 resource=discussion`: no such discussion on this merge
/// request (decided after the caller's permission, so it is no oracle).
pub(super) fn discussion_absent() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "discussion")
        .with_context("reason", "absent")
}

/// The error of a discussion or note write: the version foreign key is
/// FG-REVIEW-0004 `version_unknown` (the backstop of forge-review's
/// check), a CHECK on a request value FG-WEB-0006 `param_invalid`,
/// anything else a query error.
pub(super) fn write_error(query: &'static str) -> impl FnOnce(sqlx::Error) -> ForgeError {
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
pub(super) fn locked_mr(facts: &MergeRequestFacts) -> Result<&StoredMergeRequest, ForgeError> {
    facts.merge_request.as_ref().ok_or_else(absent)
}

/// Discussion `id` of merge request `mr`, locked `FOR UPDATE`.
pub(super) async fn lock_discussion(
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
pub(super) async fn notes_of(
    conn: &mut PgConnection,
    id: DiscussionId,
) -> Result<Vec<NoteRow>, ForgeError> {
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
pub(super) async fn counts(
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
pub(super) async fn refuse_full(
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
pub(super) async fn insert_note(
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
pub(super) fn record(
    project: ProjectId,
    change: MergeRequestChange,
) -> Result<ChangeRecord, ForgeError> {
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
pub(super) async fn whole(
    tx: &mut ChangeTx,
    mut d: StoredDiscussion,
) -> Result<StoredDiscussion, ForgeError> {
    let id = d.id;
    let notes = tx.step(|c| async move { notes_of(c, id).await }).await?;
    attach(std::slice::from_mut(&mut d), notes);
    Ok(d)
}
