//! The `purge` task of `forge worker` (step D5 MR4, ADR-0015 Amendment D5):
//! every `hosting.purge_interval_secs` it purges up to [`BATCH`] projects
//! whose grace period is over, oldest `purge_after` first. `forge admin
//! project purge --now` runs the same [`purge_batch`] once.
//!
//! Per project (forge-db `projects::purge`, module docs there):
//! 1. `start` — the repository moves to `purging` (a restore that won the
//!    race, a project gone or not due is a skip, logged at debug);
//! 2. if the repository's provisioning job is `running`, wait for the next
//!    run (`forge.repo.purge_deferred`): a provisioning run that read the
//!    repository before the purge started could otherwise create the
//!    directory after it was removed (once it ends, a later run sees
//!    `purging` and creates nothing);
//! 3. the bare directory is removed (forge-git `StorageRoot::remove_blocking`,
//!    the path derived from the repository id; a directory already gone is
//!    fine) on the blocking pool, waited for at most [`STORAGE_TIMEOUT`];
//! 4. `finish` — the rows, the `deleted` tombstone, the `project.purged`
//!    audit row and event; then `forge.repo.purged` is logged once.
//!
//! **One purge per project at a time** (review of fd19f2a): every step runs
//! under the project's session advisory lock (forge-db `PurgeLock`), taken
//! before `start` and released after `finish`. A project whose lock is held
//! (a `forge admin project purge --now` and the worker, two workers) is
//! `Deferred("in_progress")` — not an error, no alert. The removal owns the
//! lock while it runs: when it outlasts [`STORAGE_TIMEOUT`] the project is
//! `Deferred("removal_running")` and the removal goes on in its own task,
//! still holding the lock, which it releases only when the deletion has
//! really ended (so no second `remove_dir_all` runs on the same tree); the
//! next run after that resumes and finishes it.
//!
//! A failure of one project is logged once, here, as `FG-REPO-0020`
//! (`step`, `cause_code`, the underlying error as `source`; runbook
//! repository-purge) with that project's request id, and the batch goes on
//! with the next project; the project is tried again on the next run and
//! resumes where it stopped (`purging` never goes back to `ready`). Only a
//! failure to list the due projects fails the run (the supervisor logs it
//! and backs off).
//!
//! Every step is cancel-safe for the supervisor: each database step is one
//! transaction, and an interrupted removal leaves a `.purge-<id>-*`
//! directory the next run sweeps.

use std::time::{Duration, Instant};

use forge_core::ids::{ProjectId, RepoId};
use forge_core::{telemetry, RequestId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::purge::{self, DuePurge, Finished, PurgeLock, Started};
use forge_db::repositories::{self, ProvisionJobState};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::{Removal, StorageRoot};

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "purge";
/// At most this many projects per run.
pub(crate) const BATCH: i64 = 20;
/// How long a run waits for one directory removal; a slower one goes on in
/// the background under the purge lock (`Deferred("removal_running")`).
pub(crate) const STORAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// No further project of a run is started after this long, so one run (and
/// a shutdown waiting for it) stays bounded: the budget plus one project's
/// worst case (two changes of at most 30 s each and [`STORAGE_TIMEOUT`]).
/// The rest waits for the next run.
pub(crate) const RUN_BUDGET: Duration = Duration::from_secs(300);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// The blocking removal of a repository's directory (production:
/// `StorageRoot::remove_blocking`; tests: a removal they hold open).
pub(crate) type Remover =
    std::sync::Arc<dyn Fn(RepoId) -> Result<Removal, ForgeError> + Send + Sync>;

/// What the purge needs: the database and the removal under the storage root.
#[derive(Clone)]
pub(crate) struct Purger {
    pub(crate) db: Db,
    /// The audit label of the changes (`forge-worker`, or the CLI's).
    pub(crate) actor_label: String,
    /// The bound of one directory removal ([`STORAGE_TIMEOUT`]).
    pub(crate) storage_timeout: Duration,
    /// No project is started after this much of a run ([`RUN_BUDGET`]).
    pub(crate) run_budget: Duration,
    /// The directory removal.
    pub(crate) remover: Remover,
}

impl Purger {
    pub(crate) fn new(db: &Db, root: StorageRoot) -> Self {
        Self {
            db: db.clone(), // clone: pool handles
            remover: std::sync::Arc::new(move |repo| root.remove_blocking(repo)),
            actor_label: ACTOR_LABEL.to_owned(),
            storage_timeout: STORAGE_TIMEOUT,
            run_budget: RUN_BUDGET,
        }
    }
}

/// How one project of a batch ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Purged now (`storage_removed`: the directory was there).
    Purged { storage_removed: bool },
    /// Not due any more (`gone`, `not_deleted`, `not_expired`), or finished
    /// by a concurrent run (`gone`).
    Skipped(&'static str),
    /// Tried again on the next run: `provisioning_running` (its
    /// provisioning job runs), `in_progress` (another purge of it holds
    /// its lock), `removal_running` (its removal outlasted the wait and
    /// goes on in the background).
    Deferred(&'static str),
    /// Failed with this `FG-REPO-0020` (already logged).
    Failed(ErrorCode),
}

impl Outcome {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Purged { .. } => "purged",
            Self::Skipped(_) => "skipped",
            Self::Deferred(_) => "deferred",
            Self::Failed(_) => "failed",
        }
    }
}

