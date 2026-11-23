//! The merge results of merge requests (M4 C7, plan 04 M-1 / M-2, ADR-0024
//! Amendment 2026-11-11; migration 20261207100000): the derived table
//! `forge.merge_result_refs`, one row per (merge request, head, target tip)
//! the mergeability job found clean, with the merge commit it wrote
//! (`refs/merge-requests/<iid>/merge`; the git work is forge-review's
//! `mergeability`).
//!
//! - [`merge_result_of`]: the result already written for a pair — the job
//!   reuses it and writes no second commit (once per pair).
//! - [`record_merge_result`]: the row, and in the same transaction the
//!   merge request pipeline on the result (`ci.pipeline_create`, source
//!   `merge_request`, the source branch as its ref, the head's pusher as
//!   `triggered_by`) as [`MergeRequestPipeline`] says — plan 18 S5: by
//!   default only while the head has no merge request pipeline yet (a new
//!   head, or one whose first pipeline's create failed for good); a row
//!   written because the target moved (same head) gets none unless
//!   `ci.mr_pipeline_on_target_move` is on or a person asked for it. Two
//!   jobs racing on one pair: `merge_result_refs_once` keeps the first row
//!   and the loser gets its result back (`created: false`).
//! - [`replace_merge_result`]: a stored result whose commit `gc` pruned is
//!   written again by the job; the row points at the new commit and the
//!   pipeline is asked for on it (whatever [`MergeRequestPipeline`] says:
//!   the runs on the pruned commit name a commit nobody can fetch, and
//!   this happens only after `gc`, never per landing).
//! - [`merge_results_to_prune`] / [`prune_merge_results`]: a closed or
//!   merged merge request's rows are removed (with its merge ref, by the
//!   worker's sweep) except those a live pipeline still needs.
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
use crate::pipelines::{create_dedup_key, create_job, PipelineRequest, Source};
use crate::{query_error, read_query_error, Db};

/// One stored merge result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResultRow {
    pub head: ObjectId,
    pub target: ObjectId,
    pub result: ObjectId,
}

/// When [`record_merge_result`] enqueues the merge request pipeline (plan
/// 18 S5, ADR-0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeRequestPipeline {
    /// The default: only when the new row is the head's first merge result
    /// (a new head). A row written because the target moved gets none —
    /// the head's `ci/pipeline` stays stale, the merge queue's train proves
    /// the merge, and a person can ask for it ([`Self::Requested`]).
    OnNewHead,
    /// `ci.mr_pipeline_on_target_move = true`: every new row (the
    /// behaviour before S5).
    OnTargetMove,
    /// A person asked for the pipeline on the current target
    /// (`pipeline.run_merge_request`): enqueued even when the row already
    /// exists (the pipeline's job is deduplicated on its identity).
    Requested,
    /// Plan 18 S6 (ADR-0037 (f)): the merged tree has no `.forge-ci.yml`
    /// (forge-review looked) — the pipeline job would write nothing, so
    /// none is enqueued. Never used for a person's ask.
    NoDefinition,
}

impl MergeRequestPipeline {
    /// The word of the log line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnNewHead => "on_new_head",
            Self::OnTargetMove => "on_target_move",
            Self::Requested => "requested",
            Self::NoDefinition => "no_definition",
        }
    }
}

/// What [`record_merge_result`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedResult {
    /// The pair's merge result (the caller's, or the one a racing job
    /// stored first).
    pub result: ObjectId,
    /// The row is new, or its pruned result was replaced.
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

