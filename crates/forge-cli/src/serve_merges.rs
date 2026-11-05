//! The merge tasks of `forge serve` (M3 R4, ADR-0024 §4.2; ADR-0024
//! "実装で決めたこと（R4）"): the job `merge_request.merge` runs in the
//! process that serves the browse API, so the merge's ref writes drop the
//! same browse cache the next request reads (`Browser::invalidate`; the
//! worker process has no browse cache to drop). `forge worker` does not
//! register the kind, so only `forge serve` claims it; the worker's reaper
//! still requeues an expired lease.
//!
//! Two tasks (review round 1: a slow reconciliation never delays a merge),
//! each under a supervisor:
//!
//! - **runner**: one merge at a time (every second while idle, at once again
//!   after a job). A claim that fails backs off (1 s doubling to 30 s) and is
//!   logged on the first failure of a streak and then at most once a minute
//!   with the streak's count.
//! - **reconcile**: [`forge_review::Merger::reconcile`] every
//!   [`RECONCILE_PERIOD`], each pass bounded by [`PASS_TIMEOUT`] and stopped
//!   at once by a shutdown. Each pass logs a heartbeat (info, once a minute,
//!   `forge.serve.merge_reconcile_pass` with the pass's counts); `forge
//!   doctor`'s `review.merges` reports attempts the reconciliation has left
//!   past their bound (it is not running, or fails every pass).
//! - **supervisor**: a task that ends while serve is not stopping (a panic,
//!   an unexpected return) is logged once as `FG-SYS-0005` ServeFailed
//!   (`operation=merge_task_exited`, `task`, `panicked`) and started again
//!   after a backoff (1 s doubling to 30 s, reset after a minute of running).
//!
//! The handler: success or a decision (a refusal, a conflict, a lost
//! compare-and-swap) completes the job; a failure that may pass later
//! ([`retryable`]) is retried with the queue's backoff; on the last attempt,
//! or a failure that can never pass, an attempt that never passed the fence
//! ends `superseded reason=job_failed` (`Merger::give_up`) before the job
//! dies.

use std::sync::Arc;
use std::time::Duration;

use forge_core::{telemetry, RequestId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind, JobRunner, WorkerId};
use forge_db::review::merges::{merge_payload, MERGE_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_review::Merger;
use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};

/// How often an idle runner looks for a merge.
const POLL_PERIOD: Duration = Duration::from_secs(1);
/// How often stuck attempts are reconciled.
pub(crate) const RECONCILE_PERIOD: Duration = Duration::from_secs(60);
/// The most one reconciliation pass may take.
pub(crate) const PASS_TIMEOUT: Duration = Duration::from_secs(50);
/// How long a merge in progress may finish after a shutdown request.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// The backoff of a failing claim and of a restarted task.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A failure streak is logged at most this often after its first line.
const LOG_EVERY: Duration = Duration::from_secs(60);
/// A task that ran this long before it ended restarts without backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// The handler of `merge_request.merge`.
pub(crate) struct MergeHandler {
    kind: JobKind,
    merger: Arc<Merger>,
}

impl MergeHandler {
    pub(crate) fn new(merger: Arc<Merger>) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(MERGE_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", MERGE_JOB)
        })?;
        Ok(Self { kind, merger })
    }
}

