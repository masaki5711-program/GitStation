//! The `forge/ai-eval` check's store (plan 17 P17-2, ADR-0034; migration
//! 20261217100000).
//!
//! A pipeline planned with a job that declares the AI evaluation report
//! (`ci_pipelines.ai_eval_job`, set by `pipelines::create`) gets the check
//! `forge/ai-eval` on its commit; every other pipeline never does. The
//! worker's sweep (forge-pipelines `ai_eval`) works in three steps so no
//! file or git read happens inside a transaction:
//!
//! 1. [`pipelines_due`]: the declaring pipelines whose status is not yet
//!    the check's (`ai_eval_status IS DISTINCT FROM status`);
//! 2. [`target`]: what the check is made from — the pipeline, the
//!    declaring job's status and attempt, and the report artifact of that
//!    attempt (`eval-report.json`), if uploaded;
//! 3. [`record`]: the run the caller built, written as the system (label
//!    `forge-ci`, no reporter) through the same write as every check run
//!    (`checks::write_run`), only if the pipeline's status and the job's
//!    attempt are still what [`target`] saw (else nothing: the next pass
//!    looks again). Only the newest declaring pipeline of a (project,
//!    commit) writes; an older one is only marked.
//!
//! Lock order as `ci/pipeline`: the project row `FOR SHARE`, then the
//! pipeline row `FOR UPDATE SKIP LOCKED`, then the check's locks.

use forge_core::checks::AI_EVAL_CHECK;
use forge_core::{ObjectId, ProjectId, RepoId};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::checks::{write_run, NewCheckRun, Reported};
use crate::pipelines::{JobStatus, PipelineStatus};
use crate::projects::lock_shared;
use crate::{query_error, read_query_error, Db};

/// The report's artifact path (forge-ci's `EVAL_REPORT_PATH`; the store
/// does not depend on forge-ci, the test `the_report_path_is_forge_cis`
/// in forge-pipelines keeps them equal).
pub const REPORT_PATH: &str = "eval-report.json";

fn unreadable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// The declaring pipelines whose status is not yet their check's, oldest
/// first (at most `limit`).
pub async fn pipelines_due(db: &Db, limit: i64) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "ci_ai_eval.due";
    db.within(QUERY, async {
        sqlx::query_scalar(
            "SELECT id FROM forge.ci_pipelines \
             WHERE ai_eval_job IS NOT NULL AND ai_eval_status IS DISTINCT FROM status \
             ORDER BY id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The report artifact of the declaring job's current attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportArtifact {
    pub id: i64,
    pub size_bytes: i64,
    /// The sha256 forge measured at upload (32 bytes).
    pub sha256: Vec<u8>,
}

/// What the `forge/ai-eval` run of a pipeline is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiEvalTarget {
    pub pipeline_id: i64,
    pub project: ProjectId,
    /// The project's repository, when it is ready (the baseline is read
    /// from it).
    pub repo: Option<RepoId>,
    pub sha: ObjectId,
    pub status: PipelineStatus,
    /// The declaring job's name.
    pub job: String,
    /// `None`: the job row is missing (cannot happen: the pipeline was
    /// planned with it).
    pub job_status: Option<JobStatus>,
    pub job_attempt: i32,
    pub report: Option<ReportArtifact>,
}

type TargetRow = (
    i64,
    String,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<i32>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
);

/// The target of `pipeline` (`None`: gone, or it declares no report).
/// Errors: `FG-SYS-0001` for a stored value the table should have refused,
/// `FG-DB-*`.
pub async fn target(db: &Db, pipeline: i64) -> Result<Option<AiEvalTarget>, ForgeError> {
    const QUERY: &str = "ci_ai_eval.target";
    let row: Option<TargetRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.project_id, p.sha, p.status, p.ai_eval_job, r.id, j.status, j.attempt, \
                   a.id, a.size_bytes, a.sha256 \
                 FROM forge.ci_pipelines p \
                 LEFT JOIN forge.repositories r ON r.project_id = p.project_id \
                   AND r.state = 'ready' \
                 LEFT JOIN forge.ci_jobs j ON j.pipeline_id = p.id AND j.name = p.ai_eval_job \
                 LEFT JOIN forge.ci_artifacts a ON a.job_id = j.id AND a.attempt = j.attempt \
                   AND a.path = $2 \
                 WHERE p.id = $1 AND p.ai_eval_job IS NOT NULL",
            )
            .bind(pipeline)
            .bind(REPORT_PATH)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some((project, sha, status, job, repo, job_status, attempt, art, size, digest)) = row
    else {
        return Ok(None);
    };
    let status =
        PipelineStatus::parse(&status).ok_or_else(|| unreadable("stored_status_invalid"))?;
    let job_status = match job_status {
        Some(s) => {
            Some(JobStatus::parse(&s).ok_or_else(|| unreadable("stored_job_status_invalid"))?)
        }
        None => None,
    };
    let report = art
        .zip(size)
        .zip(digest)
        .map(|((id, size_bytes), sha256)| ReportArtifact {
            id,
            size_bytes,
            sha256,
        });
    Ok(Some(AiEvalTarget {
        pipeline_id: pipeline,
        project: ProjectId::new(project),
        repo: repo.map(RepoId::new),
        sha: ObjectId::parse(&sha).map_err(|_| unreadable("stored_sha_invalid"))?,
        status,
        job,
        job_status,
        job_attempt: attempt.unwrap_or(0),
        report,
    }))
}

