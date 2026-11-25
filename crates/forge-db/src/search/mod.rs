//! The search index (plan 20 K2, ADR-0040): `forge.search_entries` and
//! `forge.search_index_state`, derived from the source tables.
//!
//! - **Writers**: the `search-index` subscriber ([`SearchIndex`],
//!   `ordered`: a failed step is retried on the same event, never skipped)
//!   refreshes one entry from its source row when a project or merge
//!   request event arrives; [`reindex`] (`forge search reindex
//!   [--project <id>]`) rebuilds one project or all of them. Both re-read
//!   the source rows: the event says *what* changed, the table says *how
//!   it is now*, so a replayed or late event never writes stale text.
//! - **Text**: the vector is `forge_core::search_text`'s literal (title A,
//!   path and ids B, body C), cast by the database (`$n::tsvector`); the
//!   body is cut at [`MAX_BODY_BYTES`] and the entry marked `truncated`
//!   (logged once, `FG-KNOW-0002 reason=truncated`).
//! - **Gone things**: a merge request or project that no longer exists has
//!   its rows deleted; a purged project's rows go with `project.purged`
//!   (there is no foreign key: the purge itself stays unchanged).
//! - **Readers**: [`candidates`] (K3) reads the ranked candidates of a
//!   query on the read pool under its own statement timeout; it decides
//!   nothing — forge-web decides every hit with `authorize()`.

use forge_core::search::SourceKind;
use forge_core::search_text::{IndexText, Weight};
use forge_core::{EventType, ProjectId, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::subscribers::{DerivedTx, HandleFuture, Ordering, SourceEvent, Subscriber};
use crate::{query_error, Db};

mod documents;
mod read;

pub use documents::{
    apply_documents, documents_job, documents_payload, documents_repository, indexed_documents_sha,
    record_documents_failed, Applied, DocumentEntry, DocumentScope, DocumentUpdate,
    DocumentsRequest, DOCUMENTS_JOB, DOCUMENTS_JOB_ATTEMPTS, DOCUMENTS_JOB_TIMEOUT,
    MAX_DOCUMENT_KEY_BYTES,
};
pub use read::{
    candidates, MAX_CANDIDATES, MAX_OFFSET, MAX_RANKED, SEARCH_STATEMENT_TIMEOUT,
    SNIPPET_SOURCE_CHARS,
};

/// The subscriber's name (`forge.event_subscribers`).
pub const SEARCH_INDEX_SUBSCRIBER: &str = "search-index";
/// The most body text one entry stores (the snippet's source).
pub const MAX_BODY_BYTES: usize = 262_144;
/// The most projects one group move / user rename refreshes in one step
/// (`FG-KNOW-0002 reason=subtree_too_large` past it: run the rebuild).
pub const MAX_SUBTREE_PROJECTS: usize = 2_000;
/// The longest title stored (the column's CHECK is 4096 bytes).
const MAX_TITLE_BYTES: usize = 4_096;

/// `text` cut to at most `max` bytes at a character boundary; `true` when
/// something was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text.get(..end).unwrap_or_default(), true)
}

/// Where a write comes from, for the logs.
#[derive(Debug, Clone)]
pub(crate) struct Origin {
    request_id: String,
    seq: Option<i64>,
}

impl Origin {
    fn of_event(event: &SourceEvent) -> Self {
        Self {
            request_id: event.request_id.clone(), // clone: logged after the borrow ends
            seq: Some(event.seq),
        }
    }

    fn of_request(request: &RequestId) -> Self {
        Self {
            request_id: request.to_string(),
            seq: None,
        }
    }
}

/// One entry as written.
struct Entry<'a> {
    kind: SourceKind,
    project: ProjectId,
    key: String,
    title: &'a str,
    ids: String,
    body: &'a str,
    /// `public`, `internal` or `confidential` (projects and merge requests
    /// are `internal`; a document's is its classification at the tip).
    classification: &'static str,
}