/// Whether a failed run may pass later. Exhaustive over [`ErrorCode`]
/// (review round 1: a new code is a compile error here until it is
/// classified, never an implicit "permanent").
pub(crate) fn retryable(e: &ForgeError) -> bool {
    match e.code() {
        // git, storage and the database (the merge lock busy or lost, a
        // locked ref: FG-DB-0002), a repository still provisioning, and
        // FG-REVIEW-0009 (the attempt stays `executing`: the retry asks git).
        ErrorCode::GitUnavailable
        | ErrorCode::GitTimeout
        | ErrorCode::GitProcessFailed
        | ErrorCode::GitBusy
        | ErrorCode::StorageIoFailed
        | ErrorCode::RepositoryNotReady
        | ErrorCode::DbUnavailable
        | ErrorCode::DbQueryFailed
        | ErrorCode::MergeExecutionFailed => true,
        // Decisions, broken invariants and everything else: dead at once.
        ErrorCode::Internal
        | ErrorCode::ConfigInvalid
        | ErrorCode::UnknownErrorCode
        | ErrorCode::GitOutputTooLarge
        | ErrorCode::InvalidRefName
        | ErrorCode::StoragePathRejected
        | ErrorCode::StorageMissing
        | ErrorCode::RepoPathOccupied
        | ErrorCode::NotFound
        | ErrorCode::PendingDeletion
        | ErrorCode::NamespacePathInvalid
        | ErrorCode::NamespacePathReserved
        | ErrorCode::NamespacePathTaken
        | ErrorCode::MembershipConflict
        | ErrorCode::GroupRequestInvalid
        | ErrorCode::RepositoryProvisioningStuck
        | ErrorCode::ProtectedRefRuleInvalid
        | ErrorCode::ProjectRequestInvalid
        | ErrorCode::TransferInvalid
        | ErrorCode::ProtectedRefPatternTaken
        | ErrorCode::RenameRateLimited
        | ErrorCode::RestoreUnavailable
        | ErrorCode::RepositoryPurgeStuck
        | ErrorCode::PurgeBatchIncomplete
        | ErrorCode::NamespaceMoveContended
        | ErrorCode::RepositoryPathNotResolved
        | ErrorCode::Unauthenticated
        | ErrorCode::InsufficientScope
        | ErrorCode::Forbidden
        | ErrorCode::AccountDisabled
        | ErrorCode::UserEmailTaken
        | ErrorCode::UserRequestInvalid
        | ErrorCode::UserIdentifierTaken
        | ErrorCode::TokenRequestInvalid
        | ErrorCode::AdministrationConflict
        | ErrorCode::AdministrationOrphaned
        | ErrorCode::LoginStateInvalid
        | ErrorCode::IdTokenInvalid
        | ErrorCode::LoginNotPermitted
        | ErrorCode::IdentityLinkConflict
        | ErrorCode::IdentityProviderUnavailable
        | ErrorCode::CsrfRejected
        | ErrorCode::LogoutTokenInvalid
        | ErrorCode::LoginNotConfigured
        | ErrorCode::ServiceAccountTokenExpiring
        | ErrorCode::ProtectedRefForcePush
        | ErrorCode::ProtectedRefPushNotAllowed
        | ErrorCode::ProtectedRefDirectPushForbidden
        | ErrorCode::ProtectedRefDeletion
        | ErrorCode::ReservedRefNamespace
        | ErrorCode::AmbiguousRefName
        | ErrorCode::PushRecordFailed
        | ErrorCode::GitRequestTooLarge
        | ErrorCode::GitProtocolInvalid
        | ErrorCode::GitClientDisconnected
        | ErrorCode::PushCommitsIncomplete
        | ErrorCode::RouteNotFound
        | ErrorCode::RequestTimeout
        | ErrorCode::ServerBindFailed
        | ErrorCode::MethodNotAllowed
        | ErrorCode::RouteRegistryInvalid
        | ErrorCode::ReadinessTimeout
        | ErrorCode::RequestInvalid
        | ErrorCode::RequestTooLarge
        | ErrorCode::RateLimited
        | ErrorCode::SmartHttpServiceUnsupported
        | ErrorCode::ServerBusy
        | ErrorCode::ServeFailed
        | ErrorCode::SignalHandlerUnavailable
        | ErrorCode::AcceptFailed
        | ErrorCode::MigrationFailed
        | ErrorCode::SchemaIncompatible
        | ErrorCode::OutboxSequencerLag
        | ErrorCode::JobDeadLettered
        | ErrorCode::JobLeaseExpired
        | ErrorCode::JobQueueBacklog
        | ErrorCode::MembershipDataInvalid
        | ErrorCode::CredentialDataInvalid
        | ErrorCode::ProtectedRefDataInvalid
        | ErrorCode::SubscriberLag
        | ErrorCode::AppendOnlyViolation
        | ErrorCode::ChangeRejected
        | ErrorCode::AuditSealMismatch
        | ErrorCode::AuditMissing
        | ErrorCode::AuditSealBacklog
        | ErrorCode::NamespaceDataInvalid
        | ErrorCode::CliUsageInvalid
        | ErrorCode::CliNotLoggedIn
        | ErrorCode::CliServerUnreachable
        | ErrorCode::CliResponseInvalid
        | ErrorCode::CliConfigInvalid
        | ErrorCode::CliCredentialStoreFailed
        | ErrorCode::CliOutputFailed
        | ErrorCode::CliWaitTimedOut
        | ErrorCode::DuplicateOpenMergeRequest
        | ErrorCode::PurgePreconditionFailed
        | ErrorCode::MergeabilityNotChecked
        | ErrorCode::DiscussionPositionInvalid
        | ErrorCode::DiscussionLimitReached
        | ErrorCode::MergeRequestConflict
        | ErrorCode::MergeBlocked
        | ErrorCode::HeadChanged
        | ErrorCode::MergeMethodNotAllowed
        | ErrorCode::MergeInProgress
        | ErrorCode::MergeIdentityInvalid
        | ErrorCode::SelfApprovalRefused
        | ErrorCode::CodeownersInvalid
        | ErrorCode::ApprovalLimitReached
        | ErrorCode::RequiredCheckNamespaceNotAllowed
        | ErrorCode::MergeSettingsChanged
        | ErrorCode::IntegrationLimitReached
        | ErrorCode::BindingConflict
        | ErrorCode::IntegrationDisabled
        | ErrorCode::WorkItemProjectNotBound
        | ErrorCode::WorkItemLinkLimitReached
        // M3 I1c: the feed's codes never reach the merge job.
        | ErrorCode::FeedCursorExpired
        | ErrorCode::FeedCursorInvalid
        | ErrorCode::FeedConsumerLag
        | ErrorCode::FeedEpochChanged
        | ErrorCode::ApprovalRuleChanged
        | ErrorCode::MergeRuleChanged
        // M3 C1: a stored run this build cannot read stays unreadable;
        // a report's own codes never reach the job.
        | ErrorCode::CheckRunDataInvalid
        | ErrorCode::CheckPayloadInvalid
        | ErrorCode::CheckRunLimitReached
        // M3 O3: the maintenance job's own codes never reach a merge.
        | ErrorCode::RepositoryMaintenanceFailed
        | ErrorCode::RepositoryCorrupt
        | ErrorCode::RepositoryRefLockStale
        // M3 N1: the mail job's own code never reaches a merge.
        | ErrorCode::SmtpUnavailable => false,
    }
}

