//! The `forge/ai-eval` check (plan 17 P17-2, §1.6; ADR-0034): forge's
//! worker reads the AI evaluation report a pipeline's job left as the
//! artifact `eval-report.json`, compares it with the baseline
//! `evals/baselines/<models>.json` at the pipeline's commit
//! (`forge_core::ai_eval`) and writes the check of the commit as the system
//! (label `forge-ci`). The job itself can write no check (ADR-0029
//! decision 8) and no person or token can over HTTP (a `forge/` name).
//!
//! **Only pipelines that declare the report.** A pipeline is swept only
//! when a planned job declares `eval-report.json`
//! (`ci_pipelines.ai_eval_job`); every other pipeline never gets the check
//! (no row, never required by accident).
//!
//! **Never success without a comparison.** `pending` → `queued`,
//! `running` → `in_progress`; once the pipeline ended:
//! - the declaring job did not succeed, or left no report, or its file is
//!   gone → `error` FG-AIASSET-0002 (warn, runbook `ai-eval-bench`);
//! - the report is not v1, too large, or its bytes no longer match the
//!   sha256 measured at upload → `error` FG-AIASSET-0007;
//! - no usable baseline → `error` FG-AIASSET-0003;
//! - otherwise the gates: `success`, or `failure` (FG-AIASSET-0004 / 0006)
//!   with the comparison table in the summary for the merge request.
//!
//! A failure to read (storage I/O, git, the database) writes nothing: the
//! pipeline stays due and the next pass tries again (the supervisor logs
//! the streak); a required `forge/ai-eval` keeps waiting meanwhile.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_core::ai_eval::{self, baseline_path, evaluate, MAX_REPORT_BYTES};
use forge_core::checks::{CheckConclusion, CheckStatus, AI_EVAL_CHECK};
use forge_core::{CheckName, ObjectId, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::NewCheckRun;
use forge_db::ci_ai_eval::{self as db, AiEvalCheck, AiEvalTarget};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, RepoPath, SmallFile};
use sha2::{Digest, Sha256};

use crate::artifacts::ArtifactStore;
use crate::outputs::CI_ACTOR_LABEL;
use crate::{JobStatus, PipelineStatus};

/// Pipelines one pass looks at most.
pub const AI_EVAL_BATCH: i64 = 50;
/// The bound of reading one report file.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`BaselineSource::baseline`] returns.
pub type BaselineFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SmallFile, ForgeError>> + Send + 'a>>;

/// Reads the baseline file at a commit (the browse API: bounded bytes, its
/// timeout, a symlink never followed). Tests give a fixed answer.
pub trait BaselineSource: Send + Sync {
    fn baseline<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        path: &'a str,
    ) -> BaselineFuture<'a>;
}

impl BaselineSource for Browser {
    fn baseline<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        path: &'a str,
    ) -> BaselineFuture<'a> {
        Box::pin(async move {
            let path = RepoPath::parse(path).map_err(|rule| {
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "baseline_path_invalid")
                    .with_context("rule", rule)
            })?;
            let max = u64::try_from(MAX_REPORT_BYTES).unwrap_or(u64::MAX);
            self.small_file(repo, commit.as_str(), &path, max).await
        })
    }
}

/// The run [`build`] made and why (for the log).
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub run: NewCheckRun,
    /// The registered code of an `error` / `failure` (`None`: success or
    /// not finished).
    pub code: Option<ErrorCode>,
    pub reason: Option<String>,
    /// What is at fault (a field, a case, the baseline's path): never the
    /// report's text.
    pub detail: Option<String>,
    pub findings: usize,
}

fn name() -> Result<CheckName, ForgeError> {
    CheckName::parse(AI_EVAL_CHECK).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "ai_eval_name_invalid")
    })
}

fn pending(status: CheckStatus, summary: String) -> Result<Built, ForgeError> {
    Ok(Built {
        run: NewCheckRun {
            name: name()?,
            status,
            conclusion: None,
            summary,
            details_url: None,
            annotations: Vec::new(),
        },
        code: None,
        reason: None,
        detail: None,
        findings: 0,
    })
}

/// An `error` run: the bench gave nothing to compare (gate 5).
fn error(
    t: &AiEvalTarget,
    code: ErrorCode,
    reason: &str,
    detail: Option<String>,
) -> Result<Built, ForgeError> {
    let mut summary = format!(
        "**forge/ai-eval error** [{}] {reason} — pipeline #{}, job `{}`.",
        code.id(),
        t.pipeline_id,
        t.job
    );
    if let Some(d) = &detail {
        summary.push_str(&format!(" At fault: {d}."));
    }
    summary.push_str(&format!(" {}", code.entry().user_message_en));
    if let Some(runbook) = code.entry().runbook {
        summary.push_str(&format!(" Runbook: {runbook}."));
    }
    Ok(Built {
        run: NewCheckRun {
            name: name()?,
            status: CheckStatus::Completed,
            conclusion: Some(CheckConclusion::Error),
            summary,
            details_url: None,
            annotations: Vec::new(),
        },
        code: Some(code),
        reason: Some(reason.to_owned()),
        detail,
        findings: 0,
    })
}

