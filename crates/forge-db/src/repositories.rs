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
//!
//! **The key chain.** `jobs_dedup` keeps a job's key while it is queued,
//! running or succeeded; only a dead job releases it (ADR-0016 C6
//! amendment, unchanged here). A repository still `provisioning` whose job
//! *succeeded* (a database restore, a manual state change) would therefore
//! block its own key forever. [`reprovision`] then enqueues under
//! `repository_provision:<repo>:after:<succeeded job id>`, and the current
//! job of a repository is the end of that chain: start at the base key,
//! follow `…:after:<id>` while the holder succeeded ([`CHAIN_CAP`] links).
//! Doctor and `reprovision` read the chain with the same SQL.

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
/// At most this many provisioning repositories are classified per report
/// (the oldest first); beyond them nothing is hidden: `provisioning` counts
/// all of them (capped) and the report says `scanned`.
const REPORT_SCAN: i64 = 10_000;
/// At most this many links of the key chain are followed.
pub const CHAIN_CAP: i32 = 16;

/// The current provisioning job of the repository whose id is `{repo}` (an
/// SQL expression): the end of the key chain (module docs), `(id, state)`
/// of the last non-dead holder, no row when no job holds the base key.
/// `$1` = [`PROVISION_JOB`], `$2` = `"repository_provision:"`. Every step
/// is an equality lookup on `jobs_dedup`.
fn chain_sql(repo: &str) -> String {
    format!(
        "WITH RECURSIVE chain(id, state, n) AS ( \
             SELECT j.id, j.state, 1 FROM ONLY forge.jobs AS j \
              WHERE j.kind = $1 AND j.dedup_key = $2 || ({repo})::text \
                AND j.dedup_key IS NOT NULL AND j.state <> 'dead' \
           UNION ALL \
             SELECT j.id, j.state, c.n + 1 FROM chain AS c JOIN ONLY forge.jobs AS j \
                 ON j.kind = $1 \
                AND j.dedup_key = $2 || ({repo})::text || ':after:' || c.id::text \
                AND j.dedup_key IS NOT NULL AND j.state <> 'dead' \
              WHERE c.state = 'succeeded' AND c.n < {CHAIN_CAP}) \
         SELECT chain.id, chain.state FROM chain ORDER BY chain.n DESC LIMIT 1"
    )
}

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
    provision_job_after(repo, None, request_id)
}

/// [`provision_job`] keyed after a succeeded holder of the key chain:
/// `repository_provision:<repo>:after:<job>` (module docs).
fn provision_job_after(
    repo: RepoId,
    after: Option<JobId>,
    request_id: &RequestId,
) -> Result<NewJob, ForgeError> {
    let kind = JobKind::parse(PROVISION_JOB).ok_or_else(|| constant("invalid_job_kind"))?;
    let timeout =
        JobTimeout::new(PROVISION_TIMEOUT).ok_or_else(|| constant("invalid_job_timeout"))?;
    let dedup_key = jobs::DedupKey::parse(provision_dedup_key(repo, after)?.as_str())
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

fn provision_dedup_key(repo: RepoId, after: Option<JobId>) -> Result<DedupKey, ForgeError> {
    match after {
        None => DedupKey::new(PROVISION_DEDUP_PREFIX, &[DedupPart::Id(repo.get())]),
        Some(job) => DedupKey::new(
            PROVISION_DEDUP_PREFIX,
            &[
                DedupPart::Id(repo.get()),
                DedupPart::Word("after"),
                DedupPart::Id(job.get()),
            ],
        ),
    }
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
    mark_ready_tx(repo, ChangeTx::begin(db, ctx).await?).await
}

/// [`mark_ready`] bounded by `timeout` up to COMMIT instead of the change
/// default (30 s): the provisioning job passes what is left of its own
/// timeout, so an overrun is `FG-DB-0002 reason=change_timeout` (retryable)
/// rather than the job's generic timeout. COMMIT keeps its own bound.
pub async fn mark_ready_within(
    db: &Db,
    ctx: &ChangeContext,
    repo: RepoId,
    timeout: Duration,
) -> Result<MarkReady, ForgeError> {
    let tx = ChangeTx::begin_within(db, ctx, timeout).await?;
    mark_ready_tx(repo, tx).await
}

async fn mark_ready_tx(repo: RepoId, mut tx: ChangeTx) -> Result<MarkReady, ForgeError> {
    let (project_id, kind, state) = tx
        .step(|c| lock_repository(c, repo, "repositories.mark_ready.lock"))
        .await?;
    if state == RepositoryState::Ready {
        // Dropping the transaction rolls it back: nothing was written.
        return Ok(MarkReady::AlreadyReady { project_id });
    }
    let updated = tx
        .step(|c| async move {
            sqlx::query(
                "UPDATE forge.repositories SET state = 'ready', ready_at = now() \
                 WHERE id = $1 AND state = 'provisioning'",
            )
            .bind(repo.get())
            .execute(c)
            .await
            .map_err(|e| query_error("repositories.mark_ready", e))
        })
        .await?
        .rows_affected();
    if updated != 1 {
        // The row is locked and was `provisioning`: anything else is a bug.
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unexpected_row_count")
            .with_context("rows", updated)
            .with_context("repo_id", repo.get()));
    }
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

/// Why a repository is stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckReason {
    /// No queued or running job: dead, missing, or the chain ends at a
    /// succeeded job although the repository is not ready (a database
    /// restore or a manual change).
    JobNotLive,
    /// The job is queued or running, but the repository waits longer than
    /// the threshold (usually: no worker runs).
    TooOld,
}

