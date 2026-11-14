//! The `forge/ai-eval` check (plan 17 P17-2, §1.6; ADR-0034): forge's
//! worker reads the AI evaluation report a pipeline's job left as the
//! artifact `eval-report.json`, compares it with the baseline
//! `evals/baselines/<models>.json` (`forge_core::ai_eval`) and writes the
//! check of the commit as the system (label `forge-ci`). The job itself
//! can write no check (ADR-0029 decision 8) and no person or token can over
//! HTTP (a `forge/` name).
//!
//! **Only pipelines that declare the report.** A pipeline is swept only
//! when a planned job declares `eval-report.json`
//! (`ci_pipelines.ai_eval_job`); every other pipeline never gets the check.
//!
//! **The baseline is the target's, not the change's** (ADR-0034 §2,
//! review HIGH 3): it is read at the merge base of the pipeline's commit
//! with the tip of the base branch — a merge request's target branch, a
//! merge queue train's branch, otherwise the repository's default branch.
//! A merge request that edits `evals/baselines/` (its gates, its
//! `allowed`) is still judged by the baseline it would replace; the new
//! one counts once a person merged it (a protected path).
//!
//! **Never success without a comparison.** `pending` → `queued`,
//! `running` → `in_progress` (past [`MAX_PIPELINE_AGE_SECS`] →
//! `error`); once the pipeline ended:
//! - the declaring job did not succeed, or left no report, or its file is
//!   gone → `error` FG-AIASSET-0002 (warn, runbook `ai-eval-bench`);
//! - the report is not v1, too large, or its bytes no longer match the
//!   sha256 measured at upload → `error` FG-AIASSET-0007;
//! - no usable baseline (or no merge base) → `error` FG-AIASSET-0003;
//! - otherwise the gates: `success`, or `failure` (FG-AIASSET-0004 / 0006)
//!   with the comparison table in the summary for the merge request.
//!
//! **A read that keeps failing ends as `error`** (review HIGH 4): a
//! failure to read the report or the baseline (storage, git, a repository
//! not ready) is counted and backed off (`forge_db::ci_ai_eval::
//! record_failure`); after [`MAX_READ_FAILURES`] — or at once for an
//! internal error that cannot heal — the check is written `error`
//! FG-AIASSET-0002 `reason=read_failed` with the cause's code.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forge_core::ai_eval::{self, baseline_path, evaluate, MAX_REPORT_BYTES};
use forge_core::checks::{CheckConclusion, CheckStatus, AI_EVAL_CHECK};
use forge_core::{CheckName, ObjectId, RefName, RepoId, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::NewCheckRun;
use forge_db::ci_ai_eval::{self as db, AiEvalCheck, AiEvalTarget};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError, Severity};
use forge_git::browse::{Browser, RefLookup, RepoPath, SmallFile};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::artifacts::ArtifactStore;
use crate::outputs::CI_ACTOR_LABEL;
use crate::{JobStatus, PipelineStatus};

/// Pipelines one pass looks at most.
pub const AI_EVAL_BATCH: i64 = 50;
/// Read failures of one pipeline before its check is written `error`.
pub const MAX_READ_FAILURES: i32 = 5;
/// A pipeline still `pending` / `running` after this long gets `error`
/// (24 hours; the bench's own timeout is at most 4 hours).
pub const MAX_PIPELINE_AGE_SECS: i64 = 24 * 60 * 60;
/// The bound of reading one report file.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// What [`BaselineSource::baseline`] returns.
pub type BaselineFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SmallFile, ForgeError>> + Send + 'a>>;
/// What [`BaselineSource::base`] returns.
pub type BaseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<ObjectId>, ForgeError>> + Send + 'a>>;

/// Where the baseline is read (the browse API: bounded bytes, its timeout,
/// a symlink never followed). Tests give fixed answers.
pub trait BaselineSource: Send + Sync {
    /// The merge base of `commit` with the tip of `branch` (`None`: the
    /// repository's default branch). `Ok(None)`: no such branch, no
    /// default branch, or no shared history.
    fn base<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        branch: Option<&'a RefName>,
    ) -> BaseFuture<'a>;

    /// The file at `path` of `commit`.
    fn baseline<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        path: &'a str,
    ) -> BaselineFuture<'a>;
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

