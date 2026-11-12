//! CI artifacts (M4 C6, plan 14 §4.1, §9 #6; migration 20261211200000):
//! the rows of `forge.ci_artifacts`. The bytes live under the storage root
//! (forge-pipelines `artifacts`); this module keeps their metadata.
//!
//! - [`upload_target`] / [`record`]: a job's upload, on the job's own
//!   token — only while the job runs (its lease holds, the attempt is the
//!   current one), within the attempt's bounds (files, bytes), one row per
//!   path.
//! - [`list_decided`] / [`one_decided`]: a person's or an agent's read on
//!   a route's proof; `decide` (forge-auth `authorize_ci_output`) runs on
//!   the pipeline's mark before any row is returned.
//! - [`needs_of`] / [`need_one`]: what a job may read of the jobs it
//!   `needs` in its own pipeline (their current attempt, succeeded).
//! - [`due`] / [`forget`]: the worker's expiry sweep (not `keep`).

use forge_core::ci_artifact::{MAX_JOB_BYTES, MAX_JOB_FILES, RETENTION_DAYS};
use forge_core::ProjectId;
use forge_errors::{ErrorCode, ForgeError};

use crate::pipelines::CopyMark;
use crate::{query_error, read_query_error, Db};

const TS: &str = "YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"";

/// The most rows one read or one sweep returns.
pub const MAX_LISTED: i64 = 1000;

/// One artifact as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    pub id: i64,
    pub project_id: i64,
    pub pipeline_id: i64,
    pub job_id: i64,
    /// The name of the job that uploaded it.
    pub job_name: String,
    pub attempt: i32,
    pub path: String,
    pub size_bytes: i64,
    /// Lower-case hex.
    pub sha256: String,
    pub keep: bool,
    pub created_at: String,
    pub expires_at: String,
}

type ArtifactRow = (
    i64,
    i64,
    i64,
    i64,
    String,
    i32,
    String,
    i64,
    Vec<u8>,
    bool,
    String,
    String,
);

