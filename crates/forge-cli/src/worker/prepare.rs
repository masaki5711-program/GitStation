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
//! Steps 3 and 4 run for a version this job added; for a head already
//! recorded (a later attempt, a reopen, a head pushed back) only the steps
//! whose run the head lacks. **Failures**: only step 1 fails the job
//! (retried with the queue's backoff, dead at once for a malformed payload
//! or a broken invariant — the codes and rules of the version job, the
//! error carrying `step=refresh`), and the read of the head before steps
//! 3 and 4 (`step=checks`). A failed step 2, 3 or 4 is handed over to its
//! own job (`merge_request.mergeability`, `check.risk`,
//! `check.spec_index`), which retries only that step and answers `error`
//! on its last attempt, as for any version (warn
//! `forge.review.prepare_step_failed`, with the step's code and `step`).
//! A risk or spec hand-over that queues no job — the key is held by a
//! finished job, or the database refused — answers the step `error` at
//! once (error `forge.review.prepare_hand_over_failed`): no check is left
//! without an answer. A waiting mergeability job that cannot be settled is
//! logged (warn `forge.review.prepare_backstop_unsettled`) and runs after
//! its delay. A prepare job that dies after recording its version is
//! answered by the worker's `prepare_sweep`.
//! One info line per run (`forge.review.prepared`) says what each step did
//! and how long it took.

use std::sync::Arc;
use std::time::Instant;

use forge_checks::risk::Risk;
use forge_checks::spec_checks::SpecChecks;
use forge_core::{MergeRequestState, ObjectId, RequestId};
use forge_db::checks::risk::RiskRequest;
use forge_db::checks::spec_index::SpecIndexRequest;
use forge_db::checks::HandOver;
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
    /// Not run (nothing to do for it, or its answer is there already).
    Skipped,
    /// Ran and wrote its answer.
    Done,
    /// Failed; its own job retries it.
    HandedOver,
    /// Failed, and no job of its own could take it over: answered `error`.
    AnsweredError,
}

impl Step {
    fn as_str(self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Done => "done",
            Self::HandedOver => "handed_over",
            Self::AnsweredError => "answered_error",
        }
    }
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Log a failed step its own job now retries.
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

/// Log a failed step whose hand-over queued no job (`why`: `key_held`, or
/// the code of the database failure): the step is answered `error` now,
/// never left without an answer.
fn not_handed_over(request_id: &RequestId, mr: i64, step: &'static str, e: &ForgeError, why: &str) {
    tracing::error!(
        target: "forge_cli",
        event = "forge.review.prepare_hand_over_failed",
        request_id = %request_id,
        error_code = e.code().id(),
        job_kind = PREPARE_JOB,
        step,
        merge_request_id = mr,
        reason = why,
        error = %e,
        "a failed step of the prepare job could not be handed over; it is answered error"
    );
}

/// Settle the waiting mergeability job; a failure is logged, never the
/// job's (the waiting job then runs after its delay).
async fn settle(
    db: &Db,
    (mr, seq): (forge_core::MergeRequestId, i64),
    how: Backstop,
    request_id: &RequestId,
) {
    if let Err(e) = settle_backstop(db, mr, seq, how).await {
        tracing::warn!(
            target: "forge_cli",
            event = "forge.review.prepare_backstop_unsettled",
            request_id = %request_id,
            error_code = e.code().id(),
            merge_request_id = mr.get(),
            check_seq = seq,
            settle = ?how,
            "the waiting mergeability job could not be settled; it runs after its delay"
        );
    }
}

/// What becomes of a failed step after its hand-over: a job retries it
/// (`HandedOver`), or none could be queued — its key is held by a
/// finished job (the head was evaluated before under that key), or the
/// database refused — and the caller answers it `error` (`AnsweredError`).
fn after_hand_over(
    handed: Result<HandOver, ForgeError>,
    request_id: &RequestId,
    mr: i64,
    step: &'static str,
    e: &ForgeError,
) -> Step {
    match handed {
        Ok(HandOver::Queued | HandOver::Pending) => {
            handed_over(request_id, mr, step, e);
            Step::HandedOver
        }
        Ok(HandOver::Held) => {
            not_handed_over(request_id, mr, step, e, "key_held");
            Step::AnsweredError
        }
        Err(db) => {
            not_handed_over(request_id, mr, step, e, db.code().id());
            Step::AnsweredError
        }
    }
}

