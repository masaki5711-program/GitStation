//! The `repository.provision` job handler (step D4 MR3b, option A): create
//! the bare repository of a new project, then mark it ready.
//!
//! One run: read the repository's project, state and provision token (write
//! pool), `StorageRoot::init_bare_or_adopt` with that token (a retry after
//! a crash adopts its own marked directory, ADR-0009 Amendment D4 MR3a),
//! then `repositories::mark_ready` (audit row + `repository.created`
//! event, a no-op when already ready). Every step is idempotent, so the
//! at-least-once delivery of the job queue is safe.
//!
//! Success is logged once here (`forge.repo.provisioned`: `repo_id`,
//! `project_id`, `outcome` created | adopted | already_ready,
//! `duration_ms`); a failure is returned to the job runner, which logs it
//! once with the job's origin `request_id`. Retry: storage failures follow
//! `forge_git::storage::provision_error_is_retryable` (git timeout / busy /
//! missing, storage I/O → retry; `FG-REPO-0004` `adopt_refused`, a rejected
//! path, an invalid hooks path → dead at once, an operator must look),
//! database failures retry when the database was unreachable or a query
//! failed (`FG-DB-0001` / `FG-DB-0002`), anything else is permanent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::RepositoryState;
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::repositories::{self, MarkReady, PROVISION_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::storage::provision_error_is_retryable;
use forge_git::{Git, GitConfig, Provisioned, StorageRoot};

/// One git invocation of the handler: well inside the job's 60 s, so a hung
/// git is a retryable `FG-GIT-0002` rather than an aborted run.
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// Where repositories are created: the storage root and the git that
/// creates them.
#[derive(Clone)]
pub(crate) struct Storage {
    pub(crate) root: StorageRoot,
    pub(crate) git: Git,
    /// The forge-owned hooks directory (`core.hooksPath`). None until the
    /// policy hooks exist (M1 phase 10 adds the setting).
    pub(crate) hooks: Option<PathBuf>,
}

impl Storage {
    /// The configured root (checked: a real directory) and git from `PATH`
    /// with HOME pinned to the storage root, which only forge writes
    /// (ADR-0009 Amendment D4 MR3b): no user or shared git config applies.
    pub(crate) fn open(root: &std::path::Path) -> Result<Self, ForgeError> {
        let root = StorageRoot::open(root)?;
        let mut cfg = GitConfig::new("git", root.path());
        cfg.timeout_total = GIT_TIMEOUT;
        Ok(Self {
            git: Git::new(cfg),
            root,
            hooks: None,
        })
    }
}

/// The handler of `repository.provision`.
pub(crate) struct ProvisionHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    storage: Storage,
}

impl ProvisionHandler {
    pub(crate) fn new(db: &Db, storage: Storage) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(PROVISION_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", PROVISION_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: pool handles
                storage,
            }),
        })
    }
}

impl JobHandler for ProvisionHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.provision(job).await })
    }
}

/// A failed database step: retried when the database may answer next time.
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

/// A failed `init_bare_or_adopt` (ADR-0009 Amendment D4 MR3a).
fn storage_failure(e: ForgeError) -> JobFailure {
    if provision_error_is_retryable(&e) {
        JobFailure::retry(e)
    } else {
        JobFailure::permanent(e)
    }
}

impl Inner {
    async fn provision(&self, job: ClaimedJob) -> Result<(), JobFailure> {
        let started = Instant::now();
        let request_id = job.request_id();
        let repo = repositories::provision_payload(&job.payload).map_err(JobFailure::permanent)?;
        let target = repositories::provision_target(&self.db, repo)
            .await
            .map_err(db_failure)?;
        let outcome = if target.state == RepositoryState::Ready {
            // An earlier attempt marked it ready and lost its lease before
            // completing: nothing left to do, never touch the directory.
            "already_ready"
        } else {
            let s = &self.storage;
            let made = s
                .root
                .init_bare_or_adopt(&s.git, repo, target.token, s.hooks.as_deref())
                .await
                .map_err(storage_failure)?;
            let mut ctx = ChangeContext::new(Actor::System, request_id.clone());
            ctx.actor_label = Some(ACTOR_LABEL.to_owned());
            match repositories::mark_ready(&self.db, &ctx, repo)
                .await
                .map_err(db_failure)?
            {
                // A concurrent run marked it between the read and here.
                MarkReady::AlreadyReady { .. } => "already_ready",
                MarkReady::Marked { .. } => match made {
                    Provisioned::Created(_) => "created",
                    Provisioned::Adopted(_) => "adopted",
                },
            }
        };
        tracing::info!(
            target: "forge_cli",
            event = "forge.repo.provisioned",
            request_id = %request_id,
            repo_id = repo.get(),
            project_id = target.project_id.get(),
            outcome,
            attempt = job.lease.attempt,
            duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "repository provisioned"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "provision_tests.rs"]
pub(crate) mod tests;
