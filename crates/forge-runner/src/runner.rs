//! The runner's loop (M4 C4): claim a job (long-poll), heartbeat while it
//! runs (a cancel or a lost lease raises the job's stop flag), run it
//! ([`Executor::execute`]), report the result. One job at a time; the next
//! claim only after the job directory was removed.
//!
//! A shutdown (SIGTERM / Ctrl-C: `forge-runner`'s `main` raises
//! [`Runner::shutdown`]) stops the running job like a cancel — its
//! container is killed and removed, its directory wiped, the result
//! reported — and the loop claims nothing more.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_errors::{ErrorCode, ForgeError};

use crate::api::{Beat, ClaimedJob, RunnerApi};
use crate::job::{Executor, Outcome};

/// The claim's wait (the server's long-poll maximum).
pub const CLAIM_WAIT_S: u64 = 20;
/// How often a running job's lease is extended (the lease is 120 s).
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
/// The pause after a failed claim (the server or the network is down).
pub const CLAIM_BACKOFF: Duration = Duration::from_secs(5);

/// What one claim did.
#[derive(Debug)]
pub enum Handled {
    /// Nothing came within the wait.
    Idle,
    /// A job ran (`reported`: the result reached forge).
    Ran {
        job_id: i64,
        outcome: Outcome,
        reported: bool,
    },
}

pub struct Runner {
    pub api: Arc<dyn RunnerApi>,
    pub exec: Executor,
    pub heartbeat_every: Duration,
    /// Raised by the signal handler: stop the running job, claim no more.
    pub shutdown: Arc<AtomicBool>,
}

fn sleep_unless(flag: &AtomicBool, total: Duration) {
    let until = Instant::now() + total;
    while !flag.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

impl Runner {
    /// Claim at most one job and run it.
    pub fn run_once(&self, wait_s: u64) -> Result<Handled, ForgeError> {
        let claimed = self.api.request_job(wait_s).map_err(|e| {
            // The server's code (FG-AUTH-0001: a disabled runner or a stale
            // token) or the client's (FG-CLI-0003: unreachable).
            ForgeError::new(ErrorCode::parse(e.code_id()).unwrap_or(ErrorCode::Internal))
                .with_context("operation", "claim")
                .with_context("request_id", e.request_id().unwrap_or("").to_owned())
        })?;
        let Some(job) = claimed else {
            return Ok(Handled::Idle);
        };
        tracing::info!(
            target: "forge_runner",
            event = "forge.runner.job_started",
            job_id = job.job_id,
            attempt = job.attempt,
            pipeline_id = job.pipeline_id,
            project_id = job.project_id,
            "job claimed"
        );
        Ok(self.handle(job))
    }

    fn handle(&self, job: ClaimedJob) -> Handled {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let beat = {
            let api = Arc::clone(&self.api);
            let (stop, done, lost) = (Arc::clone(&stop), Arc::clone(&done), Arc::clone(&lost));
            let (job_id, attempt, every) = (job.job_id, job.attempt, self.heartbeat_every);
            std::thread::spawn(move || loop {
                sleep_unless(&done, every);
                if done.load(Ordering::SeqCst) {
                    return;
                }
                match api.heartbeat(job_id, attempt) {
                    Ok(Beat::Running) => {}
                    Ok(Beat::Canceled) => stop.store(true, Ordering::SeqCst),
                    Err(e) => {
                        let gone = e.code_id() == ErrorCode::JobLeaseLost.id()
                            || e.code_id() == ErrorCode::Forbidden.id();
                        tracing::warn!(
                            target: "forge_runner",
                            event = "forge.runner.heartbeat_failed",
                            error_code = e.code_id(),
                            request_id = e.request_id().unwrap_or(""),
                            job_id,
                            attempt,
                            stops_the_job = gone,
                            "heartbeat refused or not delivered"
                        );
                        if gone {
                            lost.store(true, Ordering::SeqCst);
                            stop.store(true, Ordering::SeqCst);
                        }
                    }
                }
            })
        };
        // A shutdown stops the job at once (the heartbeat may be 30 s away).
        let watch = {
            let (stop, done, shutdown) = (
                Arc::clone(&stop),
                Arc::clone(&done),
                Arc::clone(&self.shutdown),
            );
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if shutdown.load(Ordering::SeqCst) {
                        stop.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        let outcome = self.exec.execute(&job, self.api.as_ref(), &stop);
        done.store(true, Ordering::SeqCst);
        if watch.join().is_err() {
            tracing::error!(
                target: "forge_runner",
                event = "forge.runner.shutdown_watch_panicked",
                error_code = ErrorCode::Internal.id(),
                job_id = job.job_id,
                "the shutdown watch thread panicked"
            );
        }
        if beat.join().is_err() {
            tracing::error!(
                target: "forge_runner",
                event = "forge.runner.heartbeat_panicked",
                error_code = ErrorCode::Internal.id(),
                job_id = job.job_id,
                "the heartbeat thread panicked"
            );
        }
        let reported = if lost.load(Ordering::SeqCst) {
            // The lease is gone: forge answers 409 to anything about this
            // attempt, and the job runs again elsewhere.
            false
        } else {
            match self
                .api
                .complete(job.job_id, job.attempt, outcome.result, outcome.exit_code)
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        target: "forge_runner",
                        event = "forge.runner.complete_failed",
                        error_code = e.code_id(),
                        request_id = e.request_id().unwrap_or(""),
                        job_id = job.job_id,
                        attempt = job.attempt,
                        "the result was not accepted; the lease reaper takes the job back"
                    );
                    false
                }
            }
        };
        tracing::info!(
            target: "forge_runner",
            event = "forge.runner.job_finished",
            job_id = job.job_id,
            attempt = job.attempt,
            result = outcome.result.as_str(),
            exit_code = outcome.exit_code.unwrap_or(-1),
            error_code = outcome.error.as_ref().map_or("", |e| e.code().id()),
            reported,
            "job finished"
        );
        Handled::Ran {
            job_id: job.job_id,
            outcome,
            reported,
        }
    }

    /// Claim and run jobs until [`Runner::shutdown`] (a running job is
    /// stopped; checked again between claims) or the job directory could
    /// not be cleaned (then an error: a host problem).
    pub fn run_forever(&self) -> Result<(), ForgeError> {
        let shutdown = self.shutdown.as_ref();
        while !shutdown.load(Ordering::SeqCst) {
            match self.run_once(CLAIM_WAIT_S) {
                Ok(Handled::Idle) => {}
                Ok(Handled::Ran { outcome, .. }) if outcome.workspace_dirty => {
                    return Err(ForgeError::new(ErrorCode::ContainerStartFailed)
                        .with_context("reason", "workspace_dirty"));
                }
                Ok(Handled::Ran { .. }) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "forge_runner",
                        event = "forge.runner.claim_failed",
                        error_code = e.code().id(),
                        context = ?e.context(),
                        "claim failed; retrying"
                    );
                    sleep_unless(shutdown, CLAIM_BACKOFF);
                }
            }
        }
        Ok(())
    }
}
