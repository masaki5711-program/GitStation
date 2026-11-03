//! The repository maintenance of `forge worker` (M3 O3, plan 11 §5 O3,
//! ADR-0024 "実装で決めたこと（O3）"): its own job runner (the
//! `maintenance_runner` task, one job at a time, only `repository.maintain`
//! registered — a long gc never delays a provisioning, version or
//! mergeability job of the main runner) and the `maintenance_schedule` task
//! (every [`SCHEDULE_PERIOD`], inside the nightly window only: the due
//! repositories, oldest first, at most `maintenance.batch_max` live jobs).
//!
//! One run ([`Inner::maintain_repo`]), each git step under the run's
//! deadline (the job's timeout minus `forge_db::maintenance::RUN_RESERVE`)
//! and its own `maintenance.step_timeout_secs`, whichever ends first:
//!
//! 1. the ledger's `start` (a repository that is not `ready` → nothing);
//! 2. an incoming push quarantine → `skipped incoming_push` (again in 1 h);
//! 3. `fsck` — full when the last full one is older than
//!    `maintenance.fsck_interval_days` (or the last fsck failed),
//!    connectivity otherwise. A finding → `corrupt` (FG-GIT-0019, alert):
//!    nothing is pruned while a repository is corrupt;
//! 4. `gc --cruft --prune=<grace>` (another gc running → `skipped
//!    gc_running`, again in 1 h);
//! 5. `pack-refs` under the merge lock, try-lock only: a merge in progress
//!    skips this step (`skipped_reason = pack_refs_merge_lock_held`);
//! 6. `commit-graph write --reachable`;
//! 7. the counts (objects, pack size, refs) into the ledger.
//!
//! A step that fails is recorded (`failed`, its code; `failures` + 1,
//! again in 12 h) and the job completes: the ledger, not the dead-letter
//! queue, carries maintenance failures (doctor alerts after 2 in a row). A
//! ledger write that fails is retried by the queue.
//!
//! Logs (once, here, with the job's `request_id`):
//! `forge.git.maintenance_started` (info), `forge.git.maintenance_skipped`
//! (info, `reason`), `forge.git.maintenance_completed` (info on success
//! with the counts; warn with `error_code` on a failure, error from the
//! second failure in a row and on corruption).

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::config::MaintenanceConfig;
use forge_core::ids::RepoId;
use forge_core::RequestId;
use forge_db::jobs::{
    ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind, JobRunner, MaxAttempts, WorkerId,
    LEASE_SLACK,
};
use forge_db::maintenance::{
    self as ledger, Finish, Outcome, Started, MAINTAIN_JOB, MAINTAIN_JOB_TIMEOUT, RUN_RESERVE,
};
use forge_db::review::merges::{try_lock_merges, MERGE_JOB_TIMEOUT};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::maintenance::{Blocked, FsckMode, GcRun, LockCleanup, RepositoryCounts};
use forge_git::{Git, StorageRoot};

use super::provision::Storage;
use super::supervise::Next;

/// The schedule task's name in the supervisor's logs.
pub(crate) const SCHEDULE_NAME: &str = "maintenance_schedule";
/// The maintenance runner task's name.
pub(crate) const RUNNER_NAME: &str = "maintenance_runner";
/// How often the schedule looks for due repositories.
pub(crate) const SCHEDULE_PERIOD: Duration = Duration::from_secs(60);
/// A skipped run (a push in progress, another gc) is due again after this.
const SKIP_RETRY: Duration = Duration::from_secs(3600);
/// A failed run is due again after this (the next night's window).
const FAILURE_RETRY: Duration = Duration::from_secs(12 * 3600);
/// The consecutive failures that make a maintenance failure an alert.
const ALERT_AFTER_FAILURES: i32 = 2;

/// The merge reconciliation window plus the merge job's whole retry budget
/// (ADR-0024 §3, §4.2): the longest an unreferenced object a merge wrote
/// (`commit-tree`'s `new_sha` before its ref moved) may still be needed.
/// Every attempt's lease (timeout + slack), the backoff between attempts
/// (10 s × 2^(n−1), `jobs` module docs), then the reconciliation's
/// `executing` bound and its idle bound.
pub(crate) fn merge_window_and_budget() -> Duration {
    let attempts = u32::from(MaxAttempts::DEFAULT.get());
    let leases = MERGE_JOB_TIMEOUT
        .saturating_add(LEASE_SLACK)
        .saturating_mul(attempts);
    let backoff: Duration = (1..attempts)
        .map(|n| Duration::from_secs(10u64.saturating_mul(1u64 << (n - 1)).min(3600)))
        .sum();
    let merge = forge_review::merges::MergeTimeouts::default();
    leases
        .saturating_add(backoff)
        .saturating_add(merge.executing_after())
        .saturating_add(merge.idle)
}

