//! Merge requests (M3 R1a, ADR-0024 §1, §6; plan 11 §11.2: the SQL of
//! forge-review lives here). The reads the route loaders and the list use
//! ([`get`], [`list`]), and the commands decided inside their transaction
//! (`commands.rs`): [`open_mr_decided`], [`edit_mr_decided`],
//! [`close_mr_decided`], [`reopen_mr_decided`]; M3 R1b: the versions
//! (`versions.rs`: the version job's [`record_mr_version`]), their hidden
//! refs (`version_refs.rs`: the per merge request lock [`lock_version_refs`] and the
//! administrator's [`purge_mr_version_refs_decided`]) and the
//! `merge-request-refresh` subscriber (`refresh.rs`).
//!
//! The command names carry `_mr_` so they never match the needles of
//! forge-hosting's `decided_callers.rs` (ADR-0024 §11, review #33); a test
//! of their own (`only_forge_review_calls_the_review_commands`) keeps every
//! caller outside forge-db in forge-review, whose functions take
//! `Authorized<MergeRequestCtx>` (forge-db sits below forge-auth and cannot
//! require the proof itself).
//!
//! Each command locks the project row first (the lock of
//! `projects::members` and `protected_refs`), then the merge request row,
//! then the caller's roles and account (the lock order of
//! `projects::update`), and hands those facts to `decide` before anything
//! is written. Every change writes one `merge_request.*` audit row per
//! transition on the project (resource `project`, like `protected_ref.*`)
//! and one event per transition (ADR-0024 §6: exactly one each), deduplicated
//! by `<prefix>:<merge_request_id>:<revision>`.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::{MergeRequestState, RefName};
use forge_errors::{ErrorCode, ForgeError};

use crate::{read_query_error, Db};

mod commands;
mod refresh;
mod version_refs;
pub mod versions;

pub use refresh::{MergeRequestRefresh, REFRESH_SUBSCRIBER};
pub use version_refs::{
    lock_version_refs, purge_mr_version_refs_decided, versions_snapshot, PurgeOutcome, RefsLock,
    VersionsSnapshot, REFS_LOCK_HOLD, REFS_LOCK_WAIT,
};
pub use versions::{
    list_versions, record_mr_version, refresh_job, refresh_payload, refresh_target, RefreshRequest,
    RefreshTarget, RefreshTrigger, StoredVersion, VersionOutcome, REFRESH_JOB, REFRESH_TIMEOUT,
};

pub use commands::{
    close_mr_decided, edit_mr_decided, open_mr_decided, reopen_mr_decided, EditOutcome,
    MergeRequestFacts, MergeRequestPatch, NewMergeRequest, StateOutcome,
};

/// The most merge requests one list call returns.
pub const MAX_PAGE: usize = 100;

/// One merge request as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMergeRequest {
    pub id: MergeRequestId,
    pub project_id: ProjectId,
    /// The project's own number of it.
    pub iid: i64,
    pub title: String,
    pub description: String,
    pub author: UserId,
    pub source_branch: RefName,
    pub target_branch: RefName,
    pub state: MergeRequestState,
    pub draft: bool,
    /// `None` until R1b records versions.
    pub head_sha: Option<String>,
    /// The row's change count (1 at creation).
    pub revision: i64,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub closed_by: Option<UserId>,
}

/// The columns of [`StoredMergeRequest`], in the order of [`Row`].
pub(crate) const COLUMNS: &str = "id, project_id, iid, title, description, author_id, \
     source_branch, target_branch, state, draft, head_sha, revision, \
     to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at, \
     to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS updated_at, \
     to_char(closed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS closed_at, \
     closed_by";

/// One row as read with [`COLUMNS`].
#[derive(sqlx::FromRow)]
pub(crate) struct Row {
    id: i64,
    project_id: i64,
    iid: i64,
    title: String,
    description: String,
    author_id: i64,
    source_branch: String,
    target_branch: String,
    state: String,
    draft: bool,
    head_sha: Option<String>,
    revision: i64,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    closed_by: Option<i64>,
}

/// `FG-SYS-0001 reason=<reason>` for a stored value this build cannot read
/// (the table's CHECKs make it unreachable: a broken invariant).
fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("merge_request_id", id)
}

impl Row {
    pub(crate) fn parse(self) -> Result<StoredMergeRequest, ForgeError> {
        let id = self.id;
        let branch = |raw: &str| {
            RefName::parse(raw)
                .ok()
                .filter(|r| r.branch().is_some())
                .ok_or_else(|| stored_invalid("stored_branch_invalid", id))
        };
        Ok(StoredMergeRequest {
            id: MergeRequestId::new(id),
            project_id: ProjectId::new(self.project_id),
            iid: self.iid,
            source_branch: branch(&self.source_branch)?,
            target_branch: branch(&self.target_branch)?,
            state: MergeRequestState::parse(&self.state)
                .ok_or_else(|| stored_invalid("stored_state_invalid", id))?,
            title: self.title,
            description: self.description,
            author: UserId::new(self.author_id),
            draft: self.draft,
            head_sha: self.head_sha,
            revision: self.revision,
            created_at: self.created_at,
            updated_at: self.updated_at,
            closed_at: self.closed_at,
            closed_by: self.closed_by.map(UserId::new),
        })
    }
}

/// Merge request `iid` of `project` (`None`: the project has no such
/// one), one bounded statement on the read pool. The route loader reads it
/// before `authorize()`; a write reads it again under its lock.
pub async fn get(
    db: &Db,
    project: ProjectId,
    iid: i64,
) -> Result<Option<StoredMergeRequest>, ForgeError> {
    const QUERY: &str = "review.get";
    let row: Option<Row> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM forge.merge_requests WHERE project_id = $1 AND iid = $2"
            ))
            .bind(project.get())
            .bind(iid)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(Row::parse).transpose()
}

/// One page of `project`'s merge requests, newest first: `state` (`None` =
/// every state), `iid < before` when given, at most `limit` (capped at
/// [`MAX_PAGE`]). One bounded statement on the read pool.
pub async fn list(
    db: &Db,
    project: ProjectId,
    state: Option<MergeRequestState>,
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<StoredMergeRequest>, ForgeError> {
    const QUERY: &str = "review.list";
    let limit = i64::try_from(limit.min(MAX_PAGE)).unwrap_or(1);
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM forge.merge_requests WHERE project_id = $1 \
                 AND ($2::text IS NULL OR state = $2) AND ($3::bigint IS NULL OR iid < $3) \
                 ORDER BY iid DESC LIMIT $4"
            ))
            .bind(project.get())
            .bind(state.map(MergeRequestState::as_str))
            .bind(before)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(Row::parse).collect()
}

#[cfg(test)]
mod race_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod version_tests;