fn storage_failed(operation: &'static str, e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::StorageIoFailed)
        .with_context("operation", operation)
        .with_source(e)
}

/// The report's bytes: `Ok(Err(reason))` for an answer the check gives
/// (`report_file_missing`, `too_large`, `stored_digest_mismatch`), `Err`
/// for a failure to read (retried).
async fn read_report(
    store: &ArtifactStore,
    t: &AiEvalTarget,
    report: &db::ReportArtifact,
) -> Result<Result<Vec<u8>, (&'static str, ErrorCode)>, ForgeError> {
    if usize::try_from(report.size_bytes)
        .ok()
        .is_none_or(|n| n > MAX_REPORT_BYTES)
    {
        return Ok(Err(("too_large", ErrorCode::EvalReportInvalid)));
    }
    let path = store.file(t.project.get(), report.id);
    let read = tokio::time::timeout(READ_TIMEOUT, tokio::fs::read(&path)).await;
    let bytes = match read {
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "ai_eval_report_read")
                .with_context("reason", "timeout"))
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(("report_file_missing", ErrorCode::BenchUnavailable)))
        }
        Ok(Err(e)) => return Err(storage_failed("ai_eval_report_read", e)),
        Ok(Ok(bytes)) => bytes,
    };
    if bytes.len() > MAX_REPORT_BYTES {
        return Ok(Err(("too_large", ErrorCode::EvalReportInvalid)));
    }
    if Sha256::digest(&bytes).as_slice() != report.sha256.as_slice() {
        return Ok(Err((
            "stored_digest_mismatch",
            ErrorCode::EvalReportInvalid,
        )));
    }
    Ok(Ok(bytes))
}