/// What [`record`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiEvalCheck {
    /// The pipeline is gone, held by another transaction, changed since
    /// [`target`] (its status or the job's attempt), or already reported
    /// at its status: nothing done.
    Skipped,
    /// A newer declaring pipeline of the same commit owns the check: only
    /// marked.
    Superseded,
    Written(Box<Reported>),
}

/// Write `run` (a `forge/ai-eval` run, `FG-SYS-0001 reason=not_ai_eval_check`
/// otherwise) for `target` as `ctx`'s actor (the system): module docs.
/// Errors (nothing written): `FG-REVIEW-0021` (cannot happen: the name is
/// outside the cap), `FG-DB-*`.
pub async fn record(
    db: &Db,
    ctx: &ChangeContext,
    target: &AiEvalTarget,
    run: &NewCheckRun,
) -> Result<AiEvalCheck, ForgeError> {
    const QUERY: &str = "ci_ai_eval.record";
    if run.name.as_str() != AI_EVAL_CHECK {
        return Err(
            unreadable("not_ai_eval_check").with_context("name", run.name.as_str().to_owned())
        );
    }
    let (project, pipeline) = (target.project, target.pipeline_id);
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        match lock_shared(&mut tx, project).await {
            Ok(_) => {}
            // Purged meanwhile: its pipelines are gone too.
            Err(e) if e.code() == ErrorCode::NotFound => {
                return Ok(Settle::Discard(AiEvalCheck::Skipped))
            }
            Err(e) => return Err(e),
        }
        type Locked = (String, Option<String>, Option<i32>, bool);
        let locked: Option<Locked> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT p.status, p.ai_eval_status, \
                       (SELECT j.attempt FROM forge.ci_jobs j \
                         WHERE j.pipeline_id = p.id AND j.name = p.ai_eval_job), \
                       NOT EXISTS (SELECT 1 FROM forge.ci_pipelines q WHERE \
                         q.project_id = p.project_id AND q.sha = p.sha AND q.id > p.id \
                         AND q.ai_eval_job IS NOT NULL) \
                     FROM forge.ci_pipelines p \
                     WHERE p.id = $1 AND p.project_id = $2 AND p.ai_eval_job IS NOT NULL \
                     FOR UPDATE OF p SKIP LOCKED",
                )
                .bind(pipeline)
                .bind(project.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let Some((status, reported, attempt, newest)) = locked else {
            return Ok(Settle::Discard(AiEvalCheck::Skipped));
        };
        let changed =
            status != target.status.as_str() || attempt.unwrap_or(0) != target.job_attempt;
        if changed || reported.as_deref() == Some(status.as_str()) {
            return Ok(Settle::Discard(AiEvalCheck::Skipped));
        }
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.ci_pipelines SET ai_eval_status = $2 WHERE id = $1")
                .bind(pipeline)
                .bind(status)
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
        if !newest {
            return Ok(Settle::CommitDerived(AiEvalCheck::Superseded));
        }
        let (reported, change) = write_run(&mut tx, ctx, (project, &target.sha), None, run).await?;
        Ok(Settle::Commit(
            change,
            AiEvalCheck::Written(Box::new(reported)),
        ))
    }
    .await;
    tx.settle("check_run.reported", outcome).await
}

#[cfg(test)]
mod tests;
