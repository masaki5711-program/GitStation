//! Merge request discussions (M3 R3, ADR-0024 §6, §12; plan 11 §4.1):
//! the threads of a merge request and their notes. The reads the route
//! loaders and the list use ([`discussions`], [`discussion`]), and the
//! commands decided inside their transaction (`commands.rs`):
//! [`start_discussion_mr_decided`], [`add_note_mr_decided`],
//! [`resolve_discussion_mr_decided`]. The names carry `_mr_` so they never
//! match the needles of forge-hosting's `decided_callers.rs` (ADR-0024
//! §11, review #33).
//!
//! Every command locks the project row, the merge request row (`FOR
//! UPDATE`, through `commands::facts`), then — for a reply or a resolution
//! — the discussion row, then the caller's roles and account, and hands
//! those facts to `decide` before anything is written. Every write commits
//! one `merge_request.commented` audit row on the project and one event
//! (ADR-0024 §6), deduplicated by the note's id or the discussion's id and
//! revision ([`forge_core::CommentAction::dedup_prefix`]).
//!
//! An inline position is checked against the version's diff by
//! forge-review before the transaction (FG-REVIEW-0004); here the
//! composite foreign key to the versions is the backstop (an unknown
//! version is the same FG-REVIEW-0004 `version_unknown`).

use forge_core::ids::{DiscussionId, MergeRequestId, NoteId, UserId};
use forge_core::{DiscussionKind, DiscussionSide};
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

mod commands;

pub use commands::{
    add_note_mr_decided, resolve_discussion_mr_decided, start_discussion_mr_decided,
    DiscussionFacts, NewDiscussion, ResolveOutcome,
};

/// The most discussions a merge request holds (FG-REVIEW-0005
/// `discussions_per_merge_request`, checked under its row lock) and one
/// list returns (`truncated` beyond: never, while the cap holds).
pub const MAX_DISCUSSIONS: usize = 500;
/// The most notes a merge request holds over all its discussions
/// (`notes_per_merge_request`) and one list returns.
pub const MAX_NOTES: usize = 5_000;
/// The most notes one discussion holds (`notes_per_discussion`).
pub const MAX_NOTES_PER_DISCUSSION: usize = 200;

/// Where an inline discussion sits: a line of one side of one version's
/// diff (`merge_base..head`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionPosition {
    pub version: i64,
    pub path: String,
    pub side: DiscussionSide,
    pub line: i64,
}

/// One note as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNote {
    pub id: NoteId,
    pub author: UserId,
    /// Plain text / Markdown as written (never rendered by the server).
    pub body: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// One discussion as stored, with its notes oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDiscussion {
    pub id: DiscussionId,
    pub merge_request: MergeRequestId,
    pub kind: DiscussionKind,
    /// `Some` exactly for an inline discussion.
    pub position: Option<DiscussionPosition>,
    pub started_by: UserId,
    pub resolved: bool,
    pub resolved_by: Option<UserId>,
    pub resolved_at: Option<String>,
    /// 1 at creation, +1 per resolve / unresolve.
    pub revision: i64,
    pub created_at: String,
    pub notes: Vec<StoredNote>,
}

/// The discussions of a merge request (oldest first) and whether a cap
/// cut the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionPage {
    pub discussions: Vec<StoredDiscussion>,
    /// More than [`MAX_DISCUSSIONS`] discussions, or more than
    /// [`MAX_NOTES`] notes: the rest are not listed.
    pub truncated: bool,
}

const UTC: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// The columns of a discussion row, in the order of [`DiscussionRow`].
pub(crate) fn discussion_columns() -> String {
    format!(
        "id, merge_request_id, kind, version_no, path, side, line, started_by, resolved, \
         resolved_by, to_char(resolved_at AT TIME ZONE 'UTC', {UTC}) AS resolved_at, revision, \
         to_char(created_at AT TIME ZONE 'UTC', {UTC}) AS created_at"
    )
}

/// The columns of a note row, in the order of [`NoteRow`].
pub(crate) fn note_columns() -> String {
    format!(
        "id, discussion_id, author_id, body, \
         to_char(created_at AT TIME ZONE 'UTC', {UTC}) AS created_at"
    )
}

#[derive(sqlx::FromRow)]
pub(crate) struct DiscussionRow {
    id: i64,
    merge_request_id: i64,
    kind: String,
    version_no: Option<i64>,
    path: Option<String>,
    side: Option<String>,
    line: Option<i64>,
    started_by: i64,
    resolved: bool,
    resolved_by: Option<i64>,
    resolved_at: Option<String>,
    revision: i64,
    created_at: String,
}

#[derive(sqlx::FromRow)]
pub(crate) struct NoteRow {
    id: i64,
    discussion_id: i64,
    author_id: i64,
    body: String,
    created_at: String,
}

