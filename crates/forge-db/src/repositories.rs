//! Repositories (step D4, ADR-0004, ADR-0009): one per project in M1. A
//! repository row is written by `projects::create` in state `provisioning`
//! together with the job that creates its bare directory (D4 option A).
//!
//! - [`provision_job`] is that job (`repository.provision`, payload
//!   `{"repo_id"}`, dedup `repository_provision:<repo_id>`).
//! - [`provision_target`] is what its handler (forge-cli worker) reads
//!   first: the project, the state and the provision token (write pool:
//!   `forge_ro` cannot read the token, ADR-0018).
//! - [`mark_ready`] ends provisioning: state `ready` + `ready_at`, one
//!   `repository.created` audit row on the project and one
//!   `repository.created` event (dedup `repository_created:<repo_id>`). A
//!   second call is a no-op.
//! - [`provisioning_report`] / [`ProvisioningReport::check`] are what
//!   `forge doctor` reports (`repositories.provisioning`, `FG-REPO-0012`),
//!   [`ready_ids`] pages through ready repositories (`repositories.storage`).
//! - [`reprovision`] enqueues the job again for a repository still
//!   `provisioning` (`forge admin repo reprovision`, an approved runbook
//!   step; audited as `repository.reprovision_requested`).

use std::time::Duration;

use forge_core::ids::{ProjectId, RepoId};
use forge_core::{AuditDetails, DedupKey, DedupPart, RepositoryKind, RepositoryState, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, DomainEvent, Outcome};
use crate::jobs::{self, Enqueued, JobId, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::sequencer::{shown, COUNT_CAP};
use crate::{query_error, read_query_error, Db};

/// The job kind that creates a repository's bare directory
/// (`init_bare_or_adopt`, then `mark_ready`). Payload: `{"repo_id": <id>}`.
pub const PROVISION_JOB: &str = "repository.provision";
/// One run of the provisioning job (step D plan: 60 s).
pub const PROVISION_TIMEOUT: Duration = Duration::from_secs(60);
/// The prefix of its dedup key, `repository_provision:<repo_id>`
/// (`<aggregate>_<verb>`, ADR-0016 Amendment D4).
pub const PROVISION_DEDUP_PREFIX: &str = "repository_provision";
/// The prefix of the `repository.created` event's dedup key,
/// `repository_created:<repo_id>`.
pub const CREATED_DEDUP_PREFIX: &str = "repository_created";
/// A repository still `provisioning` after this long is stuck (step D plan
/// answer 4: alert after 10 minutes).
pub const PROVISIONING_STUCK_AFTER: Duration = Duration::from_secs(600);
/// At most this many provisioning repositories are read per report (the
/// oldest first; the count of all of them is capped at `COUNT_CAP + 1`).
const REPORT_ROWS: i64 = 100;
/// The largest page [`ready_ids`] returns.
pub const READY_PAGE_MAX: i64 = 1000;

/// The audit resource type of a repository's changes: its project
/// (tables.toml; `forge.audited_change_present('project', 'project_id')`).
const RESOURCE: &str = "project";
/// The event aggregate of `repository.created`.
const AGGREGATE: &str = "repository";

/// `FG-SYS-0001` for a constant that does not fit its type (a bug here).
fn constant(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

fn not_found(repo: RepoId) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "repository")
        .with_context("reason", "absent")
        .with_context("repo_id", repo.get())
}

/// A stored value outside its CHECK (only if the constraint is gone).
fn corrupt(reason: &'static str, repo: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("repo_id", repo)
}

fn state_of(raw: &str, repo: i64) -> Result<RepositoryState, ForgeError> {
    RepositoryState::parse(raw).ok_or_else(|| corrupt("unknown_repository_state", repo))
}

/// The provisioning job of `repo`, for the request that wants it. Its
/// dedup key is built with [`DedupKey::new`] from the repository id only,
/// so a second enqueue for the same repository is deduplicated while the
/// first job is queued, running or succeeded (a dead one does not hold it:
/// [`reprovision`] enqueues again).
pub(crate) fn provision_job(repo: RepoId, request_id: &RequestId) -> Result<NewJob, ForgeError> {
    let kind = JobKind::parse(PROVISION_JOB).ok_or_else(|| constant("invalid_job_kind"))?;
    let timeout =
        JobTimeout::new(PROVISION_TIMEOUT).ok_or_else(|| constant("invalid_job_timeout"))?;
    let dedup_key = jobs::DedupKey::parse(provision_dedup_key(repo)?.as_str())
        .ok_or_else(|| constant("invalid_job_dedup_key"))?;
    Ok(NewJob {
        kind,
        payload: serde_json::json!({ "repo_id": repo.get() }),
        max_attempts: MaxAttempts::DEFAULT,
        timeout,
        dedup_key: Some(dedup_key),
        origin_request_id: request_id.clone(),
        origin_event_seq: None,
    })
}

