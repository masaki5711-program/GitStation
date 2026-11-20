//! The `merge_request.prepare` job handler (plan 18 S6, ADR-0037 (f);
//! `forge_db::review::prepare`): one claim per merge request version
//! instead of four. It runs, in order, the steps of the jobs it replaces —
//! each through the same function those jobs call, so it writes the same
//! rows, check runs and logs:
//!
//! 1. **refresh** — [`forge_review::refresh_prepared`]: the version, its
//!    hidden refs, its bases, the mergeability request (whose job waits
//!    behind this one) and the conflict scan's request (still its own
//!    low-priority job, off the merge path).
//! 2. **mergeability** — [`forge_review::check_asked`] on the request the
//!    refresh step made, then the waiting job is deleted; a failure wakes
//!    it instead, and it retries (and ends the request) as always.
//! 3. **risk** — `forge/risk` ([`forge_checks::risk::Risk::run`]).
//! 4. **spec** — `forge/spec-lint`, `forge/spec-trace`, `forge/doc-lint`,
//!    `forge/ci-lint` ([`forge_checks::spec_checks::SpecChecks::run`]).
//!
//! The merge request pipeline is decided inside step 2: the merge result's
//! pipeline is enqueued as plan 18 S5 says, and only when the merged tree
//! has a `.forge-ci.yml` (`Mergeability::with_ci_definition_probe`).
//!
//! Steps 3 and 4 run for a version this job added (or, on a later
//! attempt, for the head it already recorded, unless the head's run is
//! there). **Failures**: only step 1 fails the job (retried with the
//! queue's backoff, dead at once for a malformed payload or a broken
//! invariant — the codes and rules of the version job, the error carrying
//! `step=refresh`). A failed step 2, 3 or 4 is handed over to its own job
//! (`merge_request.mergeability`, `check.risk`, `check.spec_index`), which
//! retries only that step and answers `error` on its last attempt, as for
//! any version; the hand-over is logged once at warn
//! (`forge.review.prepare_step_failed`, with the step's code and `step`).
//! One info line per run (`forge.review.prepared`) says what each step did
//! and how long it took.

use std::sync::Arc;
use std::time::Instant;

use forge_checks::risk::Risk;
use forge_checks::spec_checks::SpecChecks;
use forge_core::{MergeRequestState, ObjectId, RequestId};
use forge_db::checks::risk::RiskRequest;
use forge_db::checks::spec_index::SpecIndexRequest;
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::review::{
    pipeline_asked, refresh_payload, settle_backstop, Backstop, MergeabilityRequest, PREPARE_JOB,
};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{Mergeability, RefreshReport, VersionRefs};

use super::provision::Storage;

/// The handler of `merge_request.prepare`.
pub(crate) struct PrepareHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    refs: VersionRefs,
    checker: Mergeability,
    risk: Risk,
    spec: SpecChecks,
}

impl PrepareHandler {
    pub(crate) fn new(db: &Db, storage: &Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(PREPARE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", PREPARE_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: pool handles
                // clone: the handler owns its git and storage handles
                refs: VersionRefs::new(storage.git.clone(), storage.root.clone()),
                checker: super::review::checker(storage),
                risk: super::risk::risk(db, storage),
                spec: super::spec_index::spec_checks(db, storage),
            }),
        })
    }
}

/// What one step did, for the run's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Not run (nothing to do for it).
    Skipped,
    /// Ran and wrote its answer.
    Done,
    /// Failed; its own job retries it.
    HandedOver,
}

impl Step {
    fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Done => "done",
            Self::HandedOver => "handed_over",
        }
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Log a step's failure that its own job now retries.
fn handed_over(request_id: &RequestId, mr: i64, step: &'static str, e: &ForgeError) {
    tracing::warn!(
        target: "forge_cli",
        event = "forge.review.prepare_step_failed",
        request_id = %request_id,
        error_code = e.code().id(),
        job_kind = PREPARE_JOB,
        step,
        merge_request_id = mr,
        error = %e,
        "a step of the prepare job failed; its own job retries it"
    );
}

impl Inner {
    /// Step 2 for the request the refresh step made.
    async fn mergeability(
        &self,
        report: &RefreshReport,
        mr: forge_core::MergeRequestId,
        request_id: &RequestId,
    ) -> Result<Step, ForgeError> {
        let Some(seq) = report.check_requested else {
            return Ok(Step::Skipped);
        };
        let request = MergeabilityRequest {
            merge_request: mr,
            check_seq: seq,
        };
        let ran = forge_review::check_asked(
            &self.db,
            &self.checker,
            request,
            report.check_asked,
            request_id,
        )
        .await;
        match ran {
            Ok(_) => {
                settle_backstop(&self.db, mr, seq, Backstop::Drop).await?;
                Ok(Step::Done)
            }
            Err(e) => {
                handed_over(request_id, mr.get(), "mergeability", &e);
                settle_backstop(&self.db, mr, seq, Backstop::Wake).await?;
                Ok(Step::HandedOver)
            }
        }
    }

