//! The runner API's commands (M4 C3, plan 14 §5 C3, ADR-0029 §1): the
//! administration of runners on an administrator's proof, the runner's own
//! calls on its proof (claim with a bounded long-poll, heartbeat, log,
//! completion), and the worker's lease reaper and `FG-CI-0005` check. The
//! SQL is `forge_db::ci_runners`.
//!
//! The runner is always the proof's principal (`Credential::Runner`),
//! never a value from the request: a runner can only see and change the
//! jobs leased to it (the decision on the loaded lease, then the statement
//! re-checks the lease and the attempt).

use std::time::{Duration, Instant};

use forge_auth::{Action, Authorized, Credential, InstanceCtx, Principal, RunnerJobCtx};
use forge_core::RequestId;
use forge_db::ci_runners::lease::{self, Appended, Claimed, Completed, Heartbeat, JobResult};
use forge_db::ci_runners::{self as db, NewRunner, Registered, RunnerUpdate, StoredRunner};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use crate::secrets::ReleasedSecret;
use crate::{caller, change_context, require, ChangeRequest, Pipelines};

pub use forge_db::ci_runners::lease::JobCopy;
pub use forge_db::ci_runners::lease::{
    Appended as LogAppended, Claimed as ClaimedJob, Completed as JobCompleted,
    Heartbeat as JobHeartbeat, JobResult as RunnerJobResult,
};
pub use forge_db::ci_runners::lease::{
    LEASE_S, MAX_LOG_BYTES, MAX_LOG_CHUNK, MAX_LOG_CHUNKS, NO_RUNNER_AFTER_S,
};
pub use forge_db::ci_runners::{
    valid_name, NewRunner as RunnerRegistration, Registered as RegisteredRunner,
    RunnerUpdate as RunnerChangeRequest, StoredRunner as Runner, MAX_DESCRIPTION, MAX_TAGS,
};

/// A claimed job and the secrets released to it (M4 C6: empty unless the
/// job asked for some and its pipeline may have them).
#[derive(Debug)]
pub struct ClaimGrant {
    pub job: Claimed,
    pub secrets: Vec<ReleasedSecret>,
}

/// The longest a claim waits for a job (well below the 30 s request
/// timeout `forge serve` never goes under: test
/// `the_long_poll_ends_before_the_request_timeout`).
pub const LONG_POLL_MAX: Duration = Duration::from_secs(20);
/// How often a waiting claim looks again.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// `FG-CI-0005` is logged at most this often.
pub const NO_RUNNER_LOG_EVERY: Duration = Duration::from_secs(600);

/// The runner a proof was decided for.
fn runner_of(principal: &Principal, action: Action) -> Result<i64, ForgeError> {
    match principal.credential() {
        Credential::Runner(c) => Ok(c.runner_id),
        Credential::Session | Credential::Token(_) | Credential::ActAs(_) | Credential::Job(_) => {
            Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "runner_proof_without_runner")
                .with_context("action", action.name()))
        }
    }
}

