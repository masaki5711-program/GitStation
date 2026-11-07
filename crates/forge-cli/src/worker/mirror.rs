//! The push mirror lane of `forge worker` (M3 O2, ADR-0024 Amendment O2,
//! runbook gitlab-mirror).
//!
//! - The `push-mirror` subscriber always runs (it only marks a mirror
//!   waiting and enqueues `repository.push_mirror`), so a push is never lost
//!   while the lane is off: the job waits in the queue and the lag shows.
//! - With `mirror.credentials_dir` configured, `repository.push_mirror` runs
//!   on its OWN job runner and git slot pool (like the maintenance lane): a
//!   slow GitLab holds one push for up to `mirror.push_timeout_secs` and must
//!   not delay merge or provisioning jobs. Without it the start logs
//!   `forge.worker.mirror_lane_off` (warn, FG-SYS-0002) when mirrors exist.
//! - Each run reads the mirror's setting and its credential file again (a
//!   rotated token or a changed allowlist takes effect at the next push),
//!   checks the URL against the policy of the moment, and pushes. A failed
//!   attempt is warn `forge.mirror.push_failed` and retried (5 attempts);
//!   the last is FG-INT-0013 (alert) once, and the job ends there (the
//!   status keeps the failure; the next push or `gstn mirror sync` tries
//!   again).
//! - The `mirror_lag` task (every minute) logs FG-INT-0014 for each mirror
//!   waiting longer than `mirror.lag_alert_secs`, at most once an hour each.

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::MirrorConfig;
use forge_core::ids::RepoId;
use forge_core::mirror::CredentialName;
use forge_core::{telemetry, RequestId};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind, JobRunner, WorkerId};
use forge_db::mirror::{self as db, JobTarget, MAX_PUSH_TIMEOUT, PUSH_MIRROR_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::mirror::MirrorDestination;
use forge_git::{Git, StorageRoot};

use super::provision::Storage;
use super::supervise::Next;

/// The lane's runner task name.
pub(crate) const RUNNER_NAME: &str = "mirror_runner";
/// The lag sweep's task name.
pub(crate) const LAG_NAME: &str = "mirror_lag";
/// How often the lag sweep runs.
pub(crate) const LAG_PERIOD: Duration = Duration::from_secs(60);

/// How a job reaches its destination (tests push to local bare
/// repositories instead of GitLab).
pub(crate) enum Resolver {
    /// The stored URL, the policy and the credential file of the config.
    Config(MirrorConfig),
    /// Tests only: every repository's mirror is this local bare repository.
    #[cfg(test)]
    Local(std::path::PathBuf),
}

pub(crate) struct Inner {
    db: Db,
    root: StorageRoot,
    git: Git,
    resolver: Resolver,
    push_timeout: Duration,
}

/// The handler of `repository.push_mirror`.
pub(crate) struct MirrorHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

fn payload_repo(payload: &serde_json::Value) -> Result<RepoId, ForgeError> {
    payload
        .get("repo_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .map(RepoId::new)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_payload")
                .with_context("kind", PUSH_MIRROR_JOB)
        })
}

impl MirrorHandler {
    pub(crate) fn new(
        db: &Db,
        root: StorageRoot,
        git: Git,
        resolver: Resolver,
        push_timeout: Duration,
    ) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(PUSH_MIRROR_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", PUSH_MIRROR_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: pool handles
                root,
                git,
                resolver,
                push_timeout: push_timeout.min(MAX_PUSH_TIMEOUT),
            }),
        })
    }
}

impl JobHandler for MirrorHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let repo = payload_repo(&job.payload).map_err(JobFailure::permanent)?;
            let last = job.lease.attempt >= job.max_attempts;
            inner
                .mirror(repo, &job.request_id(), job.lease.attempt, last)
                .await
        })
    }
}

impl Inner {
    /// One run (module docs). `Ok` also after the last failed attempt
    /// (FG-INT-0013 logged here, once); `Err(retry)` before it.
    async fn mirror(
        &self,
        repo: RepoId,
        request_id: &RequestId,
        attempt: i32,
        last: bool,
    ) -> Result<(), JobFailure> {
        // A database failure is the queue's to retry (not a mirror failure).
        let Some(target) = db::start(&self.db, repo).await.map_err(JobFailure::retry)? else {
            tracing::info!(
                target: "forge_cli",
                event = "forge.mirror.nothing_to_push",
                request_id = %request_id,
                repo_id = repo.get(),
                "no enabled mirror for a ready repository: the job ends"
            );
            return Ok(());
        };
        match self.push(repo, &target).await {
            Ok(report) => {
                db::succeeded(&self.db, repo, target.covers_seq)
                    .await
                    .map_err(JobFailure::retry)?;
                tracing::info!(
                    target: "forge_cli",
                    event = "forge.mirror.pushed",
                    request_id = %request_id,
                    repo_id = repo.get(),
                    project_id = target.project.get(),
                    origin = %target.origin,
                    updated = report.updated,
                    deleted = report.deleted,
                    up_to_date = report.up_to_date,
                    "the GitLab mirror is up to date"
                );
                Ok(())
            }
            Err(e) => {
                let e = e
                    .with_context("repo_id", repo.get())
                    .with_context("project_id", target.project.get())
                    .with_context("origin", &target.origin)
                    .with_context("attempt", attempt);
                let reason = e
                    .context()
                    .iter()
                    .find(|(k, _)| *k == "reason")
                    .map_or("git", |(_, v)| v.as_str())
                    .to_owned();
                db::failed(&self.db, repo, ErrorCode::PushMirrorFailed, &reason)
                    .await
                    .map_err(JobFailure::retry)?;
                if last {
                    telemetry::log_failure(&e, request_id);
                    Ok(())
                } else {
                    tracing::warn!(
                        target: "forge_cli",
                        event = "forge.mirror.push_failed",
                        error_code = e.code().id(),
                        request_id = %request_id,
                        repo_id = repo.get(),
                        attempt,
                        reason = %reason,
                        origin = %target.origin,
                        "a mirror push failed; it is retried"
                    );
                    Err(JobFailure::retry(e))
                }
            }
        }
    }