/// Log `FG-KNOW-0002` once for an entry that could not be stored whole.
fn log_item(
    origin: &Origin,
    reason: &'static str,
    kind: SourceKind,
    project: ProjectId,
    key: &str,
    limit: usize,
) {
    tracing::warn!(
        target: "forge_db",
        event = "forge.search.index_item",
        error_code = ErrorCode::SearchIndexItemFailed.id(),
        reason,
        source_kind = kind.as_str(),
        project_id = project.get(),
        source_key = %key,
        limit,
        request_id = %origin.request_id,
        source_seq = origin.seq,
        "a search entry was stored incompletely"
    );
}

/// Upsert one entry (title A, ids B, body C). An entry whose text, vector,
/// classification and cut are unchanged is not rewritten (no dead tuple,
/// no GIN pending-list entry: events like a draft toggle repeat often).
async fn upsert(
    conn: &mut PgConnection,
    origin: &Origin,
    entry: &Entry<'_>,
    updated_at: &str,
) -> Result<(), ForgeError> {
    let (title, _) = cut(entry.title, MAX_TITLE_BYTES);
    let (body, body_cut) = cut(entry.body, MAX_BODY_BYTES);
    let vector = IndexText::new()
        .with(Weight::A, title)
        .with(Weight::B, &entry.ids)
        .with(Weight::C, body)
        .build();
    let truncated = body_cut || vector.truncated;
    if truncated {
        let limit = if body_cut {
            MAX_BODY_BYTES
        } else {
            forge_core::search_text::MAX_INDEX_TERMS
        };
        log_item(
            origin,
            "truncated",
            entry.kind,
            entry.project,
            &entry.key,
            limit,
        );
    }
    sqlx::query(
        "INSERT INTO forge.search_entries (source_kind, project_id, source_key, title, \
           body_text, tokens, classification, truncated, source_updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6::tsvector, $9, $7, $8::timestamptz) \
         ON CONFLICT (source_kind, project_id, source_key) DO UPDATE SET \
           title = EXCLUDED.title, body_text = EXCLUDED.body_text, tokens = EXCLUDED.tokens, \
           classification = EXCLUDED.classification, truncated = EXCLUDED.truncated, \
           source_updated_at = EXCLUDED.source_updated_at, indexed_at = now() \
         WHERE (forge.search_entries.title, forge.search_entries.body_text, \
                forge.search_entries.tokens, forge.search_entries.classification, \
                forge.search_entries.truncated) \
           IS DISTINCT FROM (EXCLUDED.title, EXCLUDED.body_text, EXCLUDED.tokens, \
                EXCLUDED.classification, EXCLUDED.truncated)",
    )
    .bind(entry.kind.as_str())
    .bind(entry.project.get())
    .bind(&entry.key)
    .bind(title)
    .bind(body)
    .bind(&vector.literal)
    .bind(truncated)
    .bind(updated_at)
    .bind(entry.classification)
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("search.upsert", e))?;
    Ok(())
}

/// Refresh a project's own entry from `forge.projects` (path, name,
/// description); a project that no longer exists loses all its rows.
pub(crate) async fn refresh_project(
    conn: &mut PgConnection,
    origin: &Origin,
    project: ProjectId,
) -> Result<(), ForgeError> {
    let row: Option<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT p.display_name, p.description, n.full_path, \
           to_char(p.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
         FROM forge.projects p \
         LEFT JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
         WHERE p.id = $1",
    )
    .bind(project.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("search.project_source", e))?;
    let Some((name, description, path, updated)) = row else {
        return delete_project(conn, project).await;
    };
    let path = path.unwrap_or_default();
    upsert(
        conn,
        origin,
        &Entry {
            kind: SourceKind::Project,
            project,
            key: String::new(),
            title: &name,
            ids: path,
            body: &description,
            classification: "internal",
        },
        &updated,
    )
    .await
}