impl Pipelines {
    /// `POST /api/v1/admin/runners` (`RunnerRegister`): the runner and its
    /// token (shown once), one `ci_runner.registered` audit row.
    pub async fn register_runner(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<InstanceCtx>,
        new: &NewRunner,
    ) -> Result<Registered, ForgeError> {
        require(proof.action(), Action::RunnerRegister)?;
        let user = caller(proof.principal(), Action::RunnerRegister)?;
        let ctx = change_context(request, proof.principal(), user);
        let registered = db::register(&self.db, &ctx, new).await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.runner_registered",
            request_id = %request.request_id,
            runner_id = registered.runner.id,
            name = %registered.runner.name,
            user_id = user.get(),
            "runner registered"
        );
        Ok(registered)
    }

    /// `PATCH /api/v1/admin/runners/{id}` (`RunnerManage`); `None`: no such
    /// runner.
    pub async fn update_runner(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<InstanceCtx>,
        id: i64,
        change: &RunnerUpdate,
    ) -> Result<Option<StoredRunner>, ForgeError> {
        require(proof.action(), Action::RunnerManage)?;
        let user = caller(proof.principal(), Action::RunnerManage)?;
        let ctx = change_context(request, proof.principal(), user);
        let updated = db::update(&self.db, &ctx, id, change).await?;
        if let Some(runner) = &updated {
            tracing::info!(
                target: "forge_pipelines",
                event = "forge.ci.runner_updated",
                request_id = %request.request_id,
                runner_id = runner.id,
                enabled = runner.enabled,
                user_id = user.get(),
                "runner updated"
            );
        }
        Ok(updated)
    }

    /// `GET /api/v1/admin/runners` (`RunnerManage`).
    pub async fn runners(
        &self,
        proof: &Authorized<InstanceCtx>,
    ) -> Result<Vec<StoredRunner>, ForgeError> {
        require(proof.action(), Action::RunnerManage)?;
        db::list(&self.db).await
    }

    /// One runner (`RunnerManage`, read back after a change).
    pub async fn runner(
        &self,
        proof: &Authorized<InstanceCtx>,
        id: i64,
    ) -> Result<Option<StoredRunner>, ForgeError> {
        require(proof.action(), Action::RunnerManage)?;
        db::get(&self.db, id).await
    }

    /// The lease the runner routes' loader decides on: `(holder,
    /// finished)`, `None` without such a job.
    pub async fn lease_of(&self, job: i64) -> Result<Option<(Option<i64>, bool)>, ForgeError> {
        lease::lease_of(&self.db, job).await
    }

    /// What the job fetch route (M4 C4) decides on: the job's pipeline, its
    /// project and pinned SHA, and the project's repository; `None` without
    /// such a job. The decision is `authorize()`'s on the route's context.
    pub async fn job_copy(&self, job: i64) -> Result<Option<JobCopy>, ForgeError> {
        lease::job_copy(&self.db, job).await
    }

    /// `POST /api/v1/runner/jobs/request` (`RunnerJobRequest`): claim a
    /// job, waiting up to `wait` (at most [`LONG_POLL_MAX`]) for one, with
    /// the secrets it may have (M4 C6, `crate::secrets`). A job whose
    /// secrets may not or cannot be released is failed with the code and
    /// logged once here; the claim goes on with the next job. `None`:
    /// nothing came (the runner asks again).
    pub async fn request_job(
        &self,
        request_id: &RequestId,
        proof: &Authorized<InstanceCtx>,
        wait: Duration,
    ) -> Result<Option<ClaimGrant>, ForgeError> {
        require(proof.action(), Action::RunnerJobRequest)?;
        let runner = runner_of(proof.principal(), Action::RunnerJobRequest)?;
        let deadline = Instant::now() + wait.min(LONG_POLL_MAX);
        loop {
            if let Some(claimed) = lease::claim(&self.db, runner).await? {
                let secrets = match self.release_secrets(&claimed).await {
                    Ok(secrets) => secrets,
                    Err(e) => {
                        self.refuse_claimed(request_id, runner, &claimed, e).await?;
                        continue;
                    }
                };
                tracing::info!(
                    target: "forge_pipelines",
                    event = "forge.ci.job_claimed",
                    request_id = %request_id,
                    runner_id = runner,
                    job_id = claimed.job_id,
                    attempt = claimed.attempt,
                    pipeline_id = claimed.pipeline.id,
                    project_id = claimed.pipeline.project.get(),
                    secrets_allowed = claimed.secrets_allowed,
                    secrets_released = secrets.len(),
                    "job claimed"
                );
                return Ok(Some(ClaimGrant {
                    job: claimed,
                    secrets,
                }));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            tokio::time::sleep(left.min(POLL_INTERVAL)).await;
        }
    }

    /// `POST …/jobs/{job_id}/heartbeat` (`RunnerJobHeartbeat`).
    pub async fn heartbeat(
        &self,
        proof: &Authorized<RunnerJobCtx>,
        attempt: i32,
    ) -> Result<Heartbeat, ForgeError> {
        require(proof.action(), Action::RunnerJobHeartbeat)?;
        let runner = runner_of(proof.principal(), Action::RunnerJobHeartbeat)?;
        lease::heartbeat(&self.db, runner, *proof.id(), attempt).await
    }

    /// `POST …/jobs/{job_id}/log` (`RunnerLogAppend`): the chunk is
    /// stored with the job's masked secrets replaced (M4 C6,
    /// [`Pipelines::log_masks`]; `FG-CI-0016` stores nothing).
    pub async fn append_log(
        &self,
        proof: &Authorized<RunnerJobCtx>,
        attempt: i32,
        seq: i64,
        body: &str,
    ) -> Result<Appended, ForgeError> {
        require(proof.action(), Action::RunnerLogAppend)?;
        let runner = runner_of(proof.principal(), Action::RunnerLogAppend)?;
        let job = *proof.id();
        let forms = self
            .log_masks(job)
            .await
            .map_err(|e| e.with_context("job_id", job))?;
        let masked = forge_core::ci_secret::mask(body, &forms);
        lease::append_log(&self.db, runner, job, attempt, seq, &masked).await
    }

    /// Fail a claimed job whose secrets were refused (`e`: FG-CI-0016 /
    /// 0017 / 0018) and log it once; another error (the database's) is
    /// returned as it is.
    async fn refuse_claimed(
        &self,
        request_id: &RequestId,
        runner: i64,
        claimed: &Claimed,
        e: ForgeError,
    ) -> Result<(), ForgeError> {
        let code = e.code();
        let event = if code == ErrorCode::SecretKeyUnavailable {
            "forge.ci.secret_key_unavailable"
        } else if code == ErrorCode::SecretOnUnprotectedRef {
            "forge.ci.secret_on_unprotected_ref"
        } else if code == ErrorCode::SecretNotDefined {
            "forge.ci.secret_not_defined"
        } else {
            return Err(e);
        };
        lease::fail_claimed(&self.db, runner, claimed.job_id, claimed.attempt, code).await?;
        let e = e
            .with_context("job_id", claimed.job_id)
            .with_context("pipeline_id", claimed.pipeline.id)
            .with_context("project_id", claimed.pipeline.project.get())
            .with_context("runner_id", runner);
        let context = e
            .context()
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        // FG-CI-0016 is the alert (error); 0017 / 0018 are the definition's (info).
        if code == ErrorCode::SecretKeyUnavailable {
            tracing::error!(
                target: "forge_pipelines",
                event,
                request_id = %request_id,
                error_code = code.id(),
                job_id = claimed.job_id,
                pipeline_id = claimed.pipeline.id,
                context = %context,
                "a job's secrets were not released; the job was failed"
            );
        } else {
            tracing::info!(
                target: "forge_pipelines",
                event,
                request_id = %request_id,
                error_code = code.id(),
                job_id = claimed.job_id,
                pipeline_id = claimed.pipeline.id,
                context = %context,
                "a job's secrets were not released; the job was failed"
            );
        }
        Ok(())
    }

    /// `POST …/jobs/{job_id}/complete` (`RunnerJobComplete`).
    pub async fn complete(
        &self,
        request_id: &RequestId,
        proof: &Authorized<RunnerJobCtx>,
        attempt: i32,
        result: JobResult,
        exit_code: Option<i32>,
    ) -> Result<Completed, ForgeError> {
        require(proof.action(), Action::RunnerJobComplete)?;
        let runner = runner_of(proof.principal(), Action::RunnerJobComplete)?;
        let done = lease::complete(&self.db, runner, *proof.id(), attempt, result).await?;
        tracing::info!(
            target: "forge_pipelines",
            event = "forge.ci.job_completed",
            request_id = %request_id,
            runner_id = runner,
            job_id = done.job_id,
            attempt,
            pipeline_id = done.pipeline_id,
            project_id = done.project_id,
            status = done.status,
            exit_code = exit_code.map_or_else(|| "none".to_owned(), |c| c.to_string()),
            pipeline_status = %done.pipeline_status,
            "job completed"
        );
        Ok(done)
    }
}