impl JobHandler for MergeHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let merger = Arc::clone(&self.merger);
        Box::pin(async move {
            let merge_id = merge_payload(&job.payload).map_err(JobFailure::permanent)?;
            let Err(e) = merger.execute(merge_id).await else {
                return Ok(());
            };
            let last = job.lease.attempt >= job.max_attempts;
            if retryable(&e) && !last {
                return Err(JobFailure::retry(e));
            }
            // Failed for good: never leave an attempt that never moved a ref
            // live (an `executing` one is the reconciliation's).
            if let Err(cleanup) = merger.give_up(merge_id, &e).await {
                telemetry::log_failure(
                    &cleanup
                        .with_context("operation", "merge_give_up")
                        .with_context("merge_id", merge_id),
                    &job.request_id(),
                );
            }
            Err(if retryable(&e) {
                JobFailure::retry(e)
            } else {
                JobFailure::permanent(e)
            })
        })
    }
}

/// The two supervised tasks; [`MergeTasks::stop`] ends them.
pub(crate) struct MergeTasks {
    stop: watch::Sender<bool>,
    supervisors: Vec<(&'static str, JoinHandle<()>)>,
}

impl MergeTasks {
    /// Ask both tasks to stop and wait at most `wait` for each supervisor:
    /// a supervisor that panicked, or did not end in time, is logged once.
    pub(crate) async fn stop(self, wait: Duration) {
        // No receiver left means every task already ended.
        let _ = self.stop.send(true);
        for (task, handle) in self.supervisors {
            match tokio::time::timeout(wait, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(join)) => log_exit(task, Some(&join), "merge_supervisor_exited"),
                Err(_) => telemetry::log_failure(
                    &ForgeError::new(ErrorCode::ServeFailed)
                        .with_context("operation", "merge_task_pending")
                        .with_context("task", task)
                        .with_context("wait_ms", wait.as_millis()),
                    &RequestId::generate(),
                ),
            }
        }
    }
}

