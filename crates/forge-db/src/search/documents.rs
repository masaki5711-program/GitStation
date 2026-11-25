//! The documents of the search index (plan 20 K4, ADR-0040 Amendment K4):
//! the `search.index_documents` job and its writes.
//!
//! - **Trigger**: the `search-index` subscriber enqueues one job per
//!   `repository.pushed` that moved a branch ([`documents_job`], deduped
//!   per event). Whether the branch was the **default** one is git's
//!   (`HEAD`), so the job decides it: it reads the default branch's tip and
//!   does nothing when the tip is the one already indexed (a push to
//!   another branch leaves the tip where it was). `forge search reindex`
//!   enqueues a full job per project.
//! - **Writes** ([`apply_documents`]): one transaction under the rebuild's
//!   bounds. The project's `search_index_state` row is locked first and the
//!   SHA the job started from is compared (a newer job may have indexed in
//!   between: [`Applied::Moved`], the job reads again); then the changed
//!   entries are written (unchanged ones are not rewritten), the rows of
//!   paths that are no document at the tip are deleted, and the state is
//!   recorded: the tip when the read was complete, `failed` with
//!   `FG-KNOW-0002` and no SHA when it hit a bound (the next push reads
//!   the whole tip again).
//! - **Fail closed**: a read that hit a bound leaves every document row it
//!   could not read `confidential` (an AI audience never sees a row whose
//!   current text is unknown), and deletes only rows whose path the
//!   complete listing no longer holds.

use std::time::Duration;

use forge_core::search::SourceKind;
use forge_core::{ProjectId, RepoId, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{log_item, upsert, Entry, Origin};
use crate::jobs::{enqueue_on, DedupKey, Enqueued, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::SourceEvent;
use crate::{query_error, read_query_error, Db};

/// The job kind.
pub const DOCUMENTS_JOB: &str = "search.index_documents";
/// Runs before the job is dead.
pub const DOCUMENTS_JOB_ATTEMPTS: u8 = 5;
/// One run at most (the reads are bounded by the browse limits; the
/// writes by the transaction's statement timeout).
pub const DOCUMENTS_JOB_TIMEOUT: Duration = Duration::from_secs(600);
/// The longest path stored as a key (the column's CHECK is 1024 bytes).
pub const MAX_DOCUMENT_KEY_BYTES: usize = 1_024;
/// The rows of paths that are no document at the tip.
const REMOVE_PATHS_SQL: &str = "DELETE FROM forge.search_entries WHERE source_kind = 'document' \
     AND project_id = $1 AND source_key = ANY ($2::text[])";
/// The rebuild's bounds (`REINDEX_TIMEOUTS_SQL`) as single statements.
pub(crate) const WRITE_BOUNDS: [&str; 2] = [
    "SET LOCAL statement_timeout = 30000",
    "SET LOCAL lock_timeout = 5000",
];
/// The branch prefix a push must move for a job to be enqueued.
const BRANCH_PREFIX: &str = "refs/heads/";

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", DOCUMENTS_JOB)
}

/// What one job asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentsRequest {
    pub project: ProjectId,
    /// Read and write the whole tip (the rebuild), not only what changed.
    pub full: bool,
}

/// The job of `request`; `what` makes the dedup key (`push:<seq>`,
/// `reindex:<request id>`).
pub fn documents_job(
    request: DocumentsRequest,
    what: &str,
    origin: RequestId,
    event_seq: Option<i64>,
) -> Result<NewJob, ForgeError> {
    let key = format!("search-docs:{}:{what}", request.project.get());
    Ok(NewJob {
        kind: JobKind::parse(DOCUMENTS_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({
            "project_id": request.project.get(),
            "full": request.full,
        }),
        max_attempts: MaxAttempts::new(DOCUMENTS_JOB_ATTEMPTS)
            .ok_or_else(|| internal("invalid_max_attempts"))?,
        timeout: JobTimeout::new(DOCUMENTS_JOB_TIMEOUT)
            .ok_or_else(|| internal("invalid_job_timeout"))?,
        dedup_key: Some(DedupKey::parse(&key).ok_or_else(|| internal("invalid_dedup_key"))?),
        origin_request_id: origin,
        origin_event_seq: event_seq,
    })
}

/// The request of a claimed job's payload (`FG-SYS-0001
/// reason=invalid_job_payload` when it is malformed: the job is dead).
pub fn documents_payload(payload: &serde_json::Value) -> Result<DocumentsRequest, ForgeError> {
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or_else(|| internal("invalid_job_payload"))?;
    let full = match payload.get("full") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| internal("invalid_job_payload"))?,
    };
    Ok(DocumentsRequest {
        project: ProjectId::new(project),
        full,
    })
}

