//! The merge results of merge requests (M4 C7, plan 04 M-1 / M-2, ADR-0024
//! Amendment 2026-11-11; migration 20261207100000): the derived table
//! `forge.merge_result_refs`, one row per (merge request, head, target tip)
//! the mergeability job found clean, with the merge commit it wrote
//! (`refs/merge-requests/<iid>/merge`; the git work is forge-review's
//! `mergeability`).
//!
//! - [`merge_result_of`]: the result already written for a pair — the job
//!   reuses it and writes no second commit (once per pair).
//! - [`record_merge_result`]: the row, and in the same transaction — only
//!   when the row is new — the merge request pipeline on the result
//!   (`ci.pipeline_create`, source `merge_request`, the source branch as its
//!   ref, the head's pusher as `triggered_by`). Two jobs racing on one pair:
//!   `merge_result_refs_once` keeps the first row and the loser gets its
//!   result back (`created: false`, nothing enqueued).
//! - [`replace_merge_result`]: a stored result whose commit `gc` pruned is
//!   written again by the job; the row points at the new commit and the
//!   pipeline is asked for on it.
//! - [`merge_runs_on`] / [`merge_runs_for`]: the runs a merge decision reads
//!   for a head — on the head and on each merge result of the head (newest
//!   first, at most [`MAX_MERGE_RESULTS_READ`]) — resolved by
//!   `forge_core::merge_results` against the live target tip.

use forge_core::ids::{MergeRequestId, ProjectId, UserId};
use forge_core::merge_results::{MergeCheckRuns, MergeResultRuns, MAX_MERGE_RESULTS_READ};
use forge_core::{ObjectId, RefName, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::checks::{runs_for, runs_on};
use crate::jobs::enqueue_on;
use crate::pipelines::{create_job, PipelineRequest, Source};
use crate::{query_error, read_query_error, Db};

/// One stored merge result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResultRow {
    pub head: ObjectId,
    pub target: ObjectId,
    pub result: ObjectId,
}

/// What [`record_merge_result`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedResult {
    /// The pair's merge result (the caller's, or the one a racing job
    /// stored first).
    pub result: ObjectId,
    /// The row is new, or its pruned result was replaced (and the pipeline
    /// was enqueued with it).
    pub created: bool,
    /// The merge request pipeline job was enqueued (a deduplicated one is
    /// `false`).
    pub pipeline_enqueued: bool,
}

fn stored_invalid(mr: MergeRequestId, column: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "merge_result_row_invalid")
        .with_context("merge_request_id", mr.get())
        .with_context("column", column)
}

fn oid(mr: MergeRequestId, column: &'static str, raw: &str) -> Result<ObjectId, ForgeError> {
    ObjectId::parse(raw).map_err(|_| stored_invalid(mr, column))
}

/// The merge result stored for (`head`, `target`) of `mr` (`None`: not
/// written yet). On the write pool: the job reads what it may just have
/// written in an earlier attempt.
pub async fn merge_result_of(
    db: &Db,
    mr: MergeRequestId,
    head: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ObjectId>, ForgeError> {
    const QUERY: &str = "review.merge_result_of";
    let raw: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT result_sha FROM forge.merge_result_refs \
                 WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3",
            )
            .bind(mr.get())
            .bind(head.as_str())
            .bind(target.as_str())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    raw.map(|r| oid(mr, "result_sha", &r)).transpose()
}

/// [`merge_result_of`] on the read pool (the merge status).
pub async fn merge_result_of_read(
    db: &Db,
    mr: MergeRequestId,
    head: &ObjectId,
    target: &ObjectId,
) -> Result<Option<ObjectId>, ForgeError> {
    const QUERY: &str = "review.merge_result_read";
    let raw: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT result_sha FROM forge.merge_result_refs \
                 WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3",
            )
            .bind(mr.get())
            .bind(head.as_str())
            .bind(target.as_str())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    raw.map(|r| oid(mr, "result_sha", &r)).transpose()
}