/// `FG-SYS-0005` ServeFailed for a task (or its supervisor) that ended
/// unexpectedly; `join` tells a panic from a cancellation.
fn log_exit(task: &'static str, join: Option<&JoinError>, operation: &'static str) {
    telemetry::log_failure(
        &ForgeError::new(ErrorCode::ServeFailed)
            .with_context("operation", operation)
            .with_context("task", task)
            .with_context("panicked", join.is_some_and(JoinError::is_panic))
            .with_context("cancelled", join.is_some_and(JoinError::is_cancelled)),
        &RequestId::generate(),
    );
}

/// Run what `start` spawns until `stop`; start it again (after a backoff)
/// whenever it ends while not stopping — each exit logged once.
pub(crate) async fn supervise<F>(
    task: &'static str,
    mut stop: watch::Receiver<bool>,
    (min, max): (Duration, Duration),
    start: F,
) where
    F: Fn(watch::Receiver<bool>) -> JoinHandle<()>,
{
    let mut delay = min;
    loop {
        if *stop.borrow() {
            return;
        }
        let began = tokio::time::Instant::now();
        let ended = start(stop.clone()).await; // clone: the task waits on its own receiver
        if *stop.borrow() {
            if let Err(join) = &ended {
                log_exit(task, Some(join), "merge_task_exited");
            }
            return;
        }
        log_exit(task, ended.as_ref().err(), "merge_task_exited");
        if began.elapsed() >= HEALTHY_RUN {
            delay = min;
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            changed = stop.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        delay = delay.saturating_mul(2).min(max);
    }
}

/// Start the merge runner and the reconciliation under their supervisors.
pub(crate) fn spawn(db: &Db, merger: Arc<Merger>) -> Result<MergeTasks, ForgeError> {
    merger.timeouts().check()?;
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(MergeHandler::new(Arc::clone(&merger))?))?;
    tracing::info!(
        worker_id = runner.worker_id().as_str(),
        kind = MERGE_JOB,
        "forge.serve.merge_runner_started"
    );
    let runner = Arc::new(runner);
    let (stop, stopped) = watch::channel(false);
    let backoff = (BACKOFF_MIN, BACKOFF_MAX);
    let run_jobs = tokio::spawn(supervise(
        "merge_runner",
        stopped.clone(), // clone: the supervisor's own receiver
        backoff,
        move |s| tokio::spawn(run_jobs(Arc::clone(&runner), s)),
    ));
    let reconcile = tokio::spawn(supervise("merge_reconcile", stopped, backoff, move |s| {
        tokio::spawn(reconcile_loop(Arc::clone(&merger), s))
    }));
    Ok(MergeTasks {
        stop,
        supervisors: vec![("merge_runner", run_jobs), ("merge_reconcile", reconcile)],
    })
}

/// Wait `period`; `false` when `stop` turned true (or serve is gone) first.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, period: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(period) => true,
        _ = stop.wait_for(|s| *s) => false,
    }
}

/// A streak of failures, logged once at its start and then at most every
/// [`LOG_EVERY`] with its count; the backoff doubles from [`BACKOFF_MIN`]
/// to [`BACKOFF_MAX`].
#[derive(Default)]
struct Streak {
    count: u64,
    logged_at: Option<tokio::time::Instant>,
}