/// Refresh one merge request's entry from `forge.merge_requests` (title,
/// `!iid`, description); a merge request that no longer exists loses its
/// row.
pub(crate) async fn refresh_merge_request(
    conn: &mut PgConnection,
    origin: &Origin,
    project: ProjectId,
    iid: i64,
) -> Result<(), ForgeError> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT title, description, \
           to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
         FROM forge.merge_requests WHERE project_id = $1 AND iid = $2",
    )
    .bind(project.get())
    .bind(iid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("search.merge_request_source", e))?;
    let Some((title, description, updated)) = row else {
        sqlx::query(
            "DELETE FROM forge.search_entries WHERE source_kind = 'merge_request' \
             AND project_id = $1 AND source_key = $2",
        )
        .bind(project.get())
        .bind(iid.to_string())
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.merge_request_delete", e))?;
        return Ok(());
    };
    upsert(
        conn,
        origin,
        &Entry {
            kind: SourceKind::MergeRequest,
            project,
            key: iid.to_string(),
            title: &title,
            ids: format!("!{iid}"),
            body: &description,
            classification: "internal",
        },
        &updated,
    )
    .await
}

/// Delete every row of `project` (purged, or gone).
pub(crate) async fn delete_project(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<(), ForgeError> {
    // The state row first: it is the documents job's per-project lock
    // (documents::apply_documents takes it before the entries), so both
    // writers lock in the same order (review K4: no deadlock).
    sqlx::query("DELETE FROM forge.search_index_state WHERE project_id = $1")
        .bind(project.get())
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.state_delete", e))?;
    sqlx::query("DELETE FROM forge.search_entries WHERE project_id = $1")
        .bind(project.get())
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.project_delete", e))?;
    Ok(())
}

/// Refresh the project entries below the namespace of `kind` (`group` /
/// `user`) owned by `owner` (a group move, a user rename: their paths
/// changed), at most [`MAX_SUBTREE_PROJECTS`].
async fn refresh_subtree(
    conn: &mut PgConnection,
    origin: &Origin,
    kind: &'static str,
    owner: i64,
) -> Result<(), ForgeError> {
    refresh_subtree_capped(conn, origin, kind, owner, MAX_SUBTREE_PROJECTS).await
}

/// The projects below a namespace, in id order (`$3` = how many to skip,
/// `$4` = how many at most).
const SUBTREE_SQL: &str = "WITH RECURSIVE tree (id, kind, owner_id) AS ( \
       SELECT n.id, n.kind, n.owner_id FROM forge.namespaces n \
       WHERE n.kind = $1 AND n.owner_id = $2 \
       UNION ALL \
       SELECT c.id, c.kind, c.owner_id FROM forge.namespaces c JOIN tree t ON c.parent_id = t.id) \
     SELECT owner_id FROM tree WHERE kind = 'project' ORDER BY owner_id OFFSET $3 LIMIT $4";

/// [`refresh_subtree`] with the cap given (tests lower it). Past the cap,
/// the projects left with their old path are recorded `failed`
/// (`FG-KNOW-0002`) in search_index_state, so the staleness is visible and
/// `forge search reindex` repairs it (runbook search-index).
pub(crate) async fn refresh_subtree_capped(
    conn: &mut PgConnection,
    origin: &Origin,
    kind: &'static str,
    owner: i64,
    cap: usize,
) -> Result<(), ForgeError> {
    let cap_i64 = i64::try_from(cap).unwrap_or(i64::MAX);
    let projects: Vec<i64> = sqlx::query_scalar(SUBTREE_SQL)
        .bind(kind)
        .bind(owner)
        .bind(0i64)
        .bind(cap_i64.saturating_add(1))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("search.subtree", e))?;
    if projects.len() > cap {
        log_item(
            origin,
            "subtree_too_large",
            SourceKind::Project,
            ProjectId::new(owner),
            "",
            cap,
        );
        sqlx::query(&format!(
            "INSERT INTO forge.search_index_state (project_id, state, last_error_code, attempts, updated_at) \
             SELECT s.owner_id, 'failed', $5, 1, now() FROM ({}) AS s (owner_id) \
             ON CONFLICT (project_id) DO UPDATE SET state = 'failed', \
               last_error_code = EXCLUDED.last_error_code, \
               attempts = forge.search_index_state.attempts + 1, updated_at = now()",
            SUBTREE_SQL
        ))
        .bind(kind)
        .bind(owner)
        .bind(cap_i64)
        .bind(i64::MAX)
        .bind(ErrorCode::SearchIndexItemFailed.id())
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.subtree_stale", e))?;
    }
    for id in projects.into_iter().take(cap) {
        refresh_project(conn, origin, ProjectId::new(id)).await?;
    }
    Ok(())
}

