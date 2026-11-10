//! The CI outputs (M4 C5, plan 14 §5 C5, ADR-0029 decisions 5 and 7):
//! a job's log and a pipeline's diagnosis on a route's proof, and the
//! worker's two sweeps — failure fingerprints (plan 04 P-6 / P-7) and the
//! `ci/pipeline` check.
//!
//! **The marks.** [`Pipelines::job_log`] and [`Pipelines::diagnosis`] read
//! through `forge_db::ci_outputs::*_decided`, whose `decide` is
//! `forge_auth::authorize_ci_output` with the pipeline's own mark
//! (`ci_pipelines.copy_mark`) and the default branch's tip mark the route
//! measured (`NotMeasured` for a person). An AI agent audience gets
//! `FG-AUTH-0022` unless both are `Open`; no log text is read before that
//! (the store's test `ci_log_text_is_read_only_after_a_decision`). Every
//! page and excerpt is masked ([`crate::fingerprint::mask`]) for every
//! reader.
//!
//! **`ci/pipeline`** ([`report_pipeline_checks`]): the pipeline's status as
//! the check of its commit — `pending` → `queued`, `running` →
//! `in_progress`, `succeeded` / `skipped` (no job selected) → `success`,
//! `failed` / `invalid` (FG-CI-0001) → `failure`, `error` (FG-CI-0002) →
//! `error`, `canceled` → `neutral` (not success: a required check stays
//! unmet). Written as the system (label `forge-ci`), never over HTTP.

use forge_auth::{authorize_ci_output, Action, Authorized, CopyMark, ProjectCtx};
use forge_core::checks::{CheckConclusion, CheckStatus, CI_PIPELINE_CHECK};
use forge_core::{CheckName, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::checks::NewCheckRun;
use forge_db::ci_outputs::{
    self as db, Fingerprinted, LogPage, PipelineCheck, PipelineCheckFacts, StoredFailure,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::fingerprint::{digest, mask};
use crate::{require, CopyMark as StoredMark, JobStatus, PipelineStatus, Pipelines};

pub use forge_db::ci_outputs::{
    FoundFailure, KnownFix, StoredFailure as Failure, LOG_PAGE_MAX_BYTES, LOG_PAGE_MAX_CHUNKS,
    MAX_EXCERPT_BYTES,
};

/// The audit label of the system's `ci/pipeline` runs (ADR-0029 decision 7).
pub const CI_ACTOR_LABEL: &str = "forge-ci";
/// Pipelines one check pass writes at most.
pub const CHECK_BATCH: i64 = 100;
/// Failed jobs one fingerprint pass reads at most.
pub const FINGERPRINT_BATCH: i64 = 20;

/// The mark a stored pipeline was measured with, as `authorize()` reads it.
pub fn pipeline_mark(mark: StoredMark) -> CopyMark {
    match mark {
        StoredMark::Open => CopyMark::Open,
        StoredMark::Confidential => CopyMark::Confidential,
        StoredMark::Undecided => CopyMark::Undecided,
    }
}

/// What the route measured of the reader of a CI output: the default
/// branch's tip mark (`NotMeasured` for a person) and whether the request
/// declared an AI agent audience (`Forge-Context-Audience: agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputView {
    pub tip_mark: CopyMark,
    pub declared_agent: bool,
}

/// One page of a job's log, masked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLog {
    pub pipeline_id: i64,
    pub job_id: i64,
    pub job_name: String,
    pub job_status: JobStatus,
    /// The attempt this page is of, and the job's current one.
    pub attempt: i32,
    pub current_attempt: i32,
    /// The first and last chunk of the page (`None`: nothing stored from
    /// the asked chunk on).
    pub chunks: Option<(i32, i32)>,
    /// The page's text, masked.
    pub text: String,
    /// The next chunk to ask for (`None`: none stored yet).
    pub next_seq: Option<i32>,
    /// Nothing more will be appended to this attempt and nothing is left
    /// after this page.
    pub complete: bool,
}