/// Enqueue `project`'s documents job on `conn` (the subscriber's and the
/// rebuild's transaction).
pub(crate) async fn enqueue_documents(
    conn: &mut PgConnection,
    request: DocumentsRequest,
    what: &str,
    origin: RequestId,
    event_seq: Option<i64>,
) -> Result<Enqueued, ForgeError> {
    enqueue_on(conn, &documents_job(request, what, origin, event_seq)?).await
}

/// Whether a `repository.pushed` payload moved a branch (`refs`, each with
/// `ref`). A malformed payload is the caller's `invalid_event_payload`.
pub(crate) fn moved_a_branch(event: &SourceEvent) -> Option<bool> {
    let refs = event.payload.get("refs")?.as_array()?;
    // A cut list may have left the branch out (review K4): the job decides.
    let cut = event
        .payload
        .get("refs_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if cut {
        return Some(true);
    }
    let mut any = false;
    for entry in refs {
        let name = entry.get("ref")?.as_str()?;
        any |= name.starts_with(BRANCH_PREFIX);
    }
    Some(any)
}

/// `project`'s repository when it is ready (`None`: no such project, or
/// its repository is still being provisioned — nothing to index yet).
pub async fn documents_repository(
    db: &Db,
    project: ProjectId,
) -> Result<Option<RepoId>, ForgeError> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM forge.repositories WHERE project_id = $1 AND state = 'ready'",
    )
    .bind(project.get())
    .fetch_optional(db.write_pool())
    .await
    .map_err(|e| read_query_error("search.documents_repository", e))?;
    Ok(id.map(RepoId::new))
}

/// The default-branch SHA whose documents are indexed (`None`: never
/// indexed completely).
pub async fn indexed_documents_sha(
    db: &Db,
    project: ProjectId,
) -> Result<Option<String>, ForgeError> {
    let sha: Option<Option<String>> = sqlx::query_scalar(
        "SELECT indexed_sha FROM forge.search_index_state WHERE project_id = $1",
    )
    .bind(project.get())
    .fetch_optional(db.write_pool())
    .await
    .map_err(|e| read_query_error("search.documents_state", e))?;
    Ok(sha.flatten())
}

/// One document as it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentEntry {
    /// Its path (the key).
    pub path: String,
    /// `title`, else its id.
    pub title: String,
    /// The id, headings and block ids (weight B).
    pub ids: String,
    /// The text after the front matter (weight C; cut at 256 KB).
    pub body: String,
    /// At the tip: `public`, `internal` or `confidential`.
    pub classification: &'static str,
}

/// Which rows besides the written ones an update concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentScope {
    /// Only these paths changed: the rows of `removed` (paths that are no
    /// document at the tip) are deleted; every other row stays.
    Changed { removed: Vec<String> },
    /// The whole tip was read. A complete read deletes every other
    /// document row. A read that hit a bound deletes the rows whose path
    /// `listed` (the complete listing of the tip's files; `None` when the
    /// listing itself was cut) does not hold, and leaves the others
    /// `confidential`.
    Full { listed: Option<Vec<String>> },
}

/// One documents update of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentUpdate {
    /// The default branch's tip read.
    pub tip: String,
    /// The indexed SHA the job started from (compared under the lock).
    pub base: Option<String>,
    pub entries: Vec<DocumentEntry>,
    pub scope: DocumentScope,
    /// The first bound the read hit (`forge_checks::doc_lint`'s name).
    pub incomplete: Option<&'static str>,
}

