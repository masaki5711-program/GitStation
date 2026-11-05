//! Retention of check runs (feat/db-check-run-retention; ADR-0024 "実装で
//! 決めたこと（O3）" note, runbook `checks-reporting`).
//!
//! [`expire_old_runs`] deletes check runs whose `updated_at` is older than
//! the window and that are not the current head of an open merge request,
//! with their annotations, at most a batch per call. `check_runs` and
//! `check_annotations` are audited on the project, so a batch commits with
//! one `check_run.retention` audit row per project it touched (counts only;
//! the first is the change's own row, the others
//! [`ChangeRecord::affected`](crate::change::ChangeRecord)).
//!
//! Lock order, consistent with the report and the merge gate and never held
//! across batches (one short transaction per call):
//!
//! 1. the projects with due runs, `FOR SHARE SKIP LOCKED` in id order — the
//!    strength a report takes; a project the merge gate (`FOR UPDATE`) or a
//!    purge holds is skipped, so the retention never queues behind them
//!    (and a report queued behind the gate is never queued behind us);
//! 2. the due runs of those projects, `FOR UPDATE SKIP LOCKED`, oldest
//!    first, at most `batch`: a run a report is rewriting is skipped, never
//!    waited for (a report takes the project, then the commit's advisory
//!    lock, then its run row — we never take the advisory lock and never
//!    wait for a row, so no cycle is possible);
//! 3. the annotations of those runs, then the runs.
//!
//! A run a report refreshed after the first look is no longer due when
//! step 2 runs (read committed re-evaluation) or is skipped. A merge request
//! that makes an old commit its head right after step 2 finds its checks
//! missing; the merge gate then blocks on them (fail closed) until they are
//! reported again.

use std::collections::BTreeMap;

use forge_core::{AuditDetails, ProjectId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle, MAX_AFFECTED};
use crate::{query_error, read_query_error, Db};

/// The most projects one batch looks at.
const MAX_PROJECTS: i64 = 200;

/// The most runs one batch deletes.
pub const RETENTION_BATCH: u32 = 1000;

/// What one batch deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Expired {
    /// Check runs deleted.
    pub runs: i64,
    /// Annotations deleted with them.
    pub annotations: i64,
    /// Projects they belonged to (= `check_run.retention` rows written).
    pub projects: i64,
    /// Projects with due runs that the merge gate or a purge held, left for
    /// the next pass.
    pub skipped_projects: i64,
}

/// Due runs this build counts at most (a bounded read).
pub const BACKLOG_COUNT_LIMIT: i64 = 10_001;

/// Runs past the window plus a day of slack that the worker has not
/// deleted yet (doctor `checks.retention`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionBacklog {
    /// Due runs, at most [`BACKLOG_COUNT_LIMIT`].
    pub due: i64,
    /// Whole days since the oldest due run was updated.
    pub oldest_age_days: Option<i64>,
}

pub async fn retention_backlog(
    db: &Db,
    older_than_days: u32,
) -> Result<RetentionBacklog, ForgeError> {
    const QUERY: &str = "checks.retention_backlog";
    let days = i32::try_from(older_than_days)
        .ok()
        .and_then(|d| d.checked_add(1))
        .filter(|d| *d > 1)
        .ok_or_else(|| refused("retention_days_out_of_range"))?;
    let (due, oldest): (i64, Option<i32>) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*),                         extract(day FROM now() - min(updated_at))::int                    FROM (SELECT r.updated_at FROM forge.check_runs r                           WHERE r.updated_at < now() - make_interval(days => $1)                             AND NOT EXISTS (SELECT 1 FROM forge.merge_requests m                                  WHERE m.project_id = r.project_id AND m.state = 'open'                                    AND m.head_sha = r.head_sha)                           LIMIT $2) due",
            )
            .bind(days)
            .bind(BACKLOG_COUNT_LIMIT)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(RetentionBacklog {
        due,
        oldest_age_days: oldest.map(i64::from),
    })
}

/// The runs that are due: past the window, not the current head of an open
/// merge request. `$1` projects, `$2` days.
const DUE: &str = "r.project_id = ANY($1) \
     AND r.updated_at < now() - make_interval(days => $2) \
     AND NOT EXISTS (SELECT 1 FROM forge.merge_requests m \
                      WHERE m.project_id = r.project_id AND m.state = 'open' \
                        AND m.head_sha = r.head_sha)";

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("operation", "checks.retention")
        .with_context("reason", reason)
}