/// The `search-index` subscriber (`ordered`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SearchIndex;

fn bad_payload(event: &SourceEvent, field: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "invalid_event_payload")
        .with_context("subscriber", SEARCH_INDEX_SUBSCRIBER)
        .with_context("event_type", event.event_type.clone()) // clone: the context owns it
        .with_context("field", field)
        .with_context("seq", event.seq)
}

fn project_of(event: &SourceEvent) -> Result<ProjectId, ForgeError> {
    event
        .project_id
        .ok_or_else(|| bad_payload(event, "project_id"))
}

fn iid_of(event: &SourceEvent) -> Result<i64, ForgeError> {
    event
        .payload
        .get("iid")
        .and_then(serde_json::Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| bad_payload(event, "iid"))
}

impl SearchIndex {
    async fn on(
        kind: EventType,
        event: &SourceEvent,
        conn: &mut PgConnection,
    ) -> Result<(), ForgeError> {
        let origin = Origin::of_event(event);
        match kind {
            EventType::ProjectCreated
            | EventType::RepositoryCreated
            | EventType::ProjectVisibilityChanged
            | EventType::ProjectDeleted
            | EventType::ProjectRestored
            | EventType::ProjectMoved => refresh_project(conn, &origin, project_of(event)?).await,
            EventType::ProjectPurged => delete_project(conn, project_of(event)?).await,
            EventType::GroupMoved => {
                refresh_subtree(conn, &origin, "group", event.aggregate_id).await
            }
            EventType::UserRenamed => {
                refresh_subtree(conn, &origin, "user", event.aggregate_id).await
            }
            // Plan 20 K4: a moved branch may be the default one (the job
            // decides: the default branch is git's `HEAD`).
            EventType::RepositoryPushed => {
                let project = project_of(event)?;
                if !documents::moved_a_branch(event).ok_or_else(|| bad_payload(event, "refs"))? {
                    return Ok(());
                }
                let origin = event.request_id().unwrap_or_else(RequestId::generate);
                let _enqueued = documents::enqueue_documents(
                    conn,
                    DocumentsRequest {
                        project,
                        full: false,
                    },
                    &format!("push:{}", event.seq),
                    origin,
                    Some(event.seq),
                )
                .await?;
                Ok(())
            }
            EventType::MergeRequestOpened
            | EventType::MergeRequestUpdated
            | EventType::MergeRequestDraftChanged
            | EventType::MergeRequestClosed
            | EventType::MergeRequestReopened
            | EventType::MergeRequestMerged => {
                refresh_merge_request(conn, &origin, project_of(event)?, iid_of(event)?).await
            }
            EventType::TokenCreated
            | EventType::TokenRevoked
            | EventType::MemberAdded
            | EventType::MemberRoleChanged
            | EventType::MemberRemoved
            | EventType::ProtectedRefCreated
            | EventType::ProtectedRefUpdated
            | EventType::ProtectedRefDeleted
            | EventType::MergeRequestPushed
            | EventType::MergeRequestConflictDetected
            | EventType::MergeRequestConflictResolved
            | EventType::MergeRequestConflictPredicted
            | EventType::MergeRequestCommented
            | EventType::MergeRequestSourceBranchDeleted
            | EventType::MergeRequestApproved
            | EventType::MergeRequestUnapproved
            | EventType::CodeownersEnforcementChanged
            | EventType::CodeownersEnforcementRestored
            | EventType::CheckCompleted
            | EventType::WorkItemLinked
            | EventType::WorkItemUnlinked
            | EventType::MergeRequestWorkItemsChanged
            | EventType::MergeRequestRelinkRequested
            | EventType::WebhookDisabled
            | EventType::SpecReleased
            | EventType::MergeRequestRevertDrafted
            | EventType::MergeRequestAutofixFinished => Ok(()),
        }
    }
}