    /// Whether `head` already has forge's run `name` (a later attempt
    /// skips a step whose answer is there).
    async fn answered(
        &self,
        project: forge_core::ProjectId,
        head: &ObjectId,
        name: &str,
    ) -> Result<bool, ForgeError> {
        Ok(forge_db::checks::check_runs(&self.db, project, head)
            .await?
            .iter()
            .any(|r| r.name.as_str() == name && r.reported_by.is_none()))
    }

    /// Steps 3 and 4 on the merge request's current head.
    async fn checks(
        &self,
        mr: forge_core::MergeRequestId,
        later_attempt: bool,
        request_id: &RequestId,
    ) -> Result<(Step, Step), ForgeError> {
        let Some(now) = forge_db::review::refresh_target(&self.db, mr).await? else {
            return Ok((Step::Skipped, Step::Skipped));
        };
        let stored = &now.merge_request;
        let head = match (&stored.state, stored.head_sha.as_deref()) {
            (MergeRequestState::Open, Some(raw)) => ObjectId::parse(raw)?,
            _ => return Ok((Step::Skipped, Step::Skipped)),
        };
        let project = stored.project_id;
        let risk_request = RiskRequest {
            project,
            head: head.clone(), // clone: the spec request owns it too
        };
        let risk = if later_attempt
            && self
                .answered(project, &head, forge_core::checks::RISK_CHECK)
                .await?
        {
            Step::Skipped
        } else {
            match self.risk.run(&risk_request, request_id).await {
                Ok(_) => Step::Done,
                Err(e) => {
                    handed_over(request_id, mr.get(), "risk", &e);
                    // clone: the job keeps its origin request
                    forge_db::checks::risk::hand_over(
                        &self.db,
                        &risk_request,
                        mr,
                        request_id.clone(),
                    )
                    .await?;
                    Step::HandedOver
                }
            }
        };
        let spec_request = SpecIndexRequest {
            project,
            iid: stored.iid,
            head,
        };
        let spec = if later_attempt
            && self
                .answered(
                    project,
                    &spec_request.head,
                    forge_core::checks::SPEC_LINT_CHECK,
                )
                .await?
        {
            Step::Skipped
        } else {
            match self.spec.run(&spec_request, request_id).await {
                Ok(_) => Step::Done,
                Err(e) => {
                    handed_over(request_id, mr.get(), "spec", &e);
                    forge_db::checks::spec_index::hand_over(
                        &self.db,
                        &spec_request,
                        request_id.clone(), // clone: the job keeps its origin request
                    )
                    .await?;
                    Step::HandedOver
                }
            }
        };
        Ok((risk, spec))
    }

    /// One run (module docs). `Err`: the job's failure (step 1, or a step
    /// that could not be handed over).
    async fn run(&self, job: &ClaimedJob) -> Result<(), JobFailure> {
        let request = refresh_payload(&job.payload).map_err(JobFailure::permanent)?;
        let ask = pipeline_asked(&job.payload).map_err(JobFailure::permanent)?;
        let request_id = job.request_id();
        let mr = request.merge_request;
        let started = Instant::now();
        let failed = |e: ForgeError| {
            if super::review::retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            }
        };
        let report =
            forge_review::refresh_prepared(&self.db, &self.refs, request, ask, &request_id)
                .await
                .map_err(|e| failed(e.with_context("step", "refresh")))?;
        let refresh_ms = millis(started);
        let step = Instant::now();
        let mergeability = self
            .mergeability(&report, mr, &request_id)
            .await
            .map_err(|e| failed(e.with_context("step", "mergeability")))?;
        let mergeability_ms = millis(step);
        let step = Instant::now();
        let later_attempt = job.lease.attempt > 1;
        let (risk, spec) =
            if report.outcome == "added" || (later_attempt && report.outcome == "already_head") {
                self.checks(mr, later_attempt, &request_id)
                    .await
                    .map_err(|e| failed(e.with_context("step", "checks")))?
            } else {
                (Step::Skipped, Step::Skipped)
            };
        tracing::info!(
            target: "forge_cli",
            event = "forge.review.prepared",
            request_id = %request_id,
            merge_request_id = mr.get(),
            attempt = job.lease.attempt,
            outcome = report.outcome,
            version = report.version,
            check_seq = report.check_requested,
            mergeability = mergeability.as_str(),
            risk = risk.as_str(),
            spec = spec.as_str(),
            refresh_ms,
            mergeability_ms,
            checks_ms = millis(step),
            duration_ms = millis(started),
            "merge request version prepared"
        );
        Ok(())
    }
}

impl JobHandler for PrepareHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.run(&job).await })
    }
}

#[cfg(test)]
#[path = "prepare_tests.rs"]
mod tests;