/// The settings check at start (plan 11 §5 O3, decision Q2): the prune
/// grace must exceed the merge window and retry budget (it is ≥ 24 h by
/// the configuration's own range), and one step must fit the job's run.
/// `FG-SYS-0002` otherwise.
pub(crate) fn check_settings(cfg: &MaintenanceConfig) -> Result<(), ForgeError> {
    check_prune_grace(cfg.prune_grace, merge_window_and_budget())?;
    if cfg.step_timeout.saturating_add(RUN_RESERVE) > MAINTAIN_JOB_TIMEOUT {
        return Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "maintenance.step_timeout_secs")
            .with_context("reason", "above_job_timeout"));
    }
    Ok(())
}

pub(crate) fn check_prune_grace(grace: Duration, needed: Duration) -> Result<(), ForgeError> {
    if grace < forge_git::maintenance::MIN_PRUNE_GRACE {
        return Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "maintenance.prune_grace_hours")
            .with_context("reason", "below_min"));
    }
    if grace <= needed {
        return Err(ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "maintenance.prune_grace_hours")
            .with_context("reason", "below_merge_window")
            .with_context("needed_secs", needed.as_secs()));
    }
    Ok(())
}

/// The handler of `repository.maintain`.
pub(crate) struct MaintainHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    db: Db,
    root: StorageRoot,
    /// Its own git (own slot pool): a long gc never holds a slot of the
    /// other handlers' git.
    git: Git,
    cfg: MaintenanceConfig,
}

impl MaintainHandler {
    pub(crate) fn new(
        db: &Db,
        storage: &Storage,
        cfg: &MaintenanceConfig,
    ) -> Result<Self, ForgeError> {
        let git = Git::new(storage.git.config().clone()); // clone: same binary and home, own slots
        Self::with_git(db, storage.root.clone(), git, cfg) // clone: the handler's own root handle
    }

    pub(crate) fn with_git(
        db: &Db,
        root: StorageRoot,
        git: Git,
        cfg: &MaintenanceConfig,
    ) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(MAINTAIN_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", MAINTAIN_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: pool handles
                root,
                git,
                cfg: cfg.clone(), // clone: the handler owns its settings
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn inner(&self) -> Arc<Inner> {
        Arc::clone(&self.inner)
    }
}

/// The maintenance's own job runner (module docs).
pub(crate) fn runner(
    db: &Db,
    storage: &Storage,
    cfg: &MaintenanceConfig,
) -> Result<JobRunner, ForgeError> {
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(MaintainHandler::new(db, storage, cfg)?))?;
    Ok(runner)
}

impl JobHandler for MaintainHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let repo = ledger::maintain_payload(&job.payload).map_err(JobFailure::permanent)?;
            let budget = job.timeout.get().saturating_sub(RUN_RESERVE);
            inner
                .maintain_repo(repo, &job.request_id(), job.lease.attempt, budget)
                .await
        })
    }
}

/// The run's deadline: each step gets its own timeout, never past it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunDeadline {
    end: Instant,
    step: Duration,
}

impl RunDeadline {
    pub(crate) fn new(budget: Duration, step: Duration) -> Self {
        Self {
            end: Instant::now() + budget,
            step,
        }
    }

    /// The next step's timeout: its own, cut to the time left.
    /// `FG-GIT-0002 reason=run_deadline` once nothing is left.
    pub(crate) fn step(&self) -> Result<Duration, ForgeError> {
        let left = self.end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ForgeError::new(ErrorCode::GitTimeout)
                .with_context("kind", "deadline")
                .with_context("reason", "run_deadline"));
        }
        Ok(self.step.min(left))
    }
}

/// How a run ended before the ledger write.
enum RunEnd {
    Done {
        counts: RepositoryCounts,
        fsck: FsckMode,
        skipped_step: Option<&'static str>,
    },
    /// Nothing (more) done; the fsck that ran before the skip, if any.
    Skipped(&'static str, Option<FsckMode>),
    Failed {
        error: ForgeError,
        fsck: Option<(FsckMode, bool)>,
    },
}

fn db_failure(e: ForgeError) -> JobFailure {
    if matches!(
        e.code(),
        ErrorCode::DbUnavailable | ErrorCode::DbQueryFailed
    ) {
        JobFailure::retry(e)
    } else {
        JobFailure::permanent(e)
    }
}

/// The `context` field of a failure event (like `telemetry::log_failure`).
fn context_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn step_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "step")
        .map_or_else(String::new, |(_, v)| v.clone())
}