impl BaselineSource for Browser {
    fn base<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        branch: Option<&'a RefName>,
    ) -> BaseFuture<'a> {
        Box::pin(async move {
            let name = match branch {
                Some(b) => b.clone(), // clone: the lookup owns its name
                None => {
                    let refs = self.refs(repo).await?;
                    let Some(default) = refs.default_branch.as_deref() else {
                        return Ok(None);
                    };
                    let Ok(name) = RefName::parse(&format!("refs/heads/{default}")) else {
                        return Ok(None);
                    };
                    name
                }
            };
            let tip = match self.find_ref(repo, &name).await? {
                RefLookup::Commit(rev) => {
                    ObjectId::parse(&rev.commit).map_err(|_| internal("base_tip_invalid"))?
                }
                RefLookup::Missing | RefLookup::NotACommit => return Ok(None),
            };
            self.merge_base(repo, commit, &tip).await
        })
    }

    fn baseline<'a>(
        &'a self,
        repo: RepoId,
        commit: &'a ObjectId,
        path: &'a str,
    ) -> BaselineFuture<'a> {
        Box::pin(async move {
            let path = RepoPath::parse(path)
                .map_err(|rule| internal("baseline_path_invalid").with_context("rule", rule))?;
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
    /// What is at fault (a field, a case, the baseline's path and commit,
    /// a cause's code): never the report's text.
    pub detail: Option<String>,
    pub findings: usize,
}

fn name() -> Result<CheckName, ForgeError> {
    CheckName::parse(AI_EVAL_CHECK).map_err(|_| internal("ai_eval_name_invalid"))
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

/// The file's bytes, at most `MAX_REPORT_BYTES + 1` read (the size is
/// checked on the open file before, and on what was read after).
async fn read_bounded(path: &std::path::Path) -> std::io::Result<Option<Vec<u8>>> {
    let file = tokio::fs::File::open(path).await?;
    let max = u64::try_from(MAX_REPORT_BYTES).unwrap_or(u64::MAX);
    if file.metadata().await?.len() > max {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).await?;
    Ok((bytes.len() <= MAX_REPORT_BYTES).then_some(bytes))
}

/// The report's bytes: `Ok(Err(reason))` for an answer the check gives
/// (`report_file_missing`, `too_large`, `stored_digest_mismatch`), `Err`
/// for a failure to read (counted, retried).
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
    let bytes = match tokio::time::timeout(READ_TIMEOUT, read_bounded(&path)).await {
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::StorageIoFailed)
                .with_context("operation", "ai_eval_report_read")
                .with_context("reason", "timeout"))
        }
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(("report_file_missing", ErrorCode::BenchUnavailable)))
        }
        Ok(Err(e)) => return Err(storage_failed("ai_eval_report_read", e)),
        Ok(Ok(None)) => return Ok(Err(("too_large", ErrorCode::EvalReportInvalid))),
        Ok(Ok(Some(bytes))) => bytes,
    };
    if Sha256::digest(&bytes).as_slice() != report.sha256.as_slice() {
        return Ok(Err((
            "stored_digest_mismatch",
            ErrorCode::EvalReportInvalid,
        )));
    }
    Ok(Ok(bytes))
}