/// What [`apply_documents`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Written; the counts of rows written (changed or not) and deleted.
    Written { entries: u64, removed: u64 },
    /// Another job indexed in between (the state's SHA is no longer the
    /// base): nothing was written; read again.
    Moved,
    /// The project is gone: nothing was written.
    Gone,
}

/// Write `update` for `project` in one transaction (module docs).
pub async fn apply_documents(
    db: &Db,
    request: &RequestId,
    project: ProjectId,
    update: &DocumentUpdate,
) -> Result<Applied, ForgeError> {
    let origin = Origin::of_request(request);
    let mut tx = db
        .write_pool()
        .begin()
        .await
        .map_err(|e| query_error("search.documents_begin", e))?;
    // The rebuild's bounds (REINDEX_TIMEOUTS_SQL), one statement each: the
    // job's future must be Send, and `raw_sql`'s is not provably so.
    for bound in WRITE_BOUNDS {
        sqlx::query(bound)
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error("search.documents_timeouts", e))?;
    }
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forge.projects WHERE id = $1)")
            .bind(project.get())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error("search.documents_project", e))?;
    if !exists {
        return Ok(Applied::Gone);
    }
    // The state row is the per-project lock of the documents' writers.
    sqlx::query(
        "INSERT INTO forge.search_index_state (project_id, state) VALUES ($1, 'ok') \
         ON CONFLICT (project_id) DO NOTHING",
    )
    .bind(project.get())
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("search.documents_lock", e))?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT indexed_sha FROM forge.search_index_state WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project.get())
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| query_error("search.documents_lock", e))?;
    if current != update.base {
        return Ok(Applied::Moved);
    }
    // The documents' change time: when their change reached the index.
    let updated: String = sqlx::query_scalar(
        "SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| query_error("search.documents_now", e))?;
    let mut written = 0u64;
    for doc in &update.entries {
        write_entry(&mut tx, &origin, project, doc, &updated).await?;
        written += 1;
    }
    let paths: Vec<String> = update.entries.iter().map(|d| d.path.clone()).collect(); // clone: bound
                                                                                      // Chosen before any await (a borrow of the scope held across one makes
                                                                                      // the future's Send proof higher-ranked).
    let (gone, listed): (Option<Vec<String>>, Option<Vec<String>>) = match &update.scope {
        DocumentScope::Changed { removed } => (Some(removed.clone()), None), // clone: bound
        DocumentScope::Full { listed } => (None, listed.clone()),            // clone: bound
    };
    let removed = match gone {
        Some(gone) => remove_paths(&mut tx, project, gone).await?,
        None => full_scope(&mut tx, project, paths, listed, update.incomplete).await?,
    };
    let (state, code, sha) = match update.incomplete {
        None => ("ok", None, Some(update.tip.as_str())),
        Some(_) => ("failed", Some(ErrorCode::SearchIndexItemFailed.id()), None),
    };
    sqlx::query(
        "UPDATE forge.search_index_state SET indexed_sha = $2, state = $3, \
           last_error_code = $4, \
           attempts = CASE WHEN $3 = 'ok' THEN 0 ELSE attempts + 1 END, updated_at = now() \
         WHERE project_id = $1",
    )
    .bind(project.get())
    .bind(sha)
    .bind(state)
    .bind(code)
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("search.documents_state", e))?;
    tx.commit()
        .await
        .map_err(|e| query_error("search.documents_commit", e))?;
    Ok(Applied::Written {
        entries: written,
        removed,
    })
}