impl StuckReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JobNotLive => "job_not_live",
            Self::TooOld => "too_old",
        }
    }
}

/// A stuck repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioning {
    pub repo_id: RepoId,
    pub project_id: ProjectId,
    pub age_ms: i64,
    /// The state of the end of its key chain.
    pub job: ProvisionJobState,
    pub reason: StuckReason,
}

/// What `forge doctor` reports about provisioning (`repositories.provisioning`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisioningReport {
    /// Repositories in `provisioning` (capped at `COUNT_CAP + 1`).
    pub provisioning: i64,
    /// Stuck ones among the classified (the [`REPORT_SCAN`] oldest), by reason.
    pub job_not_live: i64,
    pub too_old: i64,
    /// At most [`REPORT_ROWS`] stuck repositories: `job_not_live` first,
    /// then `too_old`, each oldest first.
    pub stuck: Vec<Provisioning>,
    /// More repositories are provisioning than were classified.
    pub truncated: bool,
    /// The threshold the report was made with.
    pub stuck_after: Duration,
}

impl ProvisioningReport {
    /// `FG-REPO-0012` for the first stuck repository (`job_not_live` before
    /// `too_old`, oldest first), with the per-reason counts.
    pub fn check(&self) -> Result<(), ForgeError> {
        let Some(first) = self.stuck.first() else {
            return Ok(());
        };
        let mut err = ForgeError::new(ErrorCode::RepositoryProvisioningStuck)
            .with_context("reason", first.reason.as_str())
            .with_context(
                "stuck",
                shown(self.job_not_live.saturating_add(self.too_old)),
            )
            .with_context("job_not_live", self.job_not_live)
            .with_context("too_old", self.too_old)
            .with_context("repo_id", first.repo_id.get())
            .with_context("project_id", first.project_id.get())
            .with_context("age_ms", first.age_ms)
            .with_context("job_state", first.job.as_str())
            .with_context("threshold_ms", self.stuck_after.as_millis());
        if first.job == ProvisionJobState::Succeeded {
            err = err.with_context(
                "detail",
                "its job succeeded but the repository is not ready (database restore or \
                 manual change): `forge admin repo reprovision` enqueues a new one",
            );
        }
        if self.truncated {
            err = err.with_context("truncated", true);
        }
        Err(err)
    }

    /// `"<n> provisioning, none stuck"` for the ok line.
    pub fn summary(&self) -> String {
        let more = if self.truncated {
            format!(" (the oldest {REPORT_SCAN} checked)")
        } else {
            String::new()
        };
        format!(
            "{} provisioning, none stuck{more}",
            shown(self.provisioning)
        )
    }
}

type StuckRow = (i64, i64, i64, Option<String>, String, i64);