/// The `forge/ai-eval` run of `t` (module docs). Errors (nothing to
/// write; the next pass retries): `FG-REPO-0016` (the repository is not
/// ready), git and storage failures, `FG-SYS-0001`.
pub async fn build(
    t: &AiEvalTarget,
    store: &ArtifactStore,
    baselines: &dyn BaselineSource,
) -> Result<Built, ForgeError> {
    match t.status {
        PipelineStatus::Pending => {
            return pending(
                CheckStatus::Queued,
                format!("The AI evaluation waits for pipeline #{}.", t.pipeline_id),
            )
        }
        PipelineStatus::Running => {
            return pending(
                CheckStatus::InProgress,
                format!(
                    "The AI evaluation bench runs in pipeline #{} (job `{}`).",
                    t.pipeline_id, t.job
                ),
            )
        }
        PipelineStatus::Succeeded
        | PipelineStatus::Failed
        | PipelineStatus::Canceled
        | PipelineStatus::Skipped
        | PipelineStatus::Invalid
        | PipelineStatus::Error => {}
    }
    match t.job_status {
        Some(JobStatus::Succeeded) => {}
        Some(JobStatus::Failed) => {
            return error(t, ErrorCode::BenchUnavailable, "job_failed", None)
        }
        Some(JobStatus::Canceled) => {
            return error(t, ErrorCode::BenchUnavailable, "job_canceled", None)
        }
        Some(JobStatus::Created | JobStatus::Pending | JobStatus::Running) | None => {
            let reason = format!("pipeline_{}", t.status.as_str());
            return error(t, ErrorCode::BenchUnavailable, "job_not_run", Some(reason));
        }
    }
    let Some(report) = &t.report else {
        return error(t, ErrorCode::BenchUnavailable, "report_missing", None);
    };
    let bytes = match read_report(store, t, report).await? {
        Ok(bytes) => bytes,
        Err((reason, code)) => return error(t, code, reason, None),
    };
    let report = match ai_eval::parse(&bytes) {
        Ok(r) => r,
        Err(p) => return error(t, ErrorCode::EvalReportInvalid, p.reason, Some(p.detail)),
    };
    let repo = t.repo.ok_or_else(|| {
        ForgeError::new(ErrorCode::RepositoryNotReady)
            .with_context("project_id", t.project.get())
            .with_context("operation", "ai_eval_baseline")
    })?;
    let path = baseline_path(&report.models);
    let missing = |reason: &str, detail: Option<String>| {
        let detail = Some(match detail {
            Some(d) => format!("{path} ({d})"),
            None => path.clone(), // clone: the run owns its detail
        });
        error(t, ErrorCode::BaselineMissing, reason, detail)
    };
    let baseline = match baselines.baseline(repo, &t.sha, &path).await? {
        SmallFile::Absent => return missing("baseline_absent", None),
        SmallFile::NotAFile => return missing("baseline_not_a_file", None),
        SmallFile::TooLarge => return missing("baseline_too_large", None),
        SmallFile::Found(bytes) => match ai_eval::parse(&bytes) {
            Ok(b) => b,
            Err(p) => {
                return missing(
                    "baseline_invalid",
                    Some(format!("{}: {}", p.reason, p.detail)),
                )
            }
        },
    };
    if baseline.models != report.models {
        return missing("baseline_models_mismatch", None);
    }
    let evaluation = evaluate(&report, &baseline);
    let conclusion = if evaluation.passed() {
        CheckConclusion::Success
    } else {
        CheckConclusion::Failure
    };
    Ok(Built {
        run: NewCheckRun {
            name: name()?,
            status: CheckStatus::Completed,
            conclusion: Some(conclusion),
            summary: evaluation.summary(),
            details_url: None,
            annotations: Vec::new(),
        },
        code: evaluation.code(),
        reason: evaluation.findings.first().map(|f| f.reason().to_owned()),
        detail: None,
        findings: evaluation.findings.len(),
    })
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiEvalPass {
    pub written: usize,
    pub superseded: usize,
    pub skipped: usize,
    /// Pipelines whose read or write failed (logged, retried next pass).
    pub failed: usize,
}

impl AiEvalPass {
    /// Every due pipeline was looked at (a full batch runs again at once).
    pub fn looked_at(&self) -> usize {
        self.written + self.superseded + self.skipped + self.failed
    }
}

/// One pass of the sweep: every declaring pipeline whose status is not
/// yet its check's (at most [`AI_EVAL_BATCH`]), each built from its
/// target and written in its own transaction. A pipeline whose read or
/// write fails is logged (`forge.ai_eval.check_failed` with `error_code`)
/// and left for the next pass; the first such error is returned after the
/// others, so the supervisor sees the streak. Each written run is logged
/// as `forge.ai_eval.check_written` (warn for FG-AIASSET-0002, info
/// otherwise) with its conclusion, `error_code`, `reason` and `detail`.
/// Errors: the listing's `FG-DB-*`.
pub async fn report_ai_eval_checks(
    db_handle: &Db,
    store: &ArtifactStore,
    baselines: &dyn BaselineSource,
    request_id: &RequestId,
) -> Result<AiEvalPass, ForgeError> {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(CI_ACTOR_LABEL.to_owned());
    let mut pass = AiEvalPass::default();
    let mut first_error = None;
    for pipeline in db::pipelines_due(db_handle, AI_EVAL_BATCH).await? {
        match one(db_handle, &ctx, store, baselines, pipeline).await {
            Ok((AiEvalCheck::Written(reported), Some((t, built)))) => {
                pass.written += 1;
                log_written(request_id, &t, &built, reported.completed_event);
            }
            Ok((AiEvalCheck::Written(_), None)) => pass.written += 1,
            Ok((AiEvalCheck::Superseded, _)) => pass.superseded += 1,
            Ok((AiEvalCheck::Skipped, _)) => pass.skipped += 1,
            Err(e) => {
                pass.failed += 1;
                tracing::warn!(
                    event = "forge.ai_eval.check_failed",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    pipeline_id = pipeline,
                    error = %e,
                    "the forge/ai-eval check could not be built or written; retried on the next pass"
                );
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e
            .with_context("operation", "ai_eval_checks")
            .with_context("failed", pass.failed)),
        None => Ok(pass),
    }
}

/// One pipeline: its target, the run, the write (and what was written).
async fn one(
    db_handle: &Db,
    ctx: &ChangeContext,
    store: &ArtifactStore,
    baselines: &dyn BaselineSource,
    pipeline: i64,
) -> Result<(AiEvalCheck, Option<(AiEvalTarget, Built)>), ForgeError> {
    let Some(t) = db::target(db_handle, pipeline).await? else {
        return Ok((AiEvalCheck::Skipped, None));
    };
    let built = build(&t, store, baselines).await?;
    let done = db::record(db_handle, ctx, &t, &built.run).await?;
    Ok((done, Some((t, built))))
}

fn log_written(request_id: &RequestId, t: &AiEvalTarget, built: &Built, completed_event: bool) {
    let conclusion = built.run.conclusion.map(CheckConclusion::as_str);
    let code = built.code.map(|c| c.id());
    let reason = built.reason.as_deref().unwrap_or("");
    let detail = built.detail.as_deref().unwrap_or("");
    if built.code == Some(ErrorCode::BenchUnavailable) {
        tracing::warn!(
            event = "forge.ai_eval.check_written",
            request_id = %request_id,
            error_code = code,
            project_id = t.project.get(),
            pipeline_id = t.pipeline_id,
            head_sha = t.sha.as_str(),
            job = t.job.as_str(),
            conclusion,
            reason,
            detail,
            "forge/ai-eval is error: the bench left no usable report"
        );
        return;
    }
    tracing::info!(
        event = "forge.ai_eval.check_written",
        request_id = %request_id,
        error_code = code,
        project_id = t.project.get(),
        pipeline_id = t.pipeline_id,
        head_sha = t.sha.as_str(),
        job = t.job.as_str(),
        status = built.run.status.as_str(),
        conclusion,
        reason,
        detail,
        findings = built.findings,
        completed_event,
        "the forge/ai-eval check follows its pipeline"
    );
}

#[cfg(test)]
#[path = "ai_eval_tests.rs"]
mod tests;