/// Every merge result of `mr`, newest first (tests and diagnosis; at most
/// 100).
pub async fn merge_results(db: &Db, mr: MergeRequestId) -> Result<Vec<MergeResultRow>, ForgeError> {
    const QUERY: &str = "review.merge_results";
    let rows: Vec<(String, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT head_sha, target_sha, result_sha FROM forge.merge_result_refs \
                 WHERE merge_request_id = $1 ORDER BY id DESC LIMIT 100",
            )
            .bind(mr.get())
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(h, t, r)| {
            Ok(MergeResultRow {
                head: oid(mr, "head_sha", &h)?,
                target: oid(mr, "target_sha", &t)?,
                result: oid(mr, "result_sha", &r)?,
            })
        })
        .collect()
}

/// Store `result` as the merge of `head` into `target` for `mr` and, when
/// the row is new, enqueue the merge request pipeline on it (module docs).
/// `Ok(None)`: the merge request is gone (purged).
pub async fn record_merge_result(
    db: &Db,
    origin: &RequestId,
    mr: MergeRequestId,
    (head, target): (&ObjectId, &ObjectId),
    result: &ObjectId,
) -> Result<Option<RecordedResult>, ForgeError> {
    const QUERY: &str = "review.merge_result_record";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let subject: Option<(i64, String)> = sqlx::query_as(
            "SELECT project_id, source_branch FROM forge.merge_requests WHERE id = $1",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((project, source)) = subject else {
            return Ok(None);
        };
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO forge.merge_result_refs \
             (merge_request_id, project_id, head_sha, target_sha, result_sha) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (merge_request_id, head_sha, target_sha) DO NOTHING \
             RETURNING result_sha",
        )
        .bind(mr.get())
        .bind(project)
        .bind(head.as_str())
        .bind(target.as_str())
        .bind(result.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let recorded = match inserted {
            Some(_) => {
                let enqueued = enqueue_pipeline(
                    &mut tx,
                    (ProjectId::new(project), mr),
                    &source,
                    head,
                    result,
                    origin,
                )
                .await?;
                RecordedResult {
                    result: result.clone(), // clone: the answer owns its id
                    created: true,
                    pipeline_enqueued: enqueued,
                }
            }
            None => {
                let existing: String = sqlx::query_scalar(
                    "SELECT result_sha FROM forge.merge_result_refs \
                     WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3",
                )
                .bind(mr.get())
                .bind(head.as_str())
                .bind(target.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                RecordedResult {
                    result: oid(mr, "result_sha", &existing)?,
                    created: false,
                    pipeline_enqueued: false,
                }
            }
        };
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(recorded))
    })
    .await
}

/// Point the row of (`head`, `target`) of `mr` from `old` to `result` —
/// `old`'s commit is gone (`gc` pruned it once no ref kept it; the job
/// wrote `result` afresh) — and, when it moved, enqueue the merge request
/// pipeline on `result` in the same transaction (the runs on `old` belong
/// to a commit nobody can fetch). A racing job that replaced it first wins:
/// its result comes back (`created: false`). `Ok(None)`: the row or the
/// merge request is gone.
pub async fn replace_merge_result(
    db: &Db,
    origin: &RequestId,
    mr: MergeRequestId,
    (head, target): (&ObjectId, &ObjectId),
    (old, result): (&ObjectId, &ObjectId),
) -> Result<Option<RecordedResult>, ForgeError> {
    const QUERY: &str = "review.merge_result_replace";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let subject: Option<(i64, String)> = sqlx::query_as(
            "SELECT project_id, source_branch FROM forge.merge_requests WHERE id = $1",
        )
        .bind(mr.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((project, source)) = subject else {
            return Ok(None);
        };
        let moved: Option<i64> = sqlx::query_scalar(
            "UPDATE forge.merge_result_refs SET result_sha = $5              WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3              AND result_sha = $4 RETURNING id",
        )
        .bind(mr.get())
        .bind(head.as_str())
        .bind(target.as_str())
        .bind(old.as_str())
        .bind(result.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let recorded = if moved.is_some() {
            let enqueued = enqueue_pipeline(
                &mut tx,
                (ProjectId::new(project), mr),
                &source,
                head,
                result,
                origin,
            )
            .await?;
            RecordedResult {
                result: result.clone(), // clone: the answer owns its id
                created: true,
                pipeline_enqueued: enqueued,
            }
        } else {
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT result_sha FROM forge.merge_result_refs                  WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3",
            )
            .bind(mr.get())
            .bind(head.as_str())
            .bind(target.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
            let Some(existing) = existing else {
                return Ok(None);
            };
            RecordedResult {
                result: oid(mr, "result_sha", &existing)?,
                created: false,
                pipeline_enqueued: false,
            }
        };
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(Some(recorded))
    })
    .await
}