impl Subscriber for SearchIndex {
    fn name(&self) -> &'static str {
        SEARCH_INDEX_SUBSCRIBER
    }

    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let Some(kind) = EventType::parse(&event.event_type) else {
                // A type this build does not know (a newer writer).
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.subscribers.unknown_event_type",
                    subscriber = SEARCH_INDEX_SUBSCRIBER,
                    request_id = %event.request_id,
                    source_seq = event.seq,
                    event_type = %event.event_type,
                    "an event type this build does not know was passed over"
                );
                return Ok(());
            };
            Self::on(kind, event, tx.conn()).await
        })
    }
}

/// What a rebuild did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReindexReport {
    pub projects: u64,
    pub merge_requests: u64,
    /// Projects whose rebuild failed (each logged with FG-KNOW-0002
    /// `reason=write_failed` and recorded in search_index_state).
    pub failed: Vec<ProjectId>,
    /// Rows of projects that no longer exist, deleted.
    pub orphans_removed: u64,
}

/// The bounds of one project's rebuild transaction (runbook search-index):
/// a statement may run 30 s, a lock is waited for 5 s.
const REINDEX_TIMEOUTS_SQL: &str =
    "SET LOCAL statement_timeout = 30000; SET LOCAL lock_timeout = 5000";
/// Orphan rows removed per statement.
const ORPHAN_BATCH: i64 = 1_000;

/// Rebuild one project's rows in one transaction: write the project's
/// entry and every merge request's (unchanged rows are not rewritten),
/// remove the rows of merge requests that are gone, record the state.
async fn reindex_one(db: &Db, origin: &Origin, project: ProjectId) -> Result<u64, ForgeError> {
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error("search.reindex_begin", e))?;
    sqlx::raw_sql(REINDEX_TIMEOUTS_SQL)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error("search.reindex_timeouts", e))?;
    refresh_project(&mut tx, origin, project).await?;
    let iids: Vec<i64> = sqlx::query_scalar(
        "SELECT iid FROM forge.merge_requests WHERE project_id = $1 ORDER BY iid",
    )
    .bind(project.get())
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| query_error("search.reindex_merge_requests", e))?;
    for iid in &iids {
        refresh_merge_request(&mut tx, origin, project, *iid).await?;
    }
    // Plan 20 K4: the documents are read from git by the worker's job.
    let has_repository: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.repositories WHERE project_id = $1)",
    )
    .bind(project.get())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| query_error("search.reindex_repository", e))?;
    if has_repository {
        let origin = RequestId::parse(&origin.request_id).unwrap_or_else(RequestId::generate);
        let _enqueued = documents::enqueue_documents(
            &mut tx,
            DocumentsRequest {
                project,
                full: true,
            },
            &format!("reindex:{origin}"),
            origin.clone(), // clone: the job owns its request id
            None,
        )
        .await?;
    }
    let keys: Vec<String> = iids.iter().map(i64::to_string).collect();
    sqlx::query(
        "DELETE FROM forge.search_entries WHERE project_id = $1 \
         AND source_kind = 'merge_request' AND NOT (source_key = ANY ($2::text[]))",
    )
    .bind(project.get())
    .bind(&keys)
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("search.reindex_clear", e))?;
    sqlx::query(
        "INSERT INTO forge.search_index_state (project_id, state, last_error_code, attempts, updated_at) \
         SELECT $1, 'ok', NULL, 0, now() WHERE EXISTS (SELECT 1 FROM forge.projects WHERE id = $1) \
         ON CONFLICT (project_id) DO UPDATE SET state = 'ok', last_error_code = NULL, \
           attempts = 0, updated_at = now()",
    )
    .bind(project.get())
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("search.reindex_state", e))?;
    tx.commit()
        .await
        .map_err(|e| query_error("search.reindex_commit", e))?;
    Ok(u64::try_from(iids.len()).unwrap_or(u64::MAX))
}