fn provision_dedup_key(repo: RepoId) -> Result<DedupKey, ForgeError> {
    DedupKey::new(PROVISION_DEDUP_PREFIX, &[DedupPart::Id(repo.get())])
}

/// The repository id of a provisioning job's payload `{"repo_id": <id>}`;
/// anything else is `FG-SYS-0001 reason=invalid_job_payload` (the payload
/// is written by [`provision_job`] only).
pub fn provision_payload(payload: &serde_json::Value) -> Result<RepoId, ForgeError> {
    payload
        .get("repo_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .map(RepoId::new)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_payload")
                .with_context("kind", PROVISION_JOB)
        })
}

/// What the provisioning job needs about its repository.
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisionTarget {
    pub repo_id: RepoId,
    pub project_id: ProjectId,
    pub state: RepositoryState,
    /// `repositories.provision_token`: the marker `init_bare` writes into
    /// the repository's config (ADR-0009 Amendment D4 MR3a). Never logged.
    pub token: uuid::Uuid,
}

impl std::fmt::Debug for ProvisionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProvisionTarget")
            .field("repo_id", &self.repo_id)
            .field("project_id", &self.project_id)
            .field("state", &self.state)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The project, state and provision token of `repo`, read on the write
/// pool (`forge_ro` has no SELECT on `provision_token`, ADR-0018).
/// `FG-REPO-0005` (`resource=repository`, `reason=absent`) when there is no
/// such repository.
pub async fn provision_target(db: &Db, repo: RepoId) -> Result<ProvisionTarget, ForgeError> {
    let row: Option<(i64, String, String)> = db
        .within("repositories.provision_target", async {
            sqlx::query_as(
                "SELECT project_id, state, provision_token::text FROM forge.repositories \
                 WHERE id = $1",
            )
            .bind(repo.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error("repositories.provision_target", e))
        })
        .await?;
    let Some((project_id, state, token)) = row else {
        return Err(not_found(repo));
    };
    Ok(ProvisionTarget {
        repo_id: repo,
        project_id: ProjectId::new(project_id),
        state: state_of(&state, repo.get())?,
        token: uuid::Uuid::parse_str(&token)
            .map_err(|_| corrupt("invalid_provision_token", repo.get()))?,
    })
}

/// What [`mark_ready`] did.
#[must_use = "AlreadyReady wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkReady {
    /// `provisioning` → `ready`, audited, `repository.created` published.
    Marked { project_id: ProjectId },
    /// The repository was ready already: nothing was written.
    AlreadyReady { project_id: ProjectId },
}

impl MarkReady {
    pub fn project_id(self) -> ProjectId {
        match self {
            Self::Marked { project_id } | Self::AlreadyReady { project_id } => project_id,
        }
    }
}

/// The repository row, locked for the change (`FOR UPDATE`: forge_app
/// holds UPDATE on state / ready_at).
async fn lock_repository(
    conn: &mut PgConnection,
    repo: RepoId,
    query: &'static str,
) -> Result<(ProjectId, RepositoryKind, RepositoryState), ForgeError> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT project_id, kind, state FROM forge.repositories WHERE id = $1 FOR UPDATE",
    )
    .bind(repo.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error(query, e))?;
    let Some((project, kind, state)) = row else {
        return Err(not_found(repo));
    };
    let kind = RepositoryKind::parse(&kind)
        .ok_or_else(|| corrupt("unknown_repository_kind", repo.get()))?;
    Ok((ProjectId::new(project), kind, state_of(&state, repo.get())?))
}