impl Inner {
    /// Which fsck this run does (module docs, step 3).
    fn fsck_mode(&self, started: &Started) -> FsckMode {
        let interval = i64::try_from(self.cfg.fsck_interval.as_secs()).unwrap_or(i64::MAX);
        let full_due = started.full_fsck_age_secs.is_none_or(|age| age >= interval);
        if full_due || started.fsck_ok == Some(false) {
            FsckMode::Full
        } else {
            FsckMode::Connectivity
        }
    }

    /// One maintenance run of `repo` (module docs).
    pub(crate) async fn maintain_repo(
        &self,
        repo: RepoId,
        request_id: &RequestId,
        attempt: i32,
        budget: Duration,
    ) -> Result<(), JobFailure> {
        let began = Instant::now();
        let Some(started) = ledger::start(&self.db, repo).await.map_err(db_failure)? else {
            tracing::info!(
                target: "forge_cli",
                event = "forge.git.maintenance_skipped",
                request_id = %request_id,
                repo_id = repo.get(),
                reason = "not_ready",
                "repository maintenance skipped"
            );
            return Ok(());
        };
        if started.lost_previous {
            // The run before this one ended without a ledger write (a crash,
            // a lease that expired): counted as a failure by `start`.
            tracing::warn!(
                target: "forge_cli",
                event = "forge.git.maintenance_run_lost",
                error_code = ErrorCode::JobLeaseExpired.id(),
                request_id = %request_id,
                repo_id = repo.get(),
                failures = started.failures,
                "the previous maintenance run of the repository ended without a record"
            );
        }
        let mode = self.fsck_mode(&started);
        tracing::info!(
            target: "forge_cli",
            event = "forge.git.maintenance_started",
            request_id = %request_id,
            repo_id = repo.get(),
            project_id = started.project_id.get(),
            fsck_mode = mode.as_str(),
            attempt,
            "repository maintenance started"
        );
        let deadline = RunDeadline::new(budget, self.cfg.step_timeout);
        let end = self.steps(repo, mode, &deadline, request_id).await;
        let duration_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        let finish = self.finish_of(&end);
        // The outcome is logged once the ledger holds it (review LOW): a
        // failed write is its own event and the queue retries the job.
        if let Err(e) = ledger::finish(&self.db, repo, &finish).await {
            tracing::warn!(
                target: "forge_cli",
                event = "forge.git.maintenance_ledger_failed",
                error_code = e.code().id(),
                request_id = %request_id,
                repo_id = repo.get(),
                outcome = finish.outcome.as_str(),
                run_error_code = finish.error_code.map_or("", ErrorCode::id),
                context = %context_of(&e),
                "the maintenance run's outcome could not be recorded; the job is retried"
            );
            return Err(db_failure(e));
        }
        log_end(&end, request_id, repo, started.failures, duration_ms);
        Ok(())
    }

    /// The ledger record of `end`.
    fn finish_of(&self, end: &RunEnd) -> Finish {
        match end {
            RunEnd::Done {
                counts,
                fsck,
                skipped_step,
            } => Finish {
                outcome: Outcome::Succeeded,
                error_code: None,
                skipped_reason: *skipped_step,
                counts: Some((counts.objects(), counts.size_pack_kib, counts.refs)),
                fsck: Some((fsck.as_str(), true)),
                next_due_in: self.cfg.interval,
            },
            RunEnd::Skipped(reason, fsck) => Finish {
                outcome: Outcome::Skipped,
                error_code: None,
                skipped_reason: Some(reason),
                counts: None,
                fsck: fsck.map(|m| (m.as_str(), true)),
                next_due_in: SKIP_RETRY,
            },
            RunEnd::Failed { error, fsck } => Finish {
                outcome: if error.code() == ErrorCode::RepositoryCorrupt {
                    Outcome::Corrupt
                } else {
                    Outcome::Failed
                },
                error_code: Some(error.code()),
                skipped_reason: None,
                counts: None,
                fsck: fsck.map(|(m, ok)| (m.as_str(), ok)),
                next_due_in: FAILURE_RETRY,
            },
        }
    }