fn columns() -> String {
    format!(
        "a.id, a.project_id, a.pipeline_id, a.job_id, j.name, a.attempt, a.path, a.size_bytes, \
         a.sha256, a.keep, to_char(a.created_at AT TIME ZONE 'UTC', '{TS}'), \
         to_char(a.expires_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

fn artifact_of(r: ArtifactRow) -> StoredArtifact {
    StoredArtifact {
        id: r.0,
        project_id: r.1,
        pipeline_id: r.2,
        job_id: r.3,
        job_name: r.4,
        attempt: r.5,
        path: r.6,
        size_bytes: r.7,
        sha256: r.8.iter().map(|b| format!("{b:02x}")).collect(),
        keep: r.9,
        created_at: r.10,
        expires_at: r.11,
    }
}

/// What an upload of `job` is checked against before its bytes are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTarget {
    pub project: ProjectId,
    pub pipeline_id: i64,
    pub job_id: i64,
    pub attempt: i32,
    /// Bytes the attempt (and the project's quota) may still take.
    pub bytes_left: u64,
}

fn refused(reason: &'static str, job: i64) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", reason)
        .with_context("job_id", job)
}

/// The attempt's and the project's use.
struct Use {
    files: i64,
    bytes: i64,
    project_bytes: i64,
}

/// `FG-CI-0019` / `FG-CI-0022` when one more file of `size` bytes would
/// pass a bound; the bytes still free after it otherwise.
fn left_after(job: i64, project: i64, u: &Use, size: u64, quota: u64) -> Result<u64, ForgeError> {
    let used = u64::try_from(u.bytes).unwrap_or(u64::MAX);
    let project_used = u64::try_from(u.project_bytes).unwrap_or(u64::MAX);
    if u.files >= MAX_JOB_FILES || used.saturating_add(size) > MAX_JOB_BYTES {
        return Err(ForgeError::new(ErrorCode::ArtifactTooLarge)
            .with_context("reason", "job_limit")
            .with_context("job_id", job)
            .with_context("files", u.files)
            .with_context("stored_bytes", u.bytes)
            .with_context("limit_bytes", MAX_JOB_BYTES));
    }
    if project_used.saturating_add(size) > quota {
        return Err(ForgeError::new(ErrorCode::ArtifactQuotaExceeded)
            .with_context("job_id", job)
            .with_context("project_id", project)
            .with_context("stored_bytes", u.project_bytes)
            .with_context("quota_bytes", quota));
    }
    Ok((MAX_JOB_BYTES - used - size).min(quota - project_used - size))
}

/// The attempt's files and bytes, the project's bytes, and whether `path`
/// is stored ($1 job, $2 attempt, $3 project, $4 path).
const USE_SQL: &str = "SELECT \
       (SELECT count(*) FROM forge.ci_artifacts a WHERE a.job_id = $1 AND a.attempt = $2), \
       (SELECT coalesce(sum(size_bytes), 0)::bigint FROM forge.ci_artifacts a \
          WHERE a.job_id = $1 AND a.attempt = $2), \
       (SELECT coalesce(sum(size_bytes), 0)::bigint FROM forge.ci_artifacts a \
          WHERE a.project_id = $3), \
       EXISTS (SELECT 1 FROM forge.ci_artifacts a \
          WHERE a.job_id = $1 AND a.attempt = $2 AND a.path = $4)";

/// The running attempt of `job` that may upload `path` now: `None` when the
/// job is not running (ended, not claimed: the token was revoked with it
/// anyway). Errors: `FG-CI-0019` past the attempt's files or bytes,
/// `FG-CI-0022` past the project's `quota`, `FG-WEB-0006
/// reason=artifact_exists` for a path stored already. A first look: the
/// bounds are checked again, atomically, by [`record`].
pub async fn upload_target(
    db: &Db,
    job: i64,
    path: &str,
    quota: u64,
) -> Result<Option<UploadTarget>, ForgeError> {
    const QUERY: &str = "ci_artifacts.upload_target";
    let row: Option<(i64, i64, i32)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT project_id, pipeline_id, attempt FROM forge.ci_jobs \
                 WHERE id = $1 AND status = 'running'",
            )
            .bind(job)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let Some((project, pipeline_id, attempt)) = row else {
        return Ok(None);
    };
    let (files, bytes, project_bytes, exists): (i64, i64, i64, bool) = db
        .within(QUERY, async {
            sqlx::query_as(USE_SQL)
                .bind(job)
                .bind(attempt)
                .bind(project)
                .bind(path)
                .fetch_one(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    if exists {
        return Err(refused("artifact_exists", job));
    }
    let u = Use {
        files,
        bytes,
        project_bytes,
    };
    let bytes_left = left_after(job, project, &u, 0, quota)?;
    Ok(Some(UploadTarget {
        project: ProjectId::new(project),
        pipeline_id,
        job_id: job,
        attempt,
        bytes_left,
    }))
}

/// Record a stored file of `target` (its bytes are written, not yet in
/// place); its id. One transaction with the job row locked and the
/// project's artifact lock held (M4 C6 review: parallel uploads cannot pass
/// a bound together): the job must still run the same attempt
/// (`FG-CI-0004 reason=not_recorded` otherwise), the path be new
/// (`artifact_exists`), and the attempt's bounds (`FG-CI-0019`) and the
/// project's `quota` (`FG-CI-0022`) hold with this file. The caller removes
/// the file on any error.
pub async fn record(
    db: &Db,
    target: &UploadTarget,
    (path, size, sha256): (&str, u64, &[u8; 32]),
    quota: u64,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "ci_artifacts.record";
    let stored = i64::try_from(size).map_err(|_| refused("artifact_too_large", target.job_id))?;
    let job = target.job_id;
    let project = target.project.get();
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let running: Option<i32> = sqlx::query_scalar(
            "SELECT attempt FROM forge.ci_jobs WHERE id = $1 AND attempt = $2 \
               AND status = 'running' FOR UPDATE",
        )
        .bind(job)
        .bind(target.attempt)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        if running.is_none() {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(ForgeError::new(ErrorCode::JobLeaseLost)
                .with_context("reason", "not_recorded")
                .with_context("job_id", job)
                .with_context("attempt", target.attempt)
                .with_context("operation", "artifact"));
        }
        // The project's uploads are recorded one at a time (the quota).
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('ci_artifacts:' || $1::text, 0))",
        )
        .bind(project)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let (files, bytes, project_bytes, exists): (i64, i64, i64, bool) = sqlx::query_as(USE_SQL)
            .bind(job)
            .bind(target.attempt)
            .bind(project)
            .bind(path)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let u = Use {
            files,
            bytes,
            project_bytes,
        };
        let checked = if exists {
            Err(refused("artifact_exists", job))
        } else {
            left_after(job, project, &u, size, quota).map(drop)
        };
        if let Err(e) = checked {
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Err(e);
        }
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, \
               path, size_bytes, sha256, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8)) RETURNING id",
        )
        .bind(project)
        .bind(target.pipeline_id)
        .bind(job)
        .bind(target.attempt)
        .bind(path)
        .bind(stored)
        .bind(sha256.as_slice())
        .bind(i32::try_from(RETENTION_DAYS).unwrap_or(30))
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(id)
    })
    .await
}