/// Delete one batch of due check runs (see the module docs). Nothing due
/// writes nothing (`Expired::default()`). A `batch` outside
/// `1 ..= RETENTION_BATCH` or a window of 0 days is `FG-SYS-0001`.
pub async fn expire_old_runs(
    db: &Db,
    ctx: &ChangeContext,
    older_than_days: u32,
    batch: u32,
) -> Result<Expired, ForgeError> {
    if batch == 0 || batch > RETENTION_BATCH {
        return Err(refused("retention_batch_out_of_range").with_context("batch", batch));
    }
    let days = i32::try_from(older_than_days)
        .ok()
        .filter(|d| *d > 0)
        .ok_or_else(|| refused("retention_days_out_of_range"))?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        // 1. The projects with due runs, the one with the oldest run first
        //    (at most MAX_PROJECTS), then lock them. Choosing projects, not
        //    runs, keeps a locked project's old runs from filling the batch
        //    and starving every other project.
        let candidates: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT project_id FROM (                        SELECT r.project_id, min(r.updated_at) AS oldest                          FROM forge.check_runs r                         WHERE r.updated_at < now() - make_interval(days => $1)                           AND NOT EXISTS (SELECT 1 FROM forge.merge_requests m                                WHERE m.project_id = r.project_id AND m.state = 'open'                                  AND m.head_sha = r.head_sha)                         GROUP BY r.project_id ORDER BY oldest, r.project_id LIMIT $2) due                      ORDER BY project_id",
                )
                .bind(days)
                .bind(MAX_PROJECTS)
                .fetch_all(c)
                .await
                .map_err(|e| query_error("checks.retention.candidates", e))
            })
            .await?;
        if candidates.is_empty() {
            return Ok(Settle::Discard(Expired::default()));
        }
        let candidate_count = i64::try_from(candidates.len()).unwrap_or(i64::MAX);
        let locked: Vec<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT id FROM forge.projects WHERE id = ANY($1) ORDER BY id \
                     FOR SHARE SKIP LOCKED",
                )
                .bind(candidates)
                .fetch_all(c)
                .await
                .map_err(|e| query_error("checks.retention.lock_projects", e))
            })
            .await?;
        let skipped_projects =
            candidate_count - i64::try_from(locked.len()).unwrap_or(i64::MAX);
        if locked.is_empty() {
            return Ok(Settle::Discard(Expired {
                skipped_projects,
                ..Expired::default()
            }));
        }
        // 2. The due runs of the locked projects, rows a report holds skipped.
        let victims: Vec<(i64, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(&format!(
                    "SELECT r.id, r.project_id FROM forge.check_runs r WHERE {DUE} \
                     ORDER BY r.updated_at, r.id LIMIT $3 FOR UPDATE OF r SKIP LOCKED"
                ))
                .bind(locked)
                .bind(days)
                .bind(i64::from(batch))
                .fetch_all(c)
                .await
                .map_err(|e| query_error("checks.retention.lock_runs", e))
            })
            .await?;
        if victims.is_empty() {
            return Ok(Settle::Discard(Expired {
                skipped_projects,
                ..Expired::default()
            }));
        }
        let ids: Vec<i64> = victims.iter().map(|(id, _)| *id).collect();
        // 3. Annotations first (the foreign key), then the runs.
        let annotated: Vec<i64> = {
            let ids = ids.clone(); // clone: the step owns its binds
            tx.step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.check_annotations WHERE check_run_id = ANY($1) \
                     RETURNING project_id",
                )
                .bind(ids)
                .fetch_all(c)
                .await
                .map_err(|e| query_error("checks.retention.delete_annotations", e))
            })
            .await?
        };
        let deleted: u64 = {
            let ids = ids.clone(); // clone: the step owns its binds
            tx.step(|c| async move {
                sqlx::query("DELETE FROM forge.check_runs WHERE id = ANY($1)")
                    .bind(ids)
                    .execute(c)
                    .await
                    .map(|r| r.rows_affected())
                    .map_err(|e| query_error("checks.retention.delete_runs", e))
            })
            .await?
        };
        if usize::try_from(deleted).ok() != Some(ids.len()) {
            return Err(refused("retention_deleted_count_differs")
                .with_context("expected", ids.len())
                .with_context("deleted", deleted));
        }
        let mut per_project: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
        for (_, project) in &victims {
            per_project.entry(*project).or_default().0 += 1;
        }
        for project in &annotated {
            per_project.entry(*project).or_default().1 += 1;
        }
        let mut rows = per_project.iter().map(|(project, (runs, annotations))| {
            let details = AuditDetails::CheckRunsExpired {
                project: ProjectId::new(*project),
                runs: *runs,
                annotations: *annotations,
                older_than_days,
            };
            let mut record = ChangeRecord::audit_only(&details, "project", *project);
            record.audit.project_id = Some(ProjectId::new(*project));
            record
        });
        let Some(mut record) = rows.next() else {
            return Ok(Settle::Discard(Expired::default()));
        };
        record.affected = rows.map(|r| r.audit).collect();
        if record.affected.len() > MAX_AFFECTED {
            // RETENTION_BATCH keeps this unreachable; never commit past the cap.
            return Err(refused("retention_too_many_projects")
                .with_context("projects", per_project.len()));
        }
        let expired = Expired {
            skipped_projects,
            runs: i64::try_from(ids.len()).unwrap_or(i64::MAX),
            annotations: i64::try_from(annotated.len()).unwrap_or(i64::MAX),
            projects: i64::try_from(per_project.len()).unwrap_or(i64::MAX),
        };
        Ok(Settle::Commit(record, expired))
    }
    .await;
    tx.settle("check_run.retention", outcome).await
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
