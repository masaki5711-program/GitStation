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
//! 3. the bare directory is removed (forge-git `StorageRoot::remove_until`,
//!    the path derived from the repository id, bounded by
//!    [`STORAGE_TIMEOUT`]; a directory already gone is fine);
//! 4. `finish` — the rows, the `deleted` tombstone, the `project.purged`
//!    audit row and event; then `forge.repo.purged` is logged once.
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
use forge_db::projects::purge::{self, DuePurge, Finished, Started};
use forge_db::repositories::{self, ProvisionJobState};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "purge";
/// At most this many projects per run.
pub(crate) const BATCH: i64 = 20;
/// The longest removal of one bare directory; a slower one is cut
/// (`FG-REPO-0003 reason=deadline`) and resumed on the next run.
pub(crate) const STORAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// No further project of a run is started after this long, so one run (and
/// a shutdown waiting for it) stays bounded: the budget plus one project's
/// worst case (two changes of at most 30 s each and [`STORAGE_TIMEOUT`]).
/// The rest waits for the next run.
pub(crate) const RUN_BUDGET: Duration = Duration::from_secs(300);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// What the purge needs: the database and the storage root.
#[derive(Clone)]
pub(crate) struct Purger {
    pub(crate) db: Db,
    pub(crate) root: StorageRoot,
    /// The audit label of the changes (`forge-worker`, or the CLI's).
    pub(crate) actor_label: String,
    /// The bound of one directory removal ([`STORAGE_TIMEOUT`]).
    pub(crate) storage_timeout: Duration,
    /// No project is started after this much of a run ([`RUN_BUDGET`]).
    pub(crate) run_budget: Duration,
}

impl Purger {
    pub(crate) fn new(db: &Db, root: StorageRoot) -> Self {
        Self {
            db: db.clone(), // clone: pool handles
            root,
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
    /// Its provisioning job is running: tried again on the next run.
    Deferred,
    /// Failed with this `FG-REPO-0020` (already logged).
    Failed(ErrorCode),
}

impl Outcome {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Purged { .. } => "purged",
            Self::Skipped(_) => "skipped",
            Self::Deferred => "deferred",
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

async fn purge_one(
    purger: &Purger,
    project: &DuePurge,
    request_id: &RequestId,
) -> Result<Outcome, ForgeError> {
    let started_at = Instant::now();
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone());
    ctx.actor_label = Some(purger.actor_label.clone()); // clone: one label per change
    let id = project.project_id;
    let started = purge::start(&purger.db, &ctx, id)
        .await
        .map_err(|e| stuck("start", project, e))?;
    let repo = match started {
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
            return Ok(Outcome::Skipped(reason));
        }
        Started::Started { repository_id, .. } | Started::Resumed { repository_id } => {
            repository_id
        }
    };
    let job = repositories::provision_job_state(&purger.db, repo)
        .await
        .map_err(|e| stuck("start", project, e))?;
    if job == ProvisionJobState::Running {
        tracing::info!(
            target: "forge_cli",
            event = "forge.repo.purge_deferred",
            request_id = %request_id,
            project_id = id.get(),
            repo_id = repo.get(),
            reason = "provisioning_running",
            "purge deferred"
        );
        return Ok(Outcome::Deferred);
    }
    let removal = purger
        .root
        .remove_until(repo, Instant::now() + purger.storage_timeout)
        .await
        .map_err(|e| stuck("storage", project, e))?;
    let finished = purge::finish(&purger.db, &ctx, id, removal.existed)
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
        storage_removed = removal.existed,
        swept = removal.swept,
        resumed = matches!(started, Started::Resumed { .. }),
        overdue_s = project.overdue_secs,
        duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        "project purged"
    );
    Ok(Outcome::Purged {
        storage_removed: removal.existed,
    })
}

#[cfg(test)]
#[path = "purge_tests.rs"]
pub(crate) mod tests;