    /// Steps 2–7 (module docs).
    async fn steps(
        &self,
        repo: RepoId,
        mode: FsckMode,
        deadline: &RunDeadline,
        request_id: &RequestId,
    ) -> RunEnd {
        let failed = |error, fsck| RunEnd::Failed { error, fsck };
        let (git, root) = (&self.git, &self.root);
        match git.maintenance_blocker(root, repo).await {
            Ok(None) => {}
            Ok(Some(blocked @ Blocked::IncomingPush { .. })) => {
                return RunEnd::Skipped(blocked.as_str(), None)
            }
            Err(e) => return failed(e, None),
        }
        let fsck = match deadline.step() {
            Ok(t) => git.fsck(root, repo, mode, t).await,
            Err(e) => Err(e),
        };
        match fsck {
            Ok(()) => {}
            Err(e) if e.code() == ErrorCode::RepositoryCorrupt => {
                return failed(e, Some((mode, false)))
            }
            Err(e) => return failed(e, None),
        }
        let ok = Some((mode, true));
        // fsck may have taken long: a push may have started meanwhile.
        match git.maintenance_blocker(root, repo).await {
            Ok(None) => {}
            Ok(Some(blocked @ Blocked::IncomingPush { .. })) => {
                return RunEnd::Skipped(blocked.as_str(), Some(mode))
            }
            Err(e) => return failed(e, ok),
        }
        let gc = match deadline.step() {
            Ok(t) => git.gc_objects(root, repo, self.cfg.prune_grace, t).await,
            Err(e) => Err(e),
        };
        match gc {
            Ok(GcRun::Ran) => {}
            Ok(GcRun::AlreadyRunning) => return RunEnd::Skipped("gc_running", Some(mode)),
            Err(e) => return failed(e, ok),
        }
        let skipped_step = match self.pack_refs(repo, deadline, request_id).await {
            Ok(skipped) => skipped,
            Err(e) => return failed(e, ok),
        };
        if let Err(e) = match deadline.step() {
            Ok(t) => git.write_commit_graph(root, repo, t).await,
            Err(e) => Err(e),
        } {
            return failed(e, ok);
        }
        let counts = match deadline.step() {
            Ok(t) => git.repository_counts(root, repo, t).await,
            Err(e) => Err(e),
        };
        match counts {
            Ok(counts) => RunEnd::Done {
                counts,
                fsck: mode,
                skipped_step,
            },
            Err(e) => failed(e, ok),
        }
    }

    /// Step 5: pack-refs under the merge lock (try-lock). `Ok(Some(reason))`
    /// when a merge holds it (the step is left out of this run).
    async fn pack_refs(
        &self,
        repo: RepoId,
        deadline: &RunDeadline,
        request_id: &RequestId,
    ) -> Result<Option<&'static str>, ForgeError> {
        let Some(lock) = try_lock_merges(&self.db, repo).await? else {
            return Ok(Some("pack_refs_merge_lock_held"));
        };
        let step_started = std::time::SystemTime::now();
        let packed = match deadline.step() {
            // Never past the lock's own hold (a merge waits at most that).
            Ok(t) => {
                self.git
                    .pack_refs(&self.root, repo, t.min(lock.left()))
                    .await
            }
            Err(e) => Err(e),
        };
        if packed.is_err() {
            // Still under the merge lock: a killed pack-refs may have left
            // its `packed-refs.lock`, which would fail every later ref
            // write that needs it (review HIGH).
            self.clean_pack_refs_lock(repo, step_started, request_id)
                .await;
        }
        lock.release().await;
        packed.map(|()| None)
    }
}

impl Inner {
    /// After a failed pack-refs, under the merge lock: remove our own
    /// `packed-refs.lock`; leave someone else's and alert (FG-GIT-0020).
    /// A failure to look is logged with its code (the run's own error is
    /// what the ledger records).
    async fn clean_pack_refs_lock(
        &self,
        repo: RepoId,
        step_started: std::time::SystemTime,
        request_id: &RequestId,
    ) {
        match self
            .git
            .clear_own_packed_refs_lock(&self.root, repo, step_started)
            .await
        {
            Ok(LockCleanup::Absent) => {}
            Ok(LockCleanup::Removed) => tracing::warn!(
                target: "forge_cli",
                event = "forge.git.maintenance_lock_removed",
                request_id = %request_id,
                repo_id = repo.get(),
                "removed the packed-refs.lock the failed pack-refs left"
            ),
            Ok(LockCleanup::Foreign { age_secs }) => tracing::error!(
                target: "forge_cli",
                event = "forge.git.maintenance_lock_left",
                error_code = ErrorCode::RepositoryRefLockStale.id(),
                request_id = %request_id,
                repo_id = repo.get(),
                age_secs,
                "{}",
                ErrorCode::RepositoryRefLockStale.entry().cause
            ),
            Err(e) => tracing::warn!(
                target: "forge_cli",
                event = "forge.git.maintenance_lock_check_failed",
                error_code = e.code().id(),
                request_id = %request_id,
                repo_id = repo.get(),
                context = %context_of(&e),
                "could not check packed-refs.lock after a failed pack-refs"
            ),
        }
    }
}