/// The `forge/ai-eval` run of `t` (module docs). Errors (nothing written
/// yet; counted by the sweep): `FG-REPO-0016` (the repository is not
/// ready), git and storage failures, `FG-SYS-0001`.
pub async fn build(
    t: &AiEvalTarget,
    store: &ArtifactStore,
    baselines: &dyn BaselineSource,
) -> Result<Built, ForgeError> {
    let waiting = match t.status {
        PipelineStatus::Pending => Some(CheckStatus::Queued),
        PipelineStatus::Running => Some(CheckStatus::InProgress),
        PipelineStatus::Succeeded
        | PipelineStatus::Failed
        | PipelineStatus::Canceled
        | PipelineStatus::Skipped
        | PipelineStatus::Invalid
        | PipelineStatus::Error => None,
    };
    if let Some(status) = waiting {
        if t.age_secs > MAX_PIPELINE_AGE_SECS {
            let detail = format!("pipeline_{} for {} s", t.status.as_str(), t.age_secs);
            return error(
                t,
                ErrorCode::BenchUnavailable,
                "pipeline_stale",
                Some(detail),
            );
        }
        return pending(
            status,
            format!(
                "The AI evaluation waits for pipeline #{} (job `{}`).",
                t.pipeline_id, t.job
            ),
        );
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
    let branch = t
        .base_branch
        .as_ref()
        .map_or("default branch", RefName::as_str);
    let Some(base) = baselines.base(repo, &t.sha, t.base_branch.as_ref()).await? else {
        let detail = format!("{path} at the merge base with {branch}");
        return error(
            t,
            ErrorCode::BaselineMissing,
            "base_unresolved",
            Some(detail),
        );
    };
    let missing = |reason: &str, detail: Option<String>| {
        let at = format!("{path} at {} (merge base with {branch})", base.as_str());
        let detail = Some(match detail {
            Some(d) => format!("{at}: {d}"),
            None => at,
        });
        error(t, ErrorCode::BaselineMissing, reason, detail)
    };
    let baseline = match baselines.baseline(repo, &base, &path).await? {
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
        detail: Some(format!("baseline {path} at {}", base.as_str())),
        findings: evaluation.findings.len(),
    })
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiEvalPass {
    pub written: usize,
    pub superseded: usize,
    pub skipped: usize,
    /// Reads that failed and were counted (retried after their backoff).
    pub retried: usize,
    /// Pipelines whose target or write failed in the database.
    pub failed: usize,
}

impl AiEvalPass {
    /// Every due pipeline was looked at (a full batch runs again at once).
    pub fn looked_at(&self) -> usize {
        self.written + self.superseded + self.skipped + self.retried + self.failed
    }
}

/// What happened to one pipeline.
enum Done {
    Written(Box<AiEvalTarget>, Box<Built>, bool),
    Superseded,
    Skipped,
    /// A read failure counted and backed off.
    Retried,
}

/// One pass of the sweep: every due declaring pipeline (at most
/// [`AI_EVAL_BATCH`]), each built from its target and written in its own
/// transaction. A read failure is counted and backed off
/// (`forge.ai_eval.read_failed`, info, with `error_code` and `failures`);
/// the last one writes `error` (module docs). Each written run is logged as
/// `forge.ai_eval.check_written` — warn for every `error`, info otherwise —
/// with its conclusion, `error_code`, `reason` and `detail`. A database
/// failure of one pipeline is logged (`forge.ai_eval.check_failed`) and the
/// first is returned after the others, so the supervisor sees the streak.
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
        match one(db_handle, &ctx, store, baselines, pipeline, request_id).await {
            Ok(Done::Written(t, built, completed_event)) => {
                pass.written += 1;
                log_written(request_id, &t, &built, completed_event);
            }
            Ok(Done::Superseded) => pass.superseded += 1,
            Ok(Done::Skipped) => {
                pass.skipped += 1;
                tracing::debug!(
                    event = "forge.ai_eval.check_skipped",
                    request_id = %request_id,
                    pipeline_id = pipeline,
                    "the pipeline changed or is held; looked at again on the next pass"
                );
            }
            Ok(Done::Retried) => pass.retried += 1,
            Err(e) => {
                pass.failed += 1;
                tracing::warn!(
                    event = "forge.ai_eval.check_failed",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    pipeline_id = pipeline,
                    error = %e,
                    "the forge/ai-eval check could not be read or written in the database"
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

/// One pipeline: its target, the run (or the read failure counted, or the
/// `error` it ends in), the write.
async fn one(
    db_handle: &Db,
    ctx: &ChangeContext,
    store: &ArtifactStore,
    baselines: &dyn BaselineSource,
    pipeline: i64,
    request_id: &RequestId,
) -> Result<Done, ForgeError> {
    let Some(t) = db::target(db_handle, pipeline).await? else {
        return Ok(Done::Skipped);
    };
    let built = match build(&t, store, baselines).await {
        Ok(built) => built,
        Err(cause) => {
            let last = t.failures.saturating_add(1) >= MAX_READ_FAILURES;
            // An internal error does not heal by waiting.
            if !last && cause.code() != ErrorCode::Internal {
                let Some(failures) = db::record_failure(db_handle, &t).await? else {
                    return Ok(Done::Skipped);
                };
                tracing::info!(
                    event = "forge.ai_eval.read_failed",
                    request_id = %request_id,
                    error_code = cause.code().id(),
                    pipeline_id = t.pipeline_id,
                    failures,
                    error = %cause,
                    "the report or baseline could not be read; tried again after a backoff"
                );
                return Ok(Done::Retried);
            }
            let detail = format!(
                "cause {} after {} attempts",
                cause.code().id(),
                t.failures.saturating_add(1)
            );
            error(&t, ErrorCode::BenchUnavailable, "read_failed", Some(detail))?
        }
    };
    Ok(match db::record(db_handle, ctx, &t, &built.run).await? {
        AiEvalCheck::Written(reported) => {
            Done::Written(Box::new(t), Box::new(built), reported.completed_event)
        }
        AiEvalCheck::Superseded => Done::Superseded,
        AiEvalCheck::Skipped => Done::Skipped,
    })
}

fn log_written(request_id: &RequestId, t: &AiEvalTarget, built: &Built, completed_event: bool) {
    let conclusion = built.run.conclusion.map(CheckConclusion::as_str);
    let code = built.code.map(|c| c.id());
    let reason = built.reason.as_deref().unwrap_or("");
    let detail = built.detail.as_deref().unwrap_or("");
    let error_conclusion = built.run.conclusion == Some(CheckConclusion::Error)
        || built.code.is_some_and(|c| c.severity() != Severity::Info);
    if error_conclusion {
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
            "forge/ai-eval is error: no usable report or baseline"
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