/// Record a failed rebuild of `project` (a failure to record it fails the
/// rebuild: never a silent gap).
async fn record_failure(db: &Db, project: ProjectId) -> Result<(), ForgeError> {
    sqlx::query(
        "INSERT INTO forge.search_index_state (project_id, state, last_error_code, attempts, updated_at) \
         VALUES ($1, 'failed', $2, 1, now()) \
         ON CONFLICT (project_id) DO UPDATE SET state = 'failed', \
           last_error_code = EXCLUDED.last_error_code, \
           attempts = forge.search_index_state.attempts + 1, updated_at = now()",
    )
    .bind(project.get())
    .bind(ErrorCode::SearchIndexItemFailed.id())
    .execute(db.write_pool())
    .await
    .map_err(|e| query_error("search.reindex_failure_state", e))?;
    Ok(())
}

/// Rebuild the index of `project`, or of every project (`None`; rows of
/// projects that no longer exist are deleted too). Each project is its own
/// transaction; a project that fails is logged (`FG-KNOW-0002
/// reason=write_failed`, the cause chained), recorded in
/// search_index_state and listed in the report, and the others go on.
/// Idempotent: running it twice leaves the same rows.
pub async fn reindex(
    db: &Db,
    request: &RequestId,
    project: Option<ProjectId>,
) -> Result<ReindexReport, ForgeError> {
    let origin = Origin::of_request(request);
    let mut report = ReindexReport::default();
    let projects: Vec<ProjectId> = match project {
        Some(p) => vec![p],
        None => {
            // In batches: one statement never holds many row locks.
            loop {
                let removed = sqlx::query(
                    "DELETE FROM forge.search_entries WHERE id IN ( \
                       SELECT e.id FROM forge.search_entries e WHERE NOT EXISTS \
                         (SELECT 1 FROM forge.projects p WHERE p.id = e.project_id) \
                       LIMIT $1)",
                )
                .bind(ORPHAN_BATCH)
                .execute(db.write_pool())
                .await
                .map_err(|e| query_error("search.reindex_orphans", e))?
                .rows_affected();
                report.orphans_removed += removed;
                if removed < u64::try_from(ORPHAN_BATCH).unwrap_or(u64::MAX) {
                    break;
                }
            }
            sqlx::query_scalar::<_, i64>("SELECT id FROM forge.projects ORDER BY id")
                .fetch_all(db.write_pool())
                .await
                .map_err(|e| query_error("search.reindex_projects", e))?
                .into_iter()
                .map(ProjectId::new)
                .collect()
        }
    };
    for p in projects {
        match reindex_one(db, &origin, p).await {
            Ok(mrs) => {
                report.projects += 1;
                report.merge_requests += mrs;
            }
            Err(e) => {
                tracing::warn!(
                    target: "forge_db",
                    event = "forge.search.index_item",
                    error_code = ErrorCode::SearchIndexItemFailed.id(),
                    reason = "write_failed",
                    source_kind = SourceKind::Project.as_str(),
                    project_id = p.get(),
                    cause_code = e.code().id(),
                    request_id = %origin.request_id,
                    "a project's search entries could not be rebuilt"
                );
                record_failure(db, p).await?;
                report.failed.push(p);
            }
        }
    }
    Ok(report)
}

impl forge_core::search::SearchReader for Db {
    fn candidates<'a>(
        &'a self,
        tsquery: &'a str,
        filter: &'a forge_core::search::CandidateFilter,
        offset: u32,
        max_rows: u32,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, Vec<forge_core::search::SearchCandidate>>
    {
        Box::pin(candidates(self, tsquery, filter, offset, max_rows))
    }
}

#[cfg(test)]
mod tests;