/// Mark `repo` ready (its bare directory exists): state `ready`,
/// `ready_at = now()`, one `repository.created` audit row on resource
/// (`project`, project id) with `project_id` set, and one
/// `repository.created` event of that project (integration class, payload
/// ids and kind, dedup `repository_created:<repo_id>`) — one transaction.
/// The row is locked first, so concurrent calls serialize; an already
/// ready repository is [`MarkReady::AlreadyReady`] and writes nothing (no
/// second event). `ctx` is the provisioning job's (actor `System`, the
/// job's origin request id).
///
/// Errors: `FG-REPO-0005` (`resource=repository`) for an unknown id, the
/// change's own (`FG-DB-0002`, `FG-DB-0010`, …).
pub async fn mark_ready(
    db: &Db,
    ctx: &ChangeContext,
    repo: RepoId,
) -> Result<MarkReady, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let (project_id, kind, state) = tx
        .step(|c| lock_repository(c, repo, "repositories.mark_ready.lock"))
        .await?;
    if state == RepositoryState::Ready {
        // Dropping the transaction rolls it back: nothing was written.
        return Ok(MarkReady::AlreadyReady { project_id });
    }
    tx.step(|c| async move {
        sqlx::query(
            "UPDATE forge.repositories SET state = 'ready', ready_at = now() \
             WHERE id = $1 AND state = 'provisioning'",
        )
        .bind(repo.get())
        .execute(c)
        .await
        .map_err(|e| query_error("repositories.mark_ready", e))
    })
    .await?;
    let details = AuditDetails::RepositoryCreated {
        project_id,
        repository_id: repo,
        kind,
    };
    let event_type = details
        .event_type()
        .ok_or_else(|| constant("audit_only_event"))?;
    let value = details.to_value();
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(project_id.get()),
            project_id: Some(project_id),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the event payload
        },
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: AGGREGATE,
            aggregate_id: repo.get(),
            project_id: Some(project_id),
            payload: value,
            dedup_key: Some(DedupKey::new(
                CREATED_DEDUP_PREFIX,
                &[DedupPart::Id(repo.get())],
            )?),
        }],
    };
    match tx.commit(record).await? {
        crate::change::Committed::Recorded { .. } => Ok(MarkReady::Marked { project_id }),
        // The row lock makes a second writer see `ready` above; a recorded
        // key with a provisioning row means the event outlived its change.
        crate::change::Committed::Deduped => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unexpected_dedup")
            .with_context("action", details.action())
            .with_context("repo_id", repo.get())),
    }
}

/// The state of the newest live provisioning job of a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionJobState {
    Queued,
    Running,
    /// Succeeded although the repository is still `provisioning` (a bug or
    /// a manual change: the job no longer runs).
    Succeeded,
    /// No queued, running or succeeded job: dead or missing.
    None,
}

impl ProvisionJobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::None => "none",
        }
    }

    fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("queued") => Self::Queued,
            Some("running") => Self::Running,
            Some("succeeded") => Self::Succeeded,
            _ => Self::None,
        }
    }

    /// Whether a worker will still run it.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

/// A repository still `provisioning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioning {
    pub repo_id: RepoId,
    pub project_id: ProjectId,
    pub age_ms: i64,
    pub job: ProvisionJobState,
}

impl Provisioning {
    /// Stuck: no live job, or older than the threshold.
    fn stuck(&self, threshold_ms: i64) -> bool {
        !self.job.is_live() || self.age_ms > threshold_ms
    }
}

/// What `forge doctor` reports about provisioning (`repositories.provisioning`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisioningReport {
    /// Repositories in `provisioning` (capped at `COUNT_CAP + 1`).
    pub provisioning: i64,
    /// The stuck ones among the [`REPORT_ROWS`] oldest, oldest first.
    pub stuck: Vec<Provisioning>,
    /// The threshold the report was made with.
    pub stuck_after: Duration,
}

impl ProvisioningReport {
    /// `FG-REPO-0012` for the oldest stuck repository: `reason=job_not_live`
    /// (its job is dead, missing or succeeded) or `too_old` (still queued or
    /// running, but waiting longer than the threshold).
    pub fn check(&self) -> Result<(), ForgeError> {
        let Some(oldest) = self.stuck.first() else {
            return Ok(());
        };
        let reason = if oldest.job.is_live() {
            "too_old"
        } else {
            "job_not_live"
        };
        Err(ForgeError::new(ErrorCode::RepositoryProvisioningStuck)
            .with_context("reason", reason)
            .with_context("stuck", self.stuck.len())
            .with_context("repo_id", oldest.repo_id.get())
            .with_context("project_id", oldest.project_id.get())
            .with_context("age_ms", oldest.age_ms)
            .with_context("job_state", oldest.job.as_str())
            .with_context("threshold_ms", self.stuck_after.as_millis()))
    }