/// The `ci.pipeline_create` job of the merge result (deduplicated on the
/// pipeline's identity); `true` when it was enqueued.
async fn enqueue_pipeline(
    conn: &mut PgConnection,
    (project, mr): (ProjectId, MergeRequestId),
    source: &str,
    head: &ObjectId,
    result: &ObjectId,
    origin: &RequestId,
) -> Result<bool, ForgeError> {
    let git_ref = RefName::parse(source).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_branch_invalid")
            .with_context("merge_request_id", mr.get())
    })?;
    // The pusher of the head's latest version (none for an import).
    let pushed_by: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT pushed_by FROM forge.merge_request_versions \
         WHERE merge_request_id = $1 AND head_sha = $2 ORDER BY version_no DESC LIMIT 1",
    )
    .bind(mr.get())
    .bind(head.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("review.merge_result_pusher", e))?;
    let request = PipelineRequest {
        project,
        git_ref,
        sha: result.clone(), // clone: the request owns its id
        source: Source::MergeRequest,
        merge_request: Some(mr),
        triggered_by: pushed_by.flatten().filter(|id| *id > 0).map(UserId::new),
    };
    let enqueued = enqueue_on(conn, &create_job(&request, origin.clone(), None)?).await?; // clone: the job owns its id
    Ok(matches!(enqueued, crate::jobs::Enqueued::Queued(_)))
}

type ResultPair = (String, String);

const RESULTS_SQL: &str = "SELECT target_sha, result_sha FROM forge.merge_result_refs \
     WHERE merge_request_id = $1 AND head_sha = $2 ORDER BY id DESC LIMIT $3";

fn read_limit() -> i64 {
    i64::try_from(MAX_MERGE_RESULTS_READ).unwrap_or(i64::MAX)
}

fn pairs(
    mr: MergeRequestId,
    rows: Vec<ResultPair>,
) -> Result<Vec<(ObjectId, ObjectId)>, ForgeError> {
    rows.into_iter()
        .map(|(t, r)| Ok((oid(mr, "target_sha", &t)?, oid(mr, "result_sha", &r)?)))
        .collect()
}

/// The runs a merge decision reads for `head` of `mr`, inside a
/// transaction (the merge gate, under the merge request's locks).
pub(crate) async fn merge_runs_on(
    conn: &mut PgConnection,
    project: ProjectId,
    mr: MergeRequestId,
    head: &ObjectId,
) -> Result<MergeCheckRuns, ForgeError> {
    let rows: Vec<ResultPair> = sqlx::query_as(RESULTS_SQL)
        .bind(mr.get())
        .bind(head.as_str())
        .bind(read_limit())
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("review.merge_results_on", e))?;
    let head_runs = runs_on(&mut *conn, project, head).await?;
    let mut results = Vec::new();
    for (target, result) in pairs(mr, rows)? {
        let runs = runs_on(&mut *conn, project, &result).await?;
        results.push(MergeResultRuns {
            target,
            result,
            runs,
        });
    }
    Ok(MergeCheckRuns {
        head: head_runs,
        results,
    })
}

/// [`merge_runs_on`] on the read pool (the merge status).
pub async fn merge_runs_for(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
    head: &ObjectId,
) -> Result<MergeCheckRuns, ForgeError> {
    const QUERY: &str = "review.merge_results_read";
    let rows: Vec<ResultPair> = db
        .within(QUERY, async {
            sqlx::query_as(RESULTS_SQL)
                .bind(mr.get())
                .bind(head.as_str())
                .bind(read_limit())
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let head_runs = runs_for(db, project, head).await?;
    let mut results = Vec::new();
    for (target, result) in pairs(mr, rows)? {
        let runs = runs_for(db, project, &result).await?;
        results.push(MergeResultRuns {
            target,
            result,
            runs,
        });
    }
    Ok(MergeCheckRuns {
        head: head_runs,
        results,
    })
}