impl Streak {
    fn fail(&mut self, e: ForgeError, operation: &'static str) -> Duration {
        self.count += 1;
        let due = self.logged_at.is_none_or(|at| at.elapsed() >= LOG_EVERY);
        if due {
            telemetry::log_failure(
                &e.with_context("operation", operation)
                    .with_context("streak", self.count),
                &RequestId::generate(),
            );
            self.logged_at = Some(tokio::time::Instant::now());
        }
        let shift = u32::try_from(self.count.saturating_sub(1).min(5)).unwrap_or(5);
        BACKOFF_MIN.saturating_mul(1 << shift).min(BACKOFF_MAX)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

async fn run_jobs(runner: Arc<JobRunner>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        let mut job_stop = stop.clone(); // clone: the run waits on its own receiver
        let stopping = async move {
            let closed = job_stop.wait_for(|s| *s).await.is_err();
            if closed {
                std::future::pending::<()>().await;
            }
        };
        let wait = match runner.run_once_until(stopping, SHUTDOWN_GRACE).await {
            Ok(Some(_)) => {
                streak.reset();
                continue;
            }
            Ok(None) => {
                streak.reset();
                POLL_PERIOD
            }
            Err(e) => streak.fail(e, "merge_runner_claim"),
        };
        if !wait_or_stop(&mut stop, wait).await {
            break;
        }
    }
    tracing::info!("forge.serve.merge_runner_stopped");
}

async fn reconcile_loop(merger: Arc<Merger>, mut stop: watch::Receiver<bool>) {
    let mut streak = Streak::default();
    loop {
        if *stop.borrow() {
            break;
        }
        let rid = RequestId::generate();
        let pass = tokio::time::timeout(PASS_TIMEOUT, merger.reconcile(&rid));
        let mut pass_stop = stop.clone(); // clone: the pass waits on its own receiver
        tokio::select! {
            done = pass => match done {
                Ok(Ok(report)) => {
                    streak.reset();
                    // The liveness heartbeat (info, once a minute; review
                    // round 2). doctor `review.merges` is the other signal.
                    tracing::info!(
                        request_id = %rid,
                        looked = report.looked,
                        applied = report.applied,
                        superseded = report.superseded,
                        failed = report.failed,
                        skipped = report.skipped,
                        "forge.serve.merge_reconcile_pass"
                    );
                }
                Ok(Err(e)) => {
                    streak.fail(e, "merge_reconcile");
                }
                Err(_) => {
                    streak.fail(
                        ForgeError::new(ErrorCode::ServeFailed)
                            .with_context("reason", "reconcile_pass_timeout")
                            .with_context("timeout_ms", PASS_TIMEOUT.as_millis()),
                        "merge_reconcile",
                    );
                }
            },
            _ = pass_stop.wait_for(|s| *s) => break,
        }
        if !wait_or_stop(&mut stop, RECONCILE_PERIOD).await {
            break;
        }
    }
    tracing::info!("forge.serve.merge_reconcile_stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn failures_that_can_pass_later_are_retried_and_decisions_are_not() {
        for code in [
            ErrorCode::GitTimeout,
            ErrorCode::DbQueryFailed,
            ErrorCode::MergeExecutionFailed,
        ] {
            assert!(retryable(&ForgeError::new(code)), "{code:?}");
        }
        for code in [
            ErrorCode::Internal,
            ErrorCode::NotFound,
            ErrorCode::MergeBlocked,
            ErrorCode::Forbidden,
        ] {
            assert!(!retryable(&ForgeError::new(code)), "{code:?}");
        }
    }

    /// A task that panics, or returns while serve is not stopping, is
    /// started again (after the backoff); a stop ends the supervisor.
    #[tokio::test]
    async fn the_supervisor_restarts_a_task_that_panics_or_returns() {
        let starts = Arc::new(AtomicUsize::new(0));
        let (stop, stopped) = watch::channel(false);
        let counted = Arc::clone(&starts);
        let supervisor = tokio::spawn(supervise(
            "test_task",
            stopped,
            (Duration::from_millis(1), Duration::from_millis(2)),
            move |mut s| {
                let n = counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    match n {
                        0 => panic!("a task that panics"),
                        1 => {}
                        _ => {
                            let _stopped = s.wait_for(|v| *v).await;
                        }
                    }
                })
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while starts.load(Ordering::SeqCst) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("started three times");
        stop.send(true).expect("stop");
        tokio::time::timeout(Duration::from_secs(5), supervisor)
            .await
            .expect("the supervisor ends on stop")
            .expect("no panic escapes the supervisor");
        assert_eq!(starts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failure_streak_backs_off_and_caps() {
        let mut s = Streak::default();
        let waits: Vec<Duration> = (0..8)
            .map(|_| s.fail(ForgeError::new(ErrorCode::DbUnavailable), "test"))
            .collect();
        assert_eq!(waits[0], Duration::from_secs(1));
        assert_eq!(waits[1], Duration::from_secs(2));
        assert_eq!(waits.last().copied(), Some(BACKOFF_MAX));
        s.reset();
        assert_eq!(s.count, 0);
    }
}