    /// `"<n> provisioning"` for the ok line.
    pub fn summary(&self) -> String {
        format!("{} provisioning, none stuck", shown(self.provisioning))
    }
}

/// The repositories in `provisioning` and the stuck ones among the oldest
/// [`REPORT_ROWS`] (older than `stuck_after`, or without a queued / running
/// provisioning job), read on the read pool; `None` before the D4 schema.
/// The job is found by its dedup key through the live-key index
/// (`jobs_dedup`: queued, running and succeeded jobs), so a dead job is
/// seen as "no live job" without scanning the queue.
pub async fn provisioning_report(
    db: &Db,
    stuck_after: Duration,
) -> Result<Option<ProvisioningReport>, ForgeError> {
    db.within("repositories.provisioning_report", async {
        let mut conn = db.acquire().await?;
        let migrated: bool =
            sqlx::query_scalar("SELECT pg_catalog.to_regclass('forge.repositories') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| read_query_error("repositories.provisioning_report", e))?;
        if !migrated {
            return Ok(None);
        }
        let provisioning: i64 = sqlx::query_scalar(
            "SELECT pg_catalog.count(*) FROM (SELECT 1 FROM forge.repositories \
             WHERE state = 'provisioning' LIMIT $1) AS w",
        )
        .bind(COUNT_CAP + 1)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("repositories.provisioning_count", e))?;
        let rows: Vec<(i64, i64, i64, Option<String>)> = sqlx::query_as(
            "SELECT r.id, r.project_id, \
                    (EXTRACT(epoch FROM pg_catalog.now() - r.created_at) * 1000)::bigint, \
                    (SELECT j.state FROM ONLY forge.jobs AS j \
                      WHERE j.kind = $1 AND j.dedup_key = $2 || r.id::text \
                        AND j.dedup_key IS NOT NULL AND j.state <> 'dead' LIMIT 1) \
             FROM forge.repositories AS r WHERE r.state = 'provisioning' \
             ORDER BY r.created_at, r.id LIMIT $3",
        )
        .bind(PROVISION_JOB)
        .bind(format!("{PROVISION_DEDUP_PREFIX}:"))
        .bind(REPORT_ROWS)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("repositories.provisioning_report", e))?;
        let threshold_ms = i64::try_from(stuck_after.as_millis()).unwrap_or(i64::MAX);
        let stuck = rows
            .into_iter()
            .map(|(id, project, age_ms, job)| Provisioning {
                repo_id: RepoId::new(id),
                project_id: ProjectId::new(project),
                age_ms,
                job: ProvisionJobState::parse(job.as_deref()),
            })
            .filter(|p| p.stuck(threshold_ms))
            .collect();
        Ok(Some(ProvisioningReport {
            provisioning,
            stuck,
            stuck_after,
        }))
    })
    .await
}

/// Up to `limit` (1..=[`READY_PAGE_MAX`]) ready repositories with an id
/// above `after`, in id order (keyset paging), read on the read pool.
pub async fn ready_ids(
    db: &Db,
    after: Option<RepoId>,
    limit: i64,
) -> Result<Vec<RepoId>, ForgeError> {
    let limit = limit.clamp(1, READY_PAGE_MAX);
    let ids: Vec<i64> = db
        .within("repositories.ready_ids", async {
            sqlx::query_scalar(
                "SELECT id FROM forge.repositories WHERE state = 'ready' AND id > $1 \
                 ORDER BY id LIMIT $2",
            )
            .bind(after.map_or(0, RepoId::get))
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error("repositories.ready_ids", e))
        })
        .await?;
    Ok(ids.into_iter().map(RepoId::new).collect())
}

/// What [`reprovision`] did.
#[must_use = "AlreadyLive wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reprovisioned {
    /// A new provisioning job was enqueued (audited).
    Queued { project_id: ProjectId, job: JobId },
    /// A queued, running or succeeded provisioning job holds the key:
    /// nothing was written.
    AlreadyLive { project_id: ProjectId },
}