fn finished(status: JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Succeeded | JobStatus::Failed | JobStatus::Canceled
    )
}

fn log_of(page: LogPage) -> JobLog {
    let chunks = page
        .chunks
        .first()
        .zip(page.chunks.last())
        .map(|(a, b)| (a.0, b.0));
    let text: String = page.chunks.iter().map(|(_, body)| body.as_str()).collect();
    let attempt_over = page.attempt < page.target.current_attempt || finished(page.target.status);
    JobLog {
        pipeline_id: page.target.pipeline_id,
        job_id: page.target.job_id,
        job_name: page.target.job_name,
        job_status: page.target.status,
        attempt: page.attempt,
        current_attempt: page.target.current_attempt,
        chunks,
        text: mask(&page.before, &text, &page.after),
        next_seq: page.next_seq,
        complete: attempt_over && page.next_seq.is_none(),
    }
}

/// A pipeline's diagnosis (plan 04 P-9, the deterministic part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub pipeline_id: i64,
    pub status: PipelineStatus,
    pub error_code: Option<String>,
    /// The failed jobs at their current attempt; an excerpt masked again.
    pub failures: Vec<StoredFailure>,
}

impl Pipelines {
    /// `GET …/pipelines/{pipeline_id}/jobs/{job_id}/log?attempt=&from=`
    /// (`CiLogRead`): one page from chunk `from` of `attempt` (default the
    /// current one), at most [`LOG_PAGE_MAX_CHUNKS`] chunks /
    /// [`LOG_PAGE_MAX_BYTES`], masked. `view` is the route's measurement
    /// of the reader ([`OutputView`]). `None`: no
    /// such pipeline, job or attempt in the project (the same 404 for
    /// everyone who may read the project). Errors: `FG-AUTH-0022` for an
    /// agent audience on a marked pipeline or tip (with `pipeline_id`,
    /// `job_id`), `FG-DB-*`.
    pub async fn job_log(
        &self,
        proof: &Authorized<ProjectCtx>,
        view: OutputView,
        (pipeline, job): (i64, i64),
        attempt: Option<i32>,
        from_seq: i32,
    ) -> Result<Option<JobLog>, ForgeError> {
        require(proof.action(), Action::CiLogRead)?;
        let read = db::job_log_decided(
            &self.db,
            *proof.id(),
            (pipeline, job),
            attempt,
            from_seq,
            |target| {
                authorize_ci_output(
                    proof,
                    pipeline_mark(target.copy_mark),
                    view.tip_mark,
                    view.declared_agent,
                )
                .map_err(|e| {
                    e.with_context("pipeline_id", pipeline)
                        .with_context("job_id", job)
                })
            },
        )
        .await?;
        Ok(read.map(|(page, _allowed)| log_of(page)))
    }

    /// `GET …/pipelines/{pipeline_id}/diagnosis` (`PipelineDiagnosisRead`):
    /// the pipeline's failed jobs with their fingerprints, error codes,
    /// masked excerpts and known fixes, decided as [`Pipelines::job_log`].
    /// `None`: no such pipeline in the project.
    pub async fn diagnosis(
        &self,
        proof: &Authorized<ProjectCtx>,
        view: OutputView,
        pipeline: i64,
    ) -> Result<Option<Diagnosis>, ForgeError> {
        require(proof.action(), Action::PipelineDiagnosisRead)?;
        let read = db::diagnosis_decided(&self.db, *proof.id(), pipeline, |target| {
            authorize_ci_output(
                proof,
                pipeline_mark(target.copy_mark),
                view.tip_mark,
                view.declared_agent,
            )
            .map_err(|e| e.with_context("pipeline_id", pipeline))
        })
        .await?;
        Ok(read.map(|(target, failures, _allowed)| Diagnosis {
            pipeline_id: target.pipeline_id,
            status: target.status,
            error_code: target.error_code,
            failures: failures
                .into_iter()
                .map(|mut f| {
                    if let Some(found) = f.found.as_mut() {
                        found.excerpt = mask("", &found.excerpt, "");
                    }
                    f
                })
                .collect(),
        }))
    }
}