    async fn push(
        &self,
        repo: RepoId,
        target: &JobTarget,
    ) -> Result<forge_git::mirror::MirrorPushReport, ForgeError> {
        match &self.resolver {
            Resolver::Config(cfg) => {
                let url = cfg.policy.check(&target.url).map_err(|e| {
                    ForgeError::new(ErrorCode::PushMirrorFailed)
                        .with_context("reason", "target_not_allowed")
                        .with_context("cause", e.code().id())
                })?;
                let name = CredentialName::parse(&target.credential_name).map_err(|_| {
                    ForgeError::new(ErrorCode::PushMirrorFailed)
                        .with_context("reason", "credential_unreadable")
                        .with_context("detail", "credential_name")
                })?;
                let credential = cfg.credential(&name).map_err(|(problem, cause)| {
                    ForgeError::new(ErrorCode::PushMirrorFailed)
                        .with_context("reason", "credential_unreadable")
                        .with_context("detail", problem.as_str())
                        .with_context("credential_name", name.as_str())
                        .with_context("cause", cause.code().id())
                })?;
                let authorization = credential.authorization();
                let dest = MirrorDestination::Remote {
                    url: &url,
                    authorization: &authorization,
                    ca_file: cfg.ca_file.as_deref(),
                };
                self.git
                    .push_mirror(&self.root, repo, &dest, self.push_timeout)
                    .await
            }
            #[cfg(test)]
            Resolver::Local(path) => {
                self.git
                    .push_mirror(
                        &self.root,
                        repo,
                        &MirrorDestination::Local(path),
                        self.push_timeout,
                    )
                    .await
            }
        }
    }
}

/// The lane's own job runner; `None` without `mirror.credentials_dir` (the
/// start says so when mirrors exist).
pub(crate) async fn lane(
    db: &Db,
    storage: &Storage,
    cfg: &MirrorConfig,
) -> Result<Option<Arc<JobRunner>>, ForgeError> {
    if cfg.credentials_dir.is_none() {
        let report = db::report(db, cfg.lag_alert).await?;
        if report.enabled > 0 {
            tracing::warn!(
                target: "forge_cli",
                event = "forge.worker.mirror_lane_off",
                error_code = ErrorCode::ConfigInvalid.id(),
                config_key = forge_core::config::mirror::CREDENTIALS_DIR_KEY,
                reason = "missing",
                mirrors = report.enabled,
                "push mirrors exist but mirror.credentials_dir is not set: nothing is pushed"
            );
        }
        return Ok(None);
    }
    let git = Git::new(storage.git.config().clone()); // clone: same binary and home, own slots
    let mut runner = JobRunner::new(db.clone(), WorkerId::generate()); // clone: pool handles
    runner.register(Arc::new(MirrorHandler::new(
        db,
        storage.root.clone(), // clone: the handler's own root handle
        git,
        Resolver::Config(cfg.clone()), // clone: the lane owns its settings
        cfg.push_timeout,
    )?))?;
    Ok(Some(Arc::new(runner)))
}

/// One lag sweep: FG-INT-0014 per mirror behind the threshold (module docs).
pub(crate) async fn run_lag(db: &Db, threshold: Duration) -> Result<Next, ForgeError> {
    let lagging = db::lagging(db, threshold).await?;
    if !lagging.is_empty() {
        let request_id = RequestId::generate();
        for m in &lagging {
            let mut e = ForgeError::new(ErrorCode::PushMirrorLagging)
                .with_context("task", LAG_NAME)
                .with_context("repo_id", m.repo.get())
                .with_context("project_id", m.project.get())
                .with_context("pending_since", &m.pending_since)
                .with_context("lag_secs", m.lag_secs)
                .with_context("threshold_secs", threshold.as_secs());
            if let Some(at) = &m.last_success_at {
                e = e.with_context("last_success_at", at);
            }
            if let Some(code) = &m.last_error_code {
                e = e.with_context("last_error_code", code);
            }
            telemetry::log_failure(&e, &request_id);
        }
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
#[path = "mirror_tests.rs"]
mod tests;
