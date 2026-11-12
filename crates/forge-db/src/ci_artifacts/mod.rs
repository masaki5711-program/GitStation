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
    /// Bytes the attempt may still upload.
    pub bytes_left: u64,
}

fn refused(reason: &'static str, job: i64) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", reason)
        .with_context("job_id", job)
}

/// The running attempt of `job` that may upload `path` now: `None` when the
/// job is not running (ended, not claimed: the token was revoked with it
/// anyway). Errors: `FG-CI-0019` past the attempt's files or bytes,
/// `FG-WEB-0006 reason=artifact_exists` for a path stored already.
pub async fn upload_target(
    db: &Db,
    job: i64,
    path: &str,
) -> Result<Option<UploadTarget>, ForgeError> {
    const QUERY: &str = "ci_artifacts.upload_target";
    let row: Option<(i64, i64, i32, i64, i64, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT j.project_id, j.pipeline_id, j.attempt, \
                   (SELECT count(*) FROM forge.ci_artifacts a \
                      WHERE a.job_id = j.id AND a.attempt = j.attempt), \
                   (SELECT coalesce(sum(size_bytes), 0)::bigint FROM forge.ci_artifacts a \
                      WHERE a.job_id = j.id AND a.attempt = j.attempt), \
                   EXISTS (SELECT 1 FROM forge.ci_artifacts a \
                      WHERE a.job_id = j.id AND a.attempt = j.attempt AND a.path = $2) \
                 FROM forge.ci_jobs j WHERE j.id = $1 AND j.status = 'running'",
            )
            .bind(job)
            .bind(path)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let Some((project, pipeline_id, attempt, files, bytes, exists)) = row else {
        return Ok(None);
    };
    if exists {
        return Err(refused("artifact_exists", job));
    }
    let used = u64::try_from(bytes).unwrap_or(u64::MAX);
    if files >= MAX_JOB_FILES || used >= MAX_JOB_BYTES {
        return Err(ForgeError::new(ErrorCode::ArtifactTooLarge)
            .with_context("reason", "job_limit")
            .with_context("job_id", job)
            .with_context("files", files)
            .with_context("stored_bytes", bytes)
            .with_context("limit_bytes", MAX_JOB_BYTES));
    }
    Ok(Some(UploadTarget {
        project: ProjectId::new(project),
        pipeline_id,
        job_id: job,
        attempt,
        bytes_left: MAX_JOB_BYTES - used,
    }))
}

/// Record a stored file of `target` (its bytes are written, not yet in
/// place); its id. The job must still run the same attempt and the path be
/// new (`FG-CI-0004 reason=not_recorded` otherwise: the caller removes the
/// file).
pub async fn record(
    db: &Db,
    target: &UploadTarget,
    path: &str,
    size: u64,
    sha256: &[u8; 32],
) -> Result<i64, ForgeError> {
    const QUERY: &str = "ci_artifacts.record";
    let size = i64::try_from(size).map_err(|_| refused("artifact_too_large", target.job_id))?;
    let id: Option<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "INSERT INTO forge.ci_artifacts (project_id, pipeline_id, job_id, attempt, \
                   path, size_bytes, sha256, expires_at) \
                 SELECT j.project_id, j.pipeline_id, j.id, j.attempt, $2, $3, $4, \
                   now() + make_interval(days => $5) \
                 FROM forge.ci_jobs j WHERE j.id = $1 AND j.attempt = $6 AND j.status = 'running' \
                 ON CONFLICT (job_id, attempt, path) DO NOTHING RETURNING id",
            )
            .bind(target.job_id)
            .bind(path)
            .bind(size)
            .bind(sha256.as_slice())
            .bind(i32::try_from(RETENTION_DAYS).unwrap_or(30))
            .bind(target.attempt)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    id.ok_or_else(|| {
        ForgeError::new(ErrorCode::JobLeaseLost)
            .with_context("reason", "not_recorded")
            .with_context("job_id", target.job_id)
            .with_context("attempt", target.attempt)
            .with_context("operation", "artifact")
    })
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
}

async fn target_of(
    db: &Db,
    project: ProjectId,
    (pipeline, job): (i64, i64),
) -> Result<Option<ArtifactTarget>, ForgeError> {
    const QUERY: &str = "ci_artifacts.target";
    let mark: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT p.copy_mark FROM forge.ci_jobs j \
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
    let Some(mark) = mark else {
        return Ok(None);
    };
    Ok(Some(ArtifactTarget {
        pipeline_id: pipeline,
        job_id: job,
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
/// attempt's artifacts, when they succeeded, in its own pipeline.
pub async fn needs_of(db: &Db, job: i64) -> Result<Vec<StoredArtifact>, ForgeError> {
    const QUERY: &str = "ci_artifacts.needs_of";
    let rows: Vec<ArtifactRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ci_jobs me \
                 JOIN forge.ci_jobs j ON j.pipeline_id = me.pipeline_id \
                   AND j.name = ANY (me.needs) AND j.status = 'succeeded' \
                 JOIN forge.ci_artifacts a ON a.job_id = j.id AND a.attempt = j.attempt \
                 WHERE me.id = $1 AND me.status = 'running' ORDER BY j.name, a.path LIMIT $2",
                columns()
            ))
            .bind(job)
            .bind(MAX_LISTED)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
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