/// One run of the schedule (module docs): `Next::AfterPeriod` always (a
/// pass enqueues at most the free room; the runner drains it).
pub(crate) async fn run_schedule(db: &Db, cfg: &MaintenanceConfig) -> Result<Next, ForgeError> {
    // Every pass, inside the window or not: runs that are gone (crashed,
    // then dead-lettered) are recorded as failures and leave the queue
    // alone until their retry (review MEDIUM).
    for repo in ledger::reap_stale_running(db, FAILURE_RETRY).await? {
        tracing::warn!(
            target: "forge_cli",
            event = "forge.git.maintenance_run_lost",
            error_code = ErrorCode::JobLeaseExpired.id(),
            repo_id = repo.get(),
            "a maintenance run ended without a record; counted as a failure"
        );
    }
    let minute = ledger::local_minute(db, &cfg.time_zone).await?;
    if !cfg.window.contains(minute) {
        return Ok(Next::AfterPeriod);
    }
    let request_id = RequestId::generate();
    let scheduled = ledger::schedule_due(db, i64::from(cfg.batch_max), &request_id).await?;
    let queued = scheduled
        .iter()
        .filter(|s| matches!(s.enqueued, forge_db::jobs::Enqueued::Queued(_)))
        .count();
    if queued > 0 {
        tracing::info!(
            target: "forge_cli",
            event = "forge.git.maintenance_scheduled",
            request_id = %request_id,
            queued,
            waiting = scheduled.len() - queued,
            "repository maintenance scheduled"
        );
    }
    Ok(Next::AfterPeriod)
}

/// The one outcome event of a recorded run (module docs).
fn log_end(end: &RunEnd, request_id: &RequestId, repo: RepoId, before: i32, duration_ms: u64) {
    match end {
        RunEnd::Done {
            counts,
            fsck,
            skipped_step,
        } => tracing::info!(
            target: "forge_cli",
            event = "forge.git.maintenance_completed",
            request_id = %request_id,
            repo_id = repo.get(),
            outcome = "succeeded",
            fsck_mode = fsck.as_str(),
            skipped_step = skipped_step.unwrap_or(""),
            objects = counts.objects(),
            loose_objects = counts.loose_objects,
            packs = counts.packs,
            size_pack_kib = counts.size_pack_kib,
            refs = counts.refs,
            duration_ms,
            "repository maintenance completed"
        ),
        RunEnd::Skipped(reason, _) => tracing::info!(
            target: "forge_cli",
            event = "forge.git.maintenance_skipped",
            request_id = %request_id,
            repo_id = repo.get(),
            reason = *reason,
            duration_ms,
            "repository maintenance skipped"
        ),
        RunEnd::Failed { error, .. } => {
            let code = error.code();
            let corrupt = code == ErrorCode::RepositoryCorrupt;
            let outcome = if corrupt { "corrupt" } else { "failed" };
            let context = context_of(error);
            let step = step_of(error);
            // FG-GIT-0018 is a warn; the second failure in a row is an
            // alert (error level), like doctor's `reason=failing`.
            let failures = before.saturating_add(1);
            if corrupt || failures >= ALERT_AFTER_FAILURES {
                tracing::error!(
                    target: "forge_cli",
                    event = "forge.git.maintenance_completed",
                    error_code = code.id(),
                    request_id = %request_id,
                    repo_id = repo.get(),
                    outcome,
                    step = %step,
                    failures,
                    context = %context,
                    duration_ms,
                    "{}",
                    code.entry().cause
                );
            } else {
                tracing::warn!(
                    target: "forge_cli",
                    event = "forge.git.maintenance_completed",
                    error_code = code.id(),
                    request_id = %request_id,
                    repo_id = repo.get(),
                    outcome,
                    step = %step,
                    failures,
                    context = %context,
                    duration_ms,
                    "{}",
                    code.entry().cause
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