/// The `ci/pipeline` run of a pipeline in `facts` (module docs).
pub fn pipeline_check_run(facts: &PipelineCheckFacts) -> Result<NewCheckRun, ForgeError> {
    let (status, conclusion) = match facts.status {
        PipelineStatus::Pending => (CheckStatus::Queued, None),
        PipelineStatus::Running => (CheckStatus::InProgress, None),
        PipelineStatus::Succeeded | PipelineStatus::Skipped => {
            (CheckStatus::Completed, Some(CheckConclusion::Success))
        }
        PipelineStatus::Failed | PipelineStatus::Invalid => {
            (CheckStatus::Completed, Some(CheckConclusion::Failure))
        }
        PipelineStatus::Error => (CheckStatus::Completed, Some(CheckConclusion::Error)),
        PipelineStatus::Canceled => (CheckStatus::Completed, Some(CheckConclusion::Neutral)),
    };
    let (succeeded, failed, canceled, total) = facts.jobs;
    let mut summary = format!(
        "Pipeline #{} {}: {total} jobs, {succeeded} succeeded, {failed} failed, {canceled} canceled.",
        facts.pipeline_id,
        facts.status.as_str()
    );
    if let Some(code) = &facts.error_code {
        summary.push_str(&format!(" [{code}]"));
    }
    if failed > 0 {
        summary.push_str(&format!(
            " Diagnosis: gstn pipeline diagnosis --project {} --pipeline {}",
            facts.project.get(),
            facts.pipeline_id
        ));
    }
    Ok(NewCheckRun {
        name: CheckName::parse(CI_PIPELINE_CHECK).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "ci_pipeline_name_invalid")
        })?,
        status,
        conclusion,
        summary,
        details_url: None,
        annotations: Vec::new(),
    })
}

/// What one check pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckPass {
    pub written: usize,
    pub superseded: usize,
    pub skipped: usize,
    /// Pipelines whose write failed (logged here, retried on the next pass).
    pub failed: usize,
}