/// Enqueue the provisioning job of `repo` again (`forge admin repo
/// reprovision`, after its job died or vanished). The repository row is
/// locked; it must still be `provisioning`, else `FG-REPO-0014
/// reason=repository_ready`. The job (dedup `repository_provision:<id>`,
/// `ctx`'s request id as origin) and a `repository.reprovision_requested`
/// audit row on the project are one change; when a live job holds the key
/// nothing is written ([`Reprovisioned::AlreadyLive`]).
///
/// Errors: `FG-REPO-0005` (`resource=repository`) for an unknown id,
/// `FG-REPO-0014 repository_ready`, the change's own.
pub async fn reprovision(
    db: &Db,
    ctx: &ChangeContext,
    repo: RepoId,
) -> Result<Reprovisioned, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let (project_id, _kind, state) = tx
        .step(|c| lock_repository(c, repo, "repositories.reprovision.lock"))
        .await?;
    if state == RepositoryState::Ready {
        return Err(ForgeError::new(ErrorCode::ProjectRequestInvalid)
            .with_context("reason", "repository_ready")
            .with_context("repo_id", repo.get()));
    }
    let job = provision_job(repo, &ctx.request_id)?;
    let job = match tx.enqueue_job(&job).await? {
        Enqueued::Queued(id) => id,
        // Dropping the transaction rolls it back: nothing was written.
        Enqueued::Deduped => return Ok(Reprovisioned::AlreadyLive { project_id }),
    };
    let details = AuditDetails::RepositoryReprovisionRequested {
        repository_id: repo,
        job_id: job.get(),
    };
    let mut record = ChangeRecord::audit_only(&details, RESOURCE, project_id.get());
    // The event scope rule: an audit row on a project names that project.
    record.audit.project_id = Some(project_id);
    tx.commit(record)
        .await?
        .require_recorded(details.action())?;
    Ok(Reprovisioned::Queued { project_id, job })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provision_job_carries_the_repo_id_and_its_dedup_key() {
        let request = RequestId::parse("req-7").expect("request id");
        let job = provision_job(RepoId::new(42), &request).expect("job");
        assert_eq!(job.kind.as_str(), "repository.provision");
        assert_eq!(job.payload, serde_json::json!({ "repo_id": 42 }));
        assert_eq!(
            job.dedup_key.as_ref().map(jobs::DedupKey::as_str),
            Some("repository_provision:42")
        );
        assert_eq!(job.timeout.get(), Duration::from_secs(60));
        assert_eq!(job.max_attempts, MaxAttempts::DEFAULT);
        assert_eq!(job.origin_request_id.as_str(), "req-7");
        assert_eq!(
            provision_payload(&job.payload).expect("payload"),
            RepoId::new(42)
        );
    }

    #[test]
    fn a_malformed_payload_is_an_internal_error() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "repo_id": "42" }),
            serde_json::json!({ "repo_id": 0 }),
            serde_json::json!({ "repo_id": -1 }),
            serde_json::json!(null),
        ] {
            let err = provision_payload(&bad).expect_err("rejected");
            assert_eq!(err.code(), ErrorCode::Internal, "{bad}");
            assert!(err.to_string().contains("invalid_job_payload"), "{err}");
        }
    }

    #[test]
    fn the_report_names_the_oldest_stuck_repository() {
        let report = |stuck: Vec<Provisioning>| ProvisioningReport {
            provisioning: 3,
            stuck,
            stuck_after: PROVISIONING_STUCK_AFTER,
        };
        assert!(report(Vec::new()).check().is_ok());
        let dead = Provisioning {
            repo_id: RepoId::new(5),
            project_id: ProjectId::new(4),
            age_ms: 1_000,
            job: ProvisionJobState::None,
        };
        let err = report(vec![dead.clone()]).check().expect_err("stuck");
        assert_eq!(err.code(), ErrorCode::RepositoryProvisioningStuck);
        let text = err.to_string();
        for part in [
            "reason=job_not_live",
            "repo_id=5",
            "project_id=4",
            "job_state=none",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        let old = Provisioning {
            job: ProvisionJobState::Queued,
            age_ms: 700_000,
            ..dead
        };
        let err = report(vec![old]).check().expect_err("stuck");
        assert!(err.to_string().contains("reason=too_old"), "{err}");
        // Live and young is not stuck; live and old is; dead is at any age.
        let young = Provisioning {
            repo_id: RepoId::new(1),
            project_id: ProjectId::new(1),
            age_ms: 10,
            job: ProvisionJobState::Running,
        };
        assert!(!young.stuck(600_000));
        assert!(young.stuck(5));
        assert!(Provisioning {
            job: ProvisionJobState::Succeeded,
            ..young
        }
        .stuck(600_000));
    }
}

#[cfg(test)]
#[path = "repositories_tests.rs"]
mod db_tests;