impl Inner {
    /// Step 2 for the request the refresh step made.
    async fn mergeability(
        &self,
        report: &RefreshReport,
        mr: forge_core::MergeRequestId,
        request_id: &RequestId,
    ) -> Step {
        let Some(seq) = report.check_requested else {
            return Step::Skipped;
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
                settle(&self.db, (mr, seq), Backstop::Drop, request_id).await;
                Step::Done
            }
            Err(e) => {
                handed_over(request_id, mr.get(), "mergeability", &e);
                settle(&self.db, (mr, seq), Backstop::Wake, request_id).await;
                Step::HandedOver
            }
        }
    }

    /// Whether `head` already has forge's run `name` (a step whose answer
    /// is there is not run again).
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

    /// Step 3 (`forge/risk`); its failure is handed over or answered.
    async fn risk_step(
        &self,
        request: &RiskRequest,
        mr: forge_core::MergeRequestId,
        request_id: &RequestId,
    ) -> Step {
        let Err(e) = self.risk.run(request, request_id).await else {
            return Step::Done;
        };
        // clone: the job keeps its origin request
        let handed =
            forge_db::checks::risk::hand_over(&self.db, request, mr, request_id.clone()).await;
        match after_hand_over(handed, request_id, mr.get(), "risk", &e) {
            Step::AnsweredError => {
                self.risk.fail(request, request_id, &e).await;
                Step::AnsweredError
            }
            other @ (Step::Skipped | Step::Done | Step::HandedOver) => other,
        }
    }

    /// Step 4 (the spec checks); its failure is handed over or answered.
    async fn spec_step(
        &self,
        request: &SpecIndexRequest,
        mr: forge_core::MergeRequestId,
        request_id: &RequestId,
    ) -> Step {
        let Err(e) = self.spec.run(request, request_id).await else {
            return Step::Done;
        };
        // clone: the job keeps its origin request
        let handed =
            forge_db::checks::spec_index::hand_over(&self.db, request, request_id.clone()).await;
        match after_hand_over(handed, request_id, mr.get(), "spec", &e) {
            Step::AnsweredError => {
                self.spec.fail(request, request_id, &e).await;
                Step::AnsweredError
            }
            other @ (Step::Skipped | Step::Done | Step::HandedOver) => other,
        }
    }

    /// Steps 3 and 4 on the merge request's current head; with
    /// `skip_answered`, a step whose run the head has is not run again.
    async fn checks(
        &self,
        mr: forge_core::MergeRequestId,
        skip_answered: bool,
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
        let risk = if skip_answered
            && self
                .answered(project, &head, forge_core::checks::RISK_CHECK)
                .await?
        {
            Step::Skipped
        } else {
            self.risk_step(&risk_request, mr, request_id).await
        };
        let spec_request = SpecIndexRequest {
            project,
            iid: stored.iid,
            head,
        };
        let spec = if skip_answered
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
            self.spec_step(&spec_request, mr, request_id).await
        };
        Ok((risk, spec))
    }

    /// One run (module docs). `Err`: the job's failure (step 1, or the
    /// read of the head before steps 3 and 4).
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
        let mergeability = self.mergeability(&report, mr, &request_id).await;
        let mergeability_ms = millis(step);
        let step = Instant::now();
        // A version this job added is evaluated; any other outcome on an
        // open merge request (the head was recorded already — a later
        // attempt, a reopen, a head pushed back) evaluates only what the
        // head still lacks (S6 review).
        let (risk, spec) = match report.outcome {
            "added" => self.checks(mr, false, &request_id).await,
            "already_head" | "restored" => self.checks(mr, true, &request_id).await,
            _ => Ok((Step::Skipped, Step::Skipped)),
        }
        .map_err(|e| failed(e.with_context("step", "checks")))?;
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