/// Remove the row `id` whose file could not be put in place.
pub async fn discard(db: &Db, id: i64) -> Result<(), ForgeError> {
    const QUERY: &str = "ci_artifacts.discard";
    db.within(QUERY, async {
        sqlx::query("DELETE FROM forge.ci_artifacts WHERE id = $1")
            .bind(id)
            .execute(db.write_pool())
            .await
            .map(drop)
            .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// The pipeline's mark (what a read is decided on) and its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactTarget {
    pub pipeline_id: i64,
    pub job_id: i64,
    pub copy_mark: CopyMark,
    /// The job was given secrets (M4 C6 review): its artifacts are
    /// Maintainers' (people) only.
    pub secrets_released: bool,
}

async fn target_of(
    db: &Db,
    project: ProjectId,
    (pipeline, job): (i64, i64),
) -> Result<Option<ArtifactTarget>, ForgeError> {
    const QUERY: &str = "ci_artifacts.target";
    let row: Option<(String, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.copy_mark, j.secrets_released FROM forge.ci_jobs j \
                 JOIN forge.ci_pipelines p ON p.id = j.pipeline_id \
                 WHERE j.id = $1 AND j.pipeline_id = $2 AND p.project_id = $3",
            )
            .bind(job)
            .bind(pipeline)
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((mark, secrets_released)) = row else {
        return Ok(None);
    };
    Ok(Some(ArtifactTarget {
        pipeline_id: pipeline,
        job_id: job,
        secrets_released,
        copy_mark: CopyMark::parse(&mark).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_mark_invalid")
                .with_context("pipeline_id", pipeline)
        })?,
    }))
}

/// The artifacts of a job of `project`'s pipeline, decided first (`decide`
/// on the pipeline's mark: no row is read before it). `None`: no such job.
pub async fn list_decided<P>(
    db: &Db,
    project: ProjectId,
    ids: (i64, i64),
    decide: impl FnOnce(&ArtifactTarget) -> Result<P, ForgeError>,
) -> Result<Option<(Vec<StoredArtifact>, P)>, ForgeError> {
    const QUERY: &str = "ci_artifacts.list";
    let Some(target) = target_of(db, project, ids).await? else {
        return Ok(None);
    };
    let allowed = decide(&target)?;
    let rows: Vec<ArtifactRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_artifacts a JOIN forge.ci_jobs j ON j.id = a.job_id \
                 WHERE a.job_id = $1 AND a.project_id = $2 ORDER BY a.attempt DESC, a.path \
                 LIMIT $3",
                columns()
            ))
            .bind(target.job_id)
            .bind(project.get())
            .bind(MAX_LISTED)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(Some((rows.into_iter().map(artifact_of).collect(), allowed)))
}