/// One project of a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Purged {
    pub(crate) project_id: ProjectId,
    pub(crate) repo_id: RepoId,
    pub(crate) request_id: RequestId,
    pub(crate) outcome: Outcome,
}

/// One run of the task: a batch; at once again when the batch was full and
/// every project in it was purged (more may wait), else after the period.
pub(crate) async fn run_purge(purger: &Purger) -> Result<Next, ForgeError> {
    let done = purge_batch(purger, BATCH).await?;
    let purged = done
        .iter()
        .filter(|p| matches!(p.outcome, Outcome::Purged { .. }))
        .count();
    let full = i64::try_from(purged).unwrap_or(i64::MAX) >= BATCH;
    Ok(if full {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}

/// Purge up to `limit` due projects, one after the other (module docs),
/// starting none after the run budget (the rest is left for the next run,
/// `forge.repo.purge_cut`). Errors: only the listing's (`FG-DB-0001` /
/// `FG-DB-0002`); each project's failure is logged and reported in its
/// [`Purged`].
pub(crate) async fn purge_batch(purger: &Purger, limit: i64) -> Result<Vec<Purged>, ForgeError> {
    let started = Instant::now();
    let due = purge::due(&purger.db, limit).await?;
    let mut done = Vec::with_capacity(due.len());
    for project in &due {
        if started.elapsed() >= purger.run_budget {
            tracing::info!(
                target: "forge_cli",
                event = "forge.repo.purge_cut",
                left = due.len() - done.len(),
                budget_ms = u64::try_from(purger.run_budget.as_millis()).unwrap_or(u64::MAX),
                "purge run budget used up; the rest waits for the next run"
            );
            break;
        }
        let request_id = RequestId::generate();
        let outcome = match purge_one(purger, project, &request_id).await {
            Ok(outcome) => outcome,
            Err(e) => {
                telemetry::log_failure(&e, &request_id);
                Outcome::Failed(e.code())
            }
        };
        done.push(Purged {
            project_id: project.project_id,
            repo_id: project.repository_id,
            request_id,
            outcome,
        });
    }
    Ok(done)
}

/// `FG-REPO-0020` for a failed `step` of `project`, the cause as source.
fn stuck(step: &'static str, project: &DuePurge, cause: ForgeError) -> ForgeError {
    ForgeError::new(ErrorCode::RepositoryPurgeStuck)
        .with_context("step", step)
        .with_context("project_id", project.project_id.get())
        .with_context("repo_id", project.repository_id.get())
        .with_context("cause_code", cause.code().id())
        .with_context("overdue_s", project.overdue_secs)
        .with_source(cause)
}

fn deferred(request_id: &RequestId, project: &DuePurge, reason: &'static str) -> Outcome {
    tracing::info!(
        target: "forge_cli",
        event = "forge.repo.purge_deferred",
        request_id = %request_id,
        project_id = project.project_id.get(),
        repo_id = project.repository_id.get(),
        reason,
        "purge deferred"
    );
    Outcome::Deferred(reason)
}

async fn purge_one(
    purger: &Purger,
    project: &DuePurge,
    request_id: &RequestId,
) -> Result<Outcome, ForgeError> {
    let id = project.project_id;
    let Some(lock) = PurgeLock::try_acquire(&purger.db, id)
        .await
        .map_err(|e| stuck("lock", project, e))?
    else {
        return Ok(deferred(request_id, project, "in_progress"));
    };
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone());
    ctx.actor_label = Some(purger.actor_label.clone()); // clone: one label per change
    let prepared = prepare(purger, project, request_id, &ctx).await;
    let (repo, resumed) = match prepared {
        Ok(Prepared::Remove { repo, resumed }) => (repo, resumed),
        Ok(Prepared::Done(outcome)) => {
            lock.release().await;
            return Ok(outcome);
        }
        Err(e) => {
            lock.release().await;
            return Err(e);
        }
    };
    let started_at = Instant::now();
    let Some((removed, lock)) = remove(purger, repo, lock).await else {
        return Ok(deferred(request_id, project, "removal_running"));
    };
    let outcome = match removed {
        Ok(removal) => {
            finish(
                purger, project, request_id, &ctx, repo, removal, resumed, started_at,
            )
            .await
        }
        Err(e) => Err(stuck("storage", project, e)),
    };
    lock.release().await;
    outcome
}

/// What happens after `start`.
enum Prepared {
    /// Remove the directory of `repo` (`resumed`: an earlier run started).
    Remove { repo: RepoId, resumed: bool },
    /// Nothing to remove now.
    Done(Outcome),
}

/// `start` and the provisioning check, under the purge lock.
async fn prepare(
    purger: &Purger,
    project: &DuePurge,
    request_id: &RequestId,
    ctx: &ChangeContext,
) -> Result<Prepared, ForgeError> {
    let id = project.project_id;
    let started = purge::start(&purger.db, ctx, id)
        .await
        .map_err(|e| stuck("start", project, e))?;
    let (repo, resumed) = match started {
        Started::Skipped { reason } => {
            // A restore won the race, or a concurrent run finished it.
            tracing::debug!(
                target: "forge_cli",
                event = "forge.repo.purge_skipped",
                request_id = %request_id,
                project_id = id.get(),
                reason,
                "purge skipped"
            );
            return Ok(Prepared::Done(Outcome::Skipped(reason)));
        }
        Started::Started { repository_id, .. } => (repository_id, false),
        Started::Resumed { repository_id } => (repository_id, true),
    };
    let job = repositories::provision_job_state(&purger.db, repo)
        .await
        .map_err(|e| stuck("provision_check", project, e))?;
    if job == ProvisionJobState::Running {
        return Ok(Prepared::Done(deferred(
            request_id,
            project,
            "provisioning_running",
        )));
    }
    Ok(Prepared::Remove { repo, resumed })
}

/// Run the removal on the blocking pool in a task that owns `lock`, and
/// wait for it at most the storage timeout. In time: the result and the
/// lock back. Too slow: `None` — the task keeps the lock until the
/// removal has ended, then releases it.
async fn remove(
    purger: &Purger,
    repo: RepoId,
    lock: PurgeLock,
) -> Option<(Result<Removal, ForgeError>, PurgeLock)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let remover = std::sync::Arc::clone(&purger.remover);
    tokio::spawn(async move {
        let removed = match tokio::task::spawn_blocking(move || remover(repo)).await {
            Ok(removed) => removed,
            Err(join) => Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "purge_removal")
                .with_context("reason", "blocking_task_failed")
                .with_context("repo_id", repo.get())
                .with_source(join)),
        };
        // The run stopped waiting: the lock ends here, after the removal.
        if let Err((removed, lock)) = tx.send((removed, lock)) {
            if let Err(e) = &removed {
                // Nobody else will see it; the next run retries.
                tracing::warn!(
                    target: "forge_cli",
                    event = "forge.repo.purge_removal_ended",
                    repo_id = repo.get(),
                    error_code = e.code().id(),
                    error = %e,
                    "a background removal failed; the next run retries it"
                );
            }
            lock.release().await;
        }
    });
    match tokio::time::timeout(purger.storage_timeout, rx).await {
        Ok(Ok(done)) => Some(done),
        // The task ended without sending: it panicked; its lock was dropped
        // (its connection closed). Reported like a slow removal.
        Ok(Err(_)) | Err(_) => None,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the last step of one purge, each value distinct"
)]
async fn finish(
    purger: &Purger,
    project: &DuePurge,
    request_id: &RequestId,
    ctx: &ChangeContext,
    repo: RepoId,
    removal: Removal,
    resumed: bool,
    started_at: Instant,
) -> Result<Outcome, ForgeError> {
    let id = project.project_id;
    // A run that renamed the directory aside and was cut before finishing
    // leaves it to the sweep: removed now all the same.
    let storage_removed = removal.existed || removal.swept > 0;
    let finished = purge::finish(&purger.db, ctx, id, storage_removed)
        .await
        .map_err(|e| stuck("finish", project, e))?;
    let Finished::Purged(purged) = finished else {
        return Ok(Outcome::Skipped("gone"));
    };
    tracing::info!(
        target: "forge_cli",
        event = "forge.repo.purged",
        request_id = %request_id,
        project_id = id.get(),
        repo_id = repo.get(),
        tombstone_id = purged.tombstone_id.get(),
        redirects_ended = purged.redirects_ended,
        storage_removed,
        swept = removal.swept,
        resumed,
        overdue_s = project.overdue_secs,
        duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        "project purged"
    );
    Ok(Outcome::Purged { storage_removed })
}

#[cfg(test)]
#[path = "purge_tests.rs"]
pub(crate) mod tests;