/// One pass of the `ci/pipeline` sweep: every pipeline whose status is not
/// yet its check's (at most [`CHECK_BATCH`]), each in its own transaction.
/// A pipeline whose write fails is logged (`forge.ci.pipeline_check_failed`
/// with `error_code`) and left for the next pass; the first such error is
/// returned after the others were written, so the supervisor sees the
/// streak. Errors: the listing's `FG-DB-*`.
pub async fn report_pipeline_checks(
    db_handle: &Db,
    request_id: &RequestId,
) -> Result<CheckPass, ForgeError> {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(CI_ACTOR_LABEL.to_owned());
    let mut pass = CheckPass::default();
    let mut first_error = None;
    for pipeline in db::pipelines_due_for_check(db_handle, CHECK_BATCH).await? {
        let outcome =
            db::record_pipeline_check(db_handle, &ctx, pipeline, pipeline_check_run).await;
        match outcome {
            Ok(PipelineCheck::Written { reported, fixed }) => {
                pass.written += 1;
                tracing::info!(
                    event = "forge.ci.pipeline_check_written",
                    request_id = %request_id,
                    pipeline_id = pipeline,
                    head_sha = reported.run.head_sha.as_str(),
                    status = reported.run.status.as_str(),
                    conclusion = reported.run.conclusion.map(CheckConclusion::as_str),
                    completed_event = reported.completed_event,
                    fixed_fingerprints = fixed,
                    "the ci/pipeline check follows its pipeline"
                );
            }
            Ok(PipelineCheck::Superseded) => pass.superseded += 1,
            Ok(PipelineCheck::Skipped) => pass.skipped += 1,
            Err(e) => {
                pass.failed += 1;
                tracing::warn!(
                    event = "forge.ci.pipeline_check_failed",
                    request_id = %request_id,
                    error_code = e.code().id(),
                    pipeline_id = pipeline,
                    error = %e,
                    "the ci/pipeline check could not be written; retried on the next pass"
                );
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e
            .with_context("operation", "ci_pipeline_checks")
            .with_context("failed", pass.failed)),
        None => Ok(pass),
    }
}

/// One pass of the fingerprint sweep (plan 04 P-6): at most
/// [`FINGERPRINT_BATCH`] failed job attempts, masked and fingerprinted
/// ([`crate::fingerprint::digest`]). Each is logged as
/// `forge.ci.failure_fingerprinted` (the fingerprint and its count, never
/// log text). Errors: `FG-DB-*` (the attempts recorded before it stay).
pub async fn fingerprint_failures(
    db_handle: &Db,
    request_id: &RequestId,
) -> Result<Vec<Fingerprinted>, ForgeError> {
    let done = db::fingerprint_failed_jobs(db_handle, FINGERPRINT_BATCH, digest).await?;
    for f in &done {
        tracing::info!(
            event = "forge.ci.failure_fingerprinted",
            request_id = %request_id,
            project_id = f.project_id,
            pipeline_id = f.pipeline_id,
            job_id = f.job_id,
            attempt = f.attempt,
            fingerprint = %f.fingerprint,
            occurrences = f.occurrences,
            "a failed job is fingerprinted"
        );
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

    use super::*;
    use forge_core::ProjectId;

    fn facts(status: PipelineStatus, jobs: (i64, i64, i64, i64)) -> PipelineCheckFacts {
        PipelineCheckFacts {
            pipeline_id: 9,
            project: ProjectId::new(7),
            status,
            error_code: None,
            jobs,
        }
    }

    /// ADR-0029 decision 7: each pipeline status has one check run; only a
    /// success (or nothing to run) satisfies a required `ci/pipeline`.
    #[test]
    fn each_pipeline_status_maps_to_one_check_run() {
        let cases = [
            (PipelineStatus::Pending, CheckStatus::Queued, None),
            (PipelineStatus::Running, CheckStatus::InProgress, None),
            (
                PipelineStatus::Succeeded,
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            (
                PipelineStatus::Skipped,
                CheckStatus::Completed,
                Some(CheckConclusion::Success),
            ),
            (
                PipelineStatus::Failed,
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            ),
            (
                PipelineStatus::Invalid,
                CheckStatus::Completed,
                Some(CheckConclusion::Failure),
            ),
            (
                PipelineStatus::Error,
                CheckStatus::Completed,
                Some(CheckConclusion::Error),
            ),
            (
                PipelineStatus::Canceled,
                CheckStatus::Completed,
                Some(CheckConclusion::Neutral),
            ),
        ];
        for (pipeline, status, conclusion) in cases {
            let run = pipeline_check_run(&facts(pipeline, (1, 0, 0, 1))).expect("run");
            assert_eq!(run.name.as_str(), "ci/pipeline");
            assert_eq!(
                (run.status, run.conclusion),
                (status, conclusion),
                "{pipeline:?}"
            );
            assert!(run.details_url.is_none() && run.annotations.is_empty());
        }
        let failed = pipeline_check_run(&PipelineCheckFacts {
            error_code: Some("FG-CI-0004".to_owned()),
            ..facts(PipelineStatus::Failed, (1, 1, 0, 2))
        })
        .expect("run");
        assert_eq!(
            failed.summary,
            "Pipeline #9 failed: 2 jobs, 1 succeeded, 1 failed, 0 canceled. [FG-CI-0004] \
             Diagnosis: gstn pipeline diagnosis --project 7 --pipeline 9"
        );
    }

    #[test]
    fn a_stored_mark_is_read_as_the_same_mark() {
        assert_eq!(pipeline_mark(StoredMark::Open), CopyMark::Open);
        assert_eq!(
            pipeline_mark(StoredMark::Confidential),
            CopyMark::Confidential
        );
        assert_eq!(pipeline_mark(StoredMark::Undecided), CopyMark::Undecided);
    }
}