/// `FG-SYS-0001` for a stored value the table's CHECKs make unreachable.
fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("discussion_id", id)
}

impl DiscussionRow {
    /// The discussion without its notes (`notes` empty).
    pub(crate) fn parse(self) -> Result<StoredDiscussion, ForgeError> {
        let id = self.id;
        let kind =
            DiscussionKind::parse(&self.kind).ok_or_else(|| stored_invalid("stored_kind", id))?;
        let position = match (kind, self.version_no, self.path, self.side, self.line) {
            (DiscussionKind::General, None, None, None, None) => None,
            (DiscussionKind::Inline, Some(version), Some(path), Some(side), Some(line)) => {
                Some(DiscussionPosition {
                    version,
                    path,
                    side: DiscussionSide::parse(&side)
                        .ok_or_else(|| stored_invalid("stored_side", id))?,
                    line,
                })
            }
            _ => return Err(stored_invalid("stored_position", id)),
        };
        Ok(StoredDiscussion {
            id: DiscussionId::new(id),
            merge_request: MergeRequestId::new(self.merge_request_id),
            kind,
            position,
            started_by: UserId::new(self.started_by),
            resolved: self.resolved,
            resolved_by: self.resolved_by.map(UserId::new),
            resolved_at: self.resolved_at,
            revision: self.revision,
            created_at: self.created_at,
            notes: Vec::new(),
        })
    }
}

impl NoteRow {
    pub(crate) fn discussion(&self) -> i64 {
        self.discussion_id
    }

    pub(crate) fn parse(self) -> StoredNote {
        StoredNote {
            id: NoteId::new(self.id),
            author: UserId::new(self.author_id),
            body: self.body,
            created_at: self.created_at,
        }
    }
}

/// Attach `notes` (ordered by discussion, then id) to `discussions`.
pub(crate) fn attach(discussions: &mut [StoredDiscussion], notes: Vec<NoteRow>) {
    let at: std::collections::HashMap<i64, usize> = discussions
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id.get(), i))
        .collect();
    for note in notes {
        // A note of a discussion past the list's cap has no owner here.
        if let Some(d) = at
            .get(&note.discussion())
            .and_then(|&i| discussions.get_mut(i))
        {
            d.notes.push(note.parse());
        }
    }
}

/// The discussions of merge request `mr` with their notes, oldest first,
/// at most [`MAX_DISCUSSIONS`] and [`MAX_NOTES`] (`truncated` beyond). Two
/// bounded statements on the read pool.
pub async fn discussions(db: &Db, mr: MergeRequestId) -> Result<DiscussionPage, ForgeError> {
    const QUERY: &str = "review.discussions";
    let max_discussions = i64::try_from(MAX_DISCUSSIONS + 1).unwrap_or(i64::MAX);
    let max_notes = i64::try_from(MAX_NOTES + 1).unwrap_or(i64::MAX);
    let (rows, notes): (Vec<DiscussionRow>, Vec<NoteRow>) = db
        .within(QUERY, async {
            let rows: Vec<DiscussionRow> = sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_discussions WHERE merge_request_id = $1 \
                 ORDER BY id LIMIT $2",
                discussion_columns()
            ))
            .bind(mr.get())
            .bind(max_discussions)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            let notes: Vec<NoteRow> = sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_notes WHERE merge_request_id = $1 \
                 ORDER BY discussion_id, id LIMIT $2",
                note_columns()
            ))
            .bind(mr.get())
            .bind(max_notes)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok((rows, notes))
        })
        .await?;
    let truncated = rows.len() > MAX_DISCUSSIONS || notes.len() > MAX_NOTES;
    let mut discussions = rows
        .into_iter()
        .take(MAX_DISCUSSIONS)
        .map(DiscussionRow::parse)
        .collect::<Result<Vec<_>, _>>()?;
    attach(
        &mut discussions,
        notes.into_iter().take(MAX_NOTES).collect(),
    );
    Ok(DiscussionPage {
        discussions,
        truncated,
    })
}

/// Discussion `id` of merge request `mr` without its notes (`None`: the
/// merge request has no such discussion). The route loader of a
/// resolution reads it before `authorize()`; the command reads it again
/// under its lock.
pub async fn discussion(
    db: &Db,
    mr: MergeRequestId,
    id: DiscussionId,
) -> Result<Option<StoredDiscussion>, ForgeError> {
    const QUERY: &str = "review.discussion";
    let row: Option<DiscussionRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.merge_request_discussions \
                 WHERE merge_request_id = $1 AND id = $2",
                discussion_columns()
            ))
            .bind(mr.get())
            .bind(id.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(DiscussionRow::parse).transpose()
}