/// One document row (a path over the key's limit is logged and skipped:
/// never cut into another document's key).
async fn write_entry(
    conn: &mut PgConnection,
    origin: &Origin,
    project: ProjectId,
    doc: &DocumentEntry,
    updated: &str,
) -> Result<(), ForgeError> {
    if doc.path.len() > MAX_DOCUMENT_KEY_BYTES {
        log_item(
            origin,
            "key_too_long",
            SourceKind::Document,
            project,
            "",
            MAX_DOCUMENT_KEY_BYTES,
        );
        return Ok(());
    }
    upsert(
        conn,
        origin,
        &Entry {
            kind: SourceKind::Document,
            project,
            key: doc.path.clone(), // clone: the entry owns its key
            title: &doc.title,
            ids: doc.ids.clone(), // clone: as the key
            body: &doc.body,
            classification: doc.classification,
        },
        updated,
    )
    .await
}

/// [`DocumentScope::Changed`]'s removals; the rows removed.
async fn remove_paths(
    conn: &mut PgConnection,
    project: ProjectId,
    gone: Vec<String>,
) -> Result<u64, ForgeError> {
    Ok(sqlx::query(REMOVE_PATHS_SQL)
        .bind(project.get())
        .bind(&gone)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.documents_remove", e))?
        .rows_affected())
}

/// [`DocumentScope::Full`]'s deletes (and withholding); the rows deleted.
async fn full_scope(
    conn: &mut PgConnection,
    project: ProjectId,
    written: Vec<String>,
    listed: Option<Vec<String>>,
    incomplete: Option<&'static str>,
) -> Result<u64, ForgeError> {
    if incomplete.is_none() {
        return Ok(sqlx::query(
            "DELETE FROM forge.search_entries WHERE source_kind = 'document' \
             AND project_id = $1 AND NOT (source_key = ANY ($2::text[]))",
        )
        .bind(project.get())
        .bind(&written)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.documents_clear", e))?
        .rows_affected());
    }
    let mut removed = 0;
    if let Some(listed) = &listed {
        removed = sqlx::query(
            "DELETE FROM forge.search_entries WHERE source_kind = 'document' \
             AND project_id = $1 AND NOT (source_key = ANY ($2::text[])) \
             AND NOT (source_key = ANY ($3::text[]))",
        )
        .bind(project.get())
        .bind(&written)
        .bind(listed)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("search.documents_clear", e))?
        .rows_affected();
    }
    // What was not read is not known to be open (fail closed).
    sqlx::query(
        "UPDATE forge.search_entries SET classification = 'confidential', indexed_at = now() \
         WHERE source_kind = 'document' AND project_id = $1 \
           AND NOT (source_key = ANY ($2::text[])) AND classification <> 'confidential'",
    )
    .bind(project.get())
    .bind(&written)
    .execute(&mut *conn)
    .await
    .map_err(|e| query_error("search.documents_withhold", e))?;
    Ok(removed)
}

/// Record that `project`'s documents could not be indexed (the job's last
/// attempt): `failed` with `FG-KNOW-0002`, no SHA (the next job reads the
/// whole tip), and every document row of the project left `confidential`
/// (what the tip says now is unknown). A failure to record it is returned
/// (never a silent gap).
pub async fn record_documents_failed(db: &Db, project: ProjectId) -> Result<(), ForgeError> {
    // One statement: the rows are withheld with the state (review K4: the
    // tip may have marked a document confidential that was never re-read).
    sqlx::query(
        "WITH withheld AS ( \
           UPDATE forge.search_entries SET classification = 'confidential', indexed_at = now() \
           WHERE source_kind = 'document' AND project_id = $1 \
             AND classification <> 'confidential' RETURNING 1) \
         INSERT INTO forge.search_index_state (project_id, indexed_sha, state, last_error_code, \
           attempts, updated_at) \
         SELECT $1, NULL, 'failed', $2, 1, now() \
         WHERE EXISTS (SELECT 1 FROM forge.projects WHERE id = $1) \
         ON CONFLICT (project_id) DO UPDATE SET indexed_sha = NULL, state = 'failed', \
           last_error_code = EXCLUDED.last_error_code, \
           attempts = forge.search_index_state.attempts + 1, updated_at = now()",
    )
    .bind(project.get())
    .bind(ErrorCode::SearchIndexItemFailed.id())
    .execute(db.write_pool())
    .await
    .map_err(|e| query_error("search.documents_failed_state", e))?;
    Ok(())
}