/// One reaper pass (the worker, every few seconds): each held job whose
/// lease ended is requeued, failed (`FG-CI-0004`) or released, and logged
/// once here as `forge.ci.job_lease_lost` (warn, `FG-CI-0004`). A job the
/// pass could not take back does not stop it: after the pass, the first
/// such failure is returned (the worker's supervisor logs it with
/// `error_code` and `job_id`) and every further one is logged here as
/// `forge.ci.job_reap_failed` (error, its `error_code`), so each is logged
/// once.
pub async fn reap_leases(db: &Db, request_id: &RequestId) -> Result<usize, ForgeError> {
    let lease::ReapPass { reaped, failed } = lease::reap(db).await?;
    for r in &reaped {
        tracing::warn!(
            target: "forge_pipelines",
            event = "forge.ci.job_lease_lost",
            request_id = %request_id,
            error_code = ErrorCode::JobLeaseLost.id(),
            job_id = r.job_id,
            pipeline_id = r.pipeline_id,
            project_id = r.project_id,
            runner_id = r.runner_id,
            lease_losses = r.lease_losses,
            outcome = r.outcome,
            "job lease lost"
        );
    }
    let mut failed = failed.into_iter();
    let Some((_, first)) = failed.next() else {
        return Ok(reaped.len());
    };
    for (job_id, e) in failed {
        tracing::error!(
            target: "forge_pipelines",
            event = "forge.ci.job_reap_failed",
            request_id = %request_id,
            error_code = e.code().id(),
            job_id,
            error = %e,
            "job lease not taken back"
        );
    }
    Err(first)
}

/// The worker's `FG-CI-0005` check: jobs waiting past
/// [`NO_RUNNER_AFTER_S`] are logged as `forge.ci.no_runner_available`
/// (error, the alert) when `last_logged` is older than
/// [`NO_RUNNER_LOG_EVERY`] (or never). Returns whether it logged.
pub async fn check_runners(
    db: &Db,
    request_id: &RequestId,
    last_logged: Option<Instant>,
) -> Result<bool, ForgeError> {
    let health = lease::health(db).await?;
    if !health.no_runner_available() {
        return Ok(false);
    }
    if last_logged.is_some_and(|at| at.elapsed() < NO_RUNNER_LOG_EVERY) {
        return Ok(false);
    }
    tracing::error!(
        target: "forge_pipelines",
        event = "forge.ci.no_runner_available",
        request_id = %request_id,
        error_code = ErrorCode::NoRunnerAvailable.id(),
        waiting = health.waiting,
        oldest_job_id = health.oldest_job_id.unwrap_or(0),
        oldest_wait_s = health.oldest_wait_s.unwrap_or(0),
        enabled_runners = health.enabled_runners,
        live_runners = health.live_runners,
        "no runner available"
    );
    Ok(true)
}