/// The repositories in `provisioning` and the stuck ones (read pool;
/// `None` before the D4 schema). The [`REPORT_SCAN`] oldest are classified
/// in SQL by the end of their key chain (module docs): no queued / running
/// job → `job_not_live`, else older than `stuck_after` → `too_old`; the
/// counts per reason and the first [`REPORT_ROWS`] (`job_not_live` first,
/// each oldest first) come back.
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
        let sql = format!(
            "WITH p AS ( \
                 SELECT r.id, r.project_id, \
                        (EXTRACT(epoch FROM pg_catalog.now() - r.created_at) * 1000)::bigint \
                          AS age_ms, \
                        h.state AS job \
                 FROM (SELECT id, project_id, created_at FROM forge.repositories \
                        WHERE state = 'provisioning' ORDER BY created_at, id LIMIT $4) AS r \
                 LEFT JOIN LATERAL ({chain}) AS h ON true), \
             s AS ( \
                 SELECT p.*, CASE WHEN p.job IS NULL OR p.job NOT IN ('queued', 'running') \
                                  THEN 'job_not_live' \
                                  WHEN p.age_ms > $3 THEN 'too_old' END AS reason \
                 FROM p) \
             SELECT s.id, s.project_id, s.age_ms, s.job, s.reason, \
                    pg_catalog.count(*) OVER (PARTITION BY s.reason) \
             FROM s WHERE s.reason IS NOT NULL \
             ORDER BY (s.reason = 'job_not_live') DESC, s.age_ms DESC, s.id LIMIT $5",
            chain = chain_sql("r.id")
        );
        let threshold_ms = i64::try_from(stuck_after.as_millis()).unwrap_or(i64::MAX);
        let rows: Vec<StuckRow> = sqlx::query_as(&sql)
            .bind(PROVISION_JOB)
            .bind(format!("{PROVISION_DEDUP_PREFIX}:"))
            .bind(threshold_ms)
            .bind(REPORT_SCAN)
            .bind(REPORT_ROWS)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("repositories.provisioning_report", e))?;
        let (mut job_not_live, mut too_old) = (0, 0);
        let mut stuck = Vec::with_capacity(rows.len());
        for (id, project, age_ms, job, reason, count) in rows {
            let reason = match reason.as_str() {
                "job_not_live" => {
                    job_not_live = count;
                    StuckReason::JobNotLive
                }
                _ => {
                    too_old = count;
                    StuckReason::TooOld
                }
            };
            stuck.push(Provisioning {
                repo_id: RepoId::new(id),
                project_id: ProjectId::new(project),
                age_ms,
                job: ProvisionJobState::parse(job.as_deref()),
                reason,
            });
        }
        Ok(Some(ProvisioningReport {
            provisioning,
            job_not_live,
            too_old,
            stuck,
            truncated: provisioning > REPORT_SCAN,
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
    /// A new provisioning job was enqueued (audited). `after`: the
    /// succeeded job whose key it follows (module docs, the key chain).
    Queued {
        project_id: ProjectId,
        job: JobId,
        after: Option<JobId>,
    },
    /// The current job of the key chain is queued or running: nothing was
    /// written.
    AlreadyLive {
        project_id: ProjectId,
        job: JobId,
        state: ProvisionJobState,
    },
}

/// The end of `repo`'s key chain, read in the change (under its row lock).
async fn current_job(
    conn: &mut PgConnection,
    repo: RepoId,
) -> Result<Option<(JobId, ProvisionJobState)>, ForgeError> {
    let row: Option<(i64, String)> = sqlx::query_as(&chain_sql("$3"))
        .bind(PROVISION_JOB)
        .bind(format!("{PROVISION_DEDUP_PREFIX}:"))
        .bind(repo.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("repositories.reprovision.job", e))?;
    Ok(row.map(|(id, state)| (JobId::new(id), ProvisionJobState::parse(Some(&state)))))
}

/// Enqueue the provisioning job of `repo` again (`forge admin repo
/// reprovision`). The repository row is locked; it must still be
/// `provisioning`, else `FG-REPO-0014 reason=repository_ready`. Then, by
/// the current job of its key chain (module docs): queued / running →
/// [`Reprovisioned::AlreadyLive`] (nothing written); none (dead or never
/// there) → a new job with that link's key; succeeded (a restore or a
/// manual change left the repository behind) → a new job keyed
/// `repository_provision:<repo>:after:<that job>`. The job and a
/// `repository.reprovision_requested` audit row on the project (with the
/// succeeded holder, if any) are one change. Concurrent calls serialize
/// on the row lock, so they enqueue once.
///
/// Errors: `FG-REPO-0005` (`resource=repository`) for an unknown id,
/// `FG-REPO-0014 repository_ready`, `FG-SYS-0001 unexpected_job_dedup`,
/// the change's own.
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
    let after = match tx.step(|c| current_job(c, repo)).await? {
        None => None,
        Some((job, state)) if state.is_live() => {
            return Ok(Reprovisioned::AlreadyLive {
                project_id,
                job,
                state,
            })
        }
        Some((job, _succeeded)) => Some(job),
    };
    let job = provision_job_after(repo, after, &ctx.request_id)?;
    let job = match tx.enqueue_job(&job).await? {
        Enqueued::Queued(id) => id,
        // Under the row lock the chain said the key is free: a holder now
        // means the chain is longer than CHAIN_CAP or a writer skipped the lock.
        Enqueued::Deduped => {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unexpected_job_dedup")
                .with_context("repo_id", repo.get()))
        }
    };
    let details = AuditDetails::RepositoryReprovisionRequested {
        repository_id: repo,
        job_id: job.get(),
        after_succeeded_job: after.map(JobId::get),
    };
    let mut record = ChangeRecord::audit_only(&details, RESOURCE, project_id.get());
    // The event scope rule: an audit row on a project names that project.
    record.audit.project_id = Some(project_id);
    tx.commit(record)
        .await?
        .require_recorded(details.action())?;
    Ok(Reprovisioned::Queued {
        project_id,
        job,
        after,
    })
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
    fn a_job_after_a_succeeded_holder_names_it_in_its_key() {
        let request = RequestId::parse("req-8").expect("request id");
        let job = provision_job_after(RepoId::new(42), Some(JobId::new(7)), &request).expect("job");
        assert_eq!(
            job.dedup_key.as_ref().map(jobs::DedupKey::as_str),
            Some("repository_provision:42:after:7")
        );
        assert_eq!(job.payload, serde_json::json!({ "repo_id": 42 }));
    }

    #[test]
    fn the_report_names_the_first_stuck_repository_with_the_counts() {
        let report = |stuck: Vec<Provisioning>, job_not_live, too_old| ProvisioningReport {
            provisioning: 3,
            job_not_live,
            too_old,
            stuck,
            truncated: false,
            stuck_after: PROVISIONING_STUCK_AFTER,
        };
        assert!(report(Vec::new(), 0, 0).check().is_ok());
        let dead = Provisioning {
            repo_id: RepoId::new(5),
            project_id: ProjectId::new(4),
            age_ms: 1_000,
            job: ProvisionJobState::None,
            reason: StuckReason::JobNotLive,
        };
        let err = report(vec![dead.clone()], 1, 2).check().expect_err("stuck");
        assert_eq!(err.code(), ErrorCode::RepositoryProvisioningStuck);
        let text = err.to_string();
        for part in [
            "reason=job_not_live",
            "stuck=3",
            "job_not_live=1",
            "too_old=2",
            "repo_id=5",
            "project_id=4",
            "job_state=none",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        assert!(!text.contains("detail="), "{text}");
        let succeeded = Provisioning {
            job: ProvisionJobState::Succeeded,
            ..dead.clone()
        };
        let text = report(vec![succeeded], 1, 0)
            .check()
            .expect_err("stuck")
            .to_string();
        assert!(text.contains("job_state=succeeded"), "{text}");
        assert!(text.contains("database restore or manual change"), "{text}");
        let old = Provisioning {
            job: ProvisionJobState::Queued,
            age_ms: 700_000,
            reason: StuckReason::TooOld,
            ..dead
        };
        let err = report(vec![old], 0, 1).check().expect_err("stuck");
        assert!(err.to_string().contains("reason=too_old"), "{err}");
    }
}

#[cfg(test)]
#[path = "repositories_tests.rs"]
mod db_tests;