/// One artifact of a job of `project`'s pipeline, decided first as
/// [`list_decided`]. `None`: no such job or artifact.
pub async fn one_decided<P>(
    db: &Db,
    project: ProjectId,
    ids: (i64, i64),
    artifact: i64,
    decide: impl FnOnce(&ArtifactTarget) -> Result<P, ForgeError>,
) -> Result<Option<(StoredArtifact, P)>, ForgeError> {
    const QUERY: &str = "ci_artifacts.one";
    let Some(target) = target_of(db, project, ids).await? else {
        return Ok(None);
    };
    let allowed = decide(&target)?;
    let row: Option<ArtifactRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_artifacts a JOIN forge.ci_jobs j ON j.id = a.job_id \
                 WHERE a.id = $1 AND a.job_id = $2 AND a.project_id = $3",
                columns()
            ))
            .bind(artifact)
            .bind(target.job_id)
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|r| (artifact_of(r), allowed)))
}

/// What `job` (running) may read of the jobs it `needs`: their current
/// attempt's artifacts, when they succeeded, in its own pipeline — and an
/// artifact of a job that was given secrets only when that pipeline is a
/// protected push (M4 C6 review; release already requires it, this says it
/// again in the read). More than [`MAX_LISTED`] files is `FG-CI-0019
/// reason=too_many_needed_files` (never a silently shortened list).
pub async fn needs_of(db: &Db, job: i64) -> Result<Vec<StoredArtifact>, ForgeError> {
    const QUERY: &str = "ci_artifacts.needs_of";
    let mut rows: Vec<ArtifactRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_jobs me \
                 JOIN forge.ci_jobs j ON j.pipeline_id = me.pipeline_id \
                   AND j.name = ANY (me.needs) AND j.status = 'succeeded' \
                 JOIN forge.ci_artifacts a ON a.job_id = j.id AND a.attempt = j.attempt \
                 JOIN forge.ci_pipelines p ON p.id = me.pipeline_id \
                 WHERE me.id = $1 AND me.status = 'running' \
                   AND (NOT j.secrets_released OR (p.source = 'push' AND p.protected)) \
                 ORDER BY j.name, a.path LIMIT $2",
                columns()
            ))
            .bind(job)
            .bind(MAX_LISTED + 1)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    if i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_LISTED {
        rows.clear();
        return Err(ForgeError::new(ErrorCode::ArtifactTooLarge)
            .with_context("reason", "too_many_needed_files")
            .with_context("job_id", job)
            .with_context("limit_files", MAX_LISTED));
    }
    Ok(rows.into_iter().map(artifact_of).collect())
}

/// One artifact of [`needs_of`] (`None` otherwise: another pipeline's, a
/// job not needed, an earlier attempt).
pub async fn need_one(
    db: &Db,
    job: i64,
    artifact: i64,
) -> Result<Option<StoredArtifact>, ForgeError> {
    Ok(needs_of(db, job)
        .await?
        .into_iter()
        .find(|a| a.id == artifact))
}

/// Which of `ids` are projects that still exist (the purge deletes a
/// project's artifact rows; its directory is removed after it).
pub async fn known_projects(db: &Db, ids: &[i64]) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "ci_artifacts.known_projects";
    db.within(QUERY, async {
        sqlx::query_scalar("SELECT id FROM forge.projects WHERE id = ANY ($1)")
            .bind(ids)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// Expired artifacts (not `keep`), oldest first: `(id, project_id)`.
pub async fn due(db: &Db, limit: i64) -> Result<Vec<(i64, i64)>, ForgeError> {
    const QUERY: &str = "ci_artifacts.due";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT id, project_id FROM forge.ci_artifacts \
             WHERE NOT keep AND expires_at <= now() ORDER BY expires_at LIMIT $1",
        )
        .bind(limit)
        .fetch_all(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// Delete the row of an expired artifact whose file is gone (never a
/// `keep` row); whether one was deleted.
pub async fn forget(db: &Db, id: i64) -> Result<bool, ForgeError> {
    const QUERY: &str = "ci_artifacts.forget";
    db.within(QUERY, async {
        sqlx::query(
            "DELETE FROM forge.ci_artifacts WHERE id = $1 AND NOT keep AND expires_at <= now()",
        )
        .bind(id)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

#[cfg(test)]
mod tests;