/// Store `result` as the merge of `head` into `target` for `mr` and
/// enqueue the merge request pipeline on it as `pipeline` says (module
/// docs). `Ok(None)`: the merge request is gone (purged).
pub async fn record_merge_result(
    db: &Db,
    origin: &RequestId,
    mr: MergeRequestId,
    (head, target): (&ObjectId, &ObjectId),
    result: &ObjectId,
    pipeline: MergeRequestPipeline,
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
        let git_ref = stored_ref(mr, &source)?;
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
                let wanted = match pipeline {
                    MergeRequestPipeline::OnTargetMove | MergeRequestPipeline::Requested => true,
                    MergeRequestPipeline::NoDefinition => false,
                    MergeRequestPipeline::OnNewHead => {
                        !head_has_pipeline(&mut tx, (ProjectId::new(project), mr), &git_ref, head)
                            .await?
                    }
                };
                let enqueued = if wanted {
                    enqueue_pipeline(
                        &mut tx,
                        (ProjectId::new(project), mr),
                        &source,
                        head,
                        result,
                        origin,
                    )
                    .await?
                } else {
                    false
                };
                if pipeline == MergeRequestPipeline::Requested {
                    consume_ask(&mut tx, mr).await?;
                }
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
                let existing = oid(mr, "result_sha", &existing)?;
                // A person's ask is for the pipeline on this pair, whoever
                // wrote the row (deduplicated when it was asked for).
                let enqueued = if pipeline == MergeRequestPipeline::Requested {
                    enqueue_pipeline(
                        &mut tx,
                        (ProjectId::new(project), mr),
                        &source,
                        head,
                        &existing,
                        origin,
                    )
                    .await?
                } else {
                    false
                };
                if pipeline == MergeRequestPipeline::Requested {
                    consume_ask(&mut tx, mr).await?;
                }
                RecordedResult {
                    result: existing,
                    created: false,
                    pipeline_enqueued: enqueued,
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
            "UPDATE forge.merge_result_refs SET result_sha = $5 \
             WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3 \
             AND result_sha = $4 RETURNING id",
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
                "SELECT result_sha FROM forge.merge_result_refs \
                 WHERE merge_request_id = $1 AND head_sha = $2 AND target_sha = $3",
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

/// The merge results of a head [`head_has_pipeline`] looks at, at most
/// (newest first): one per target tip the head was merged into.
const HEAD_RESULTS_READ: i64 = 100;

/// Whether `head` of `mr` already has its merge request pipeline (plan 18
/// S5): a pipeline row on one of the head's merge results that is not
/// `error`, or a `ci.pipeline_create` job for one of them that is not dead
/// (queued, running, or succeeded — a succeeded job with no row found no
/// `.forge-ci.yml`). Otherwise — none written yet (a new head), or the first
/// one's create failed for good (a dead job, an `error` row) — the new merge
/// result gets one. A head that comes back after a force push (A → B → A)
/// is answered by its own earlier pipeline, whatever target it ran on.
///
/// Served by `merge_result_refs_once` (the head's rows), `ci_pipelines_once`
/// (project, source, ref, sha, merge request) and `jobs_dedup` (kind,
/// dedup key: the pipeline's identity, `pipelines::create_dedup_key`).
async fn head_has_pipeline(
    conn: &mut PgConnection,
    (project, mr): (ProjectId, MergeRequestId),
    git_ref: &RefName,
    head: &ObjectId,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "review.merge_result_head_pipeline";
    let results: Vec<String> = sqlx::query_scalar(
        "SELECT result_sha FROM forge.merge_result_refs \
         WHERE merge_request_id = $1 AND head_sha = $2 ORDER BY id DESC LIMIT $3",
    )
    .bind(mr.get())
    .bind(head.as_str())
    .bind(HEAD_RESULTS_READ)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let keys = results
        .iter()
        .map(|sha| {
            Ok(create_dedup_key(&PipelineRequest {
                project,
                git_ref: git_ref.clone(), // clone: the request owns it
                sha: oid(mr, "result_sha", sha)?,
                source: Source::MergeRequest,
                merge_request: Some(mr),
                triggered_by: None,
            }))
        })
        .collect::<Result<Vec<String>, ForgeError>>()?;
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.ci_pipelines p WHERE p.project_id = $1 \
         AND p.source = 'merge_request' AND p.ref = $2 AND p.sha = ANY($3) \
         AND coalesce(p.merge_request_id, 0) = $4 AND p.status <> 'error') \
         OR EXISTS (SELECT 1 FROM forge.jobs j WHERE j.kind = 'ci.pipeline_create' \
         AND j.dedup_key = ANY($5) AND j.state <> 'dead')",
    )
    .bind(project.get())
    .bind(git_ref.as_str())
    .bind(&results)
    .bind(mr.get())
    .bind(&keys)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error(QUERY, e))
}

/// Plan 18 S5 review: a person's ask is consumed by the first merge result
/// written for it — the ask is taken off the merge request's live asked
/// mergeability jobs (in the same transaction), so a later request does not
/// inherit it and one press makes one pipeline.
async fn consume_ask(conn: &mut PgConnection, mr: MergeRequestId) -> Result<u64, ForgeError> {
    sqlx::query(
        "UPDATE forge.jobs SET payload = payload - 'pipeline' \
         WHERE kind = 'merge_request.mergeability' AND state IN ('queued', 'running') \
         AND payload ->> 'merge_request_id' = $1 AND payload ->> 'pipeline' = 'true'",
    )
    .bind(mr.get().to_string())
    .execute(conn)
    .await
    .map(|done| done.rows_affected())
    .map_err(|e| query_error("review.merge_result_consume_ask", e))
}

/// The stored source branch of `mr` as a ref.
fn stored_ref(mr: MergeRequestId, source: &str) -> Result<RefName, ForgeError> {
    RefName::parse(source).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_branch_invalid")
            .with_context("merge_request_id", mr.get())
    })
}

/// The `ci.pipeline_create` job of the merge result (deduplicated on the
/// pipeline's identity); `true` when it was enqueued.
pub(crate) async fn enqueue_pipeline(
    conn: &mut PgConnection,
    (project, mr): (ProjectId, MergeRequestId),
    source: &str,
    head: &ObjectId,
    result: &ObjectId,
    origin: &RequestId,
) -> Result<bool, ForgeError> {
    let git_ref = stored_ref(mr, source)?;
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

/// A merge result row a live pipeline still needs (`r` is the row): a
/// merge request pipeline on its commit pending or running, or its
/// `ci.pipeline_create` job not done yet. The pipeline's own ref
/// (`refs/pipelines/<id>/head`) pins the commit; the row stays until it
/// ends.
const PINNED: &str = "(EXISTS (SELECT 1 FROM forge.ci_pipelines p \
     WHERE p.merge_request_id = r.merge_request_id AND p.sha = r.result_sha \
     AND p.status IN ('pending', 'running')) \
     OR EXISTS (SELECT 1 FROM forge.jobs j WHERE j.kind = 'ci.pipeline_create' \
     AND j.state IN ('queued', 'running') \
     AND j.payload ->> 'merge_request_id' = r.merge_request_id::text \
     AND j.payload ->> 'sha' = r.result_sha))";

/// A closed or merged merge request with merge results to prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneCandidate {
    pub merge_request: MergeRequestId,
    pub repository: forge_core::RepoId,
    pub iid: i64,
}

/// At most `limit` merge requests that are no longer open and hold a merge
/// result row no live pipeline needs (review LOW, M4 C7): the worker's
/// sweep deletes their `refs/merge-requests/<iid>/merge` and those rows
/// ([`prune_merge_results`]). Ready repositories only.
pub async fn merge_results_to_prune(
    db: &Db,
    limit: i64,
) -> Result<Vec<PruneCandidate>, ForgeError> {
    const QUERY: &str = "review.merge_results_to_prune";
    let rows: Vec<(i64, i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT m.id, g.id, m.iid FROM forge.merge_requests m \
                 JOIN forge.repositories g ON g.project_id = m.project_id AND g.state = 'ready' \
                 WHERE m.state <> 'open' AND EXISTS (SELECT 1 FROM forge.merge_result_refs r \
                 WHERE r.merge_request_id = m.id AND NOT {PINNED}) \
                 ORDER BY m.id LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(mr, repo, iid)| PruneCandidate {
            merge_request: MergeRequestId::new(mr),
            repository: forge_core::RepoId::new(repo),
            iid,
        })
        .collect())
}

/// Remove the merge result rows of `mr` no live pipeline needs, while `mr`
/// is not open (a reopened one keeps them: its next check reuses them);
/// the count removed. The caller holds the merge request's refs lock and
/// removed the merge ref first.
pub async fn prune_merge_results(db: &Db, mr: MergeRequestId) -> Result<u64, ForgeError> {
    const QUERY: &str = "review.merge_results_prune";
    db.within(QUERY, async {
        sqlx::query(&format!(
            "DELETE FROM forge.merge_result_refs r USING forge.merge_requests m \
             WHERE r.merge_request_id = $1 AND m.id = r.merge_request_id \
             AND m.state <> 'open' AND NOT {PINNED}"
        ))
        .bind(mr.get())
        .execute(db.write_pool())
        .await
        .map(|done| done.rows_affected())
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Whether `mr` is open now (`None`: gone) — read under the refs lock
/// before the prune removes the merge ref.
pub async fn merge_request_open(db: &Db, mr: MergeRequestId) -> Result<Option<bool>, ForgeError> {
    const QUERY: &str = "review.merge_request_open";
    db.within(QUERY, async {
        sqlx::query_scalar("SELECT state = 'open' FROM forge.merge_requests WHERE id = $1")
            .bind(mr.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
    })
    .await
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
