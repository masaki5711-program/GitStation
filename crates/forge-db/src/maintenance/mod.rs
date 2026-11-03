//! The repository maintenance ledger and schedule (M3 O3, plan 11 §5 O3,
//! ADR-0024 "実装で決めたこと（O3）"). The git steps are forge-git's
//! (`maintenance`); the job handler and its own runner are `forge worker`'s
//! (`worker/maintenance.rs`).
//!
//! - [`schedule_due`]: the ready repositories whose `next_due_at` passed
//!   (never maintained: since `ready_at`), oldest first, at most `limit`,
//!   each enqueued as one `repository.maintain` job keyed
//!   `repository_maintain:<repo_id>:<due epoch>` — a queued or running job
//!   of the same due time holds the key, so a pass never doubles one. The
//!   worker calls it only inside the nightly window ([`local_minute`]).
//! - [`enqueue_now`]: `forge admin repo maintain <repo_id>` (an operator,
//!   key `…:admin:<request_id>`).
//! - [`start`] / [`finish`]: the job's ledger writes. `start` returns
//!   `None` for a repository that is not `ready` (provisioning, purging,
//!   gone): the job does nothing. `finish` counts consecutive failures
//!   (reset by a success) and sets the next due time.
//! - [`report`]: `forge doctor` (`repositories.maintenance`,
//!   `repositories.integrity`, `repositories.refs`).
//!
//! The ledger is operational state (tables.toml `derived`): no audit row.
//! The project purge deletes a project's rows.

use std::time::Duration;

use forge_core::ids::{ProjectId, RepoId};
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use crate::jobs::{
    enqueue, DedupKey, Enqueued, JobKind, JobTimeout, MaxAttempts, NewJob, LEASE_SLACK,
};
use crate::{query_error, read_query_error, Db};

/// The kind of the maintenance job.
pub const MAINTAIN_JOB: &str = "repository.maintain";
/// The prefix of its dedup key.
pub const MAINTAIN_DEDUP_PREFIX: &str = "repository_maintain";
/// One run of the job: the queue's maximum (its lease is this + the
/// queue's slack, `jobs::LEASE_SLACK`). Every step runs under the run's
/// deadline ([`RUN_RESERVE`] before this), so no git outlives the lease.
pub const MAINTAIN_JOB_TIMEOUT: Duration = JobTimeout::MAX;
/// Kept from the job's timeout for the ledger writes after the last step.
pub const RUN_RESERVE: Duration = Duration::from_secs(60);
/// Attempts of one maintenance job: a step's own failure is recorded in
/// the ledger and completes the job (the next run is the retry); only a
/// crashed run or a ledger write that failed is retried by the queue.
pub const MAINTAIN_ATTEMPTS: u8 = 3;
/// The most rows of each kind [`report`] lists.
pub const REPORT_LIST_MAX: i64 = 20;

/// What a run did, as the ledger stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// Nothing done (`skipped_reason` says why).
    Skipped,
    /// A step failed (`error_code`).
    Failed,
    /// fsck found a problem (FG-GIT-0019).
    Corrupt,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Corrupt => "corrupt",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "succeeded" => Self::Succeeded,
            "skipped" => Self::Skipped,
            "failed" => Self::Failed,
            "corrupt" => Self::Corrupt,
            _ => return None,
        })
    }
}

/// The payload of a maintenance job: `{"repo_id": <id>}`.
pub fn maintain_payload(payload: &serde_json::Value) -> Result<RepoId, ForgeError> {
    payload
        .get("repo_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .map(RepoId::new)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_payload")
                .with_context("kind", MAINTAIN_JOB)
        })
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", MAINTAIN_JOB)
}

/// The job of `repo` with `dedup` (module docs).
pub fn maintain_job(repo: RepoId, dedup: &str, origin: RequestId) -> Result<NewJob, ForgeError> {
    Ok(NewJob {
        kind: JobKind::parse(MAINTAIN_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({ "repo_id": repo.get() }),
        max_attempts: MaxAttempts::new(MAINTAIN_ATTEMPTS)
            .ok_or_else(|| internal("invalid_max_attempts"))?,
        timeout: JobTimeout::new(MAINTAIN_JOB_TIMEOUT)
            .ok_or_else(|| internal("invalid_job_timeout"))?,
        dedup_key: Some(DedupKey::parse(dedup).ok_or_else(|| internal("invalid_dedup_key"))?),
        origin_request_id: origin,
        origin_event_seq: None,
    })
}

/// The local time of day in `time_zone`, in minutes after midnight, by the
/// database's clock. An unknown zone is `FG-SYS-0002
/// config_key=maintenance.window_time_zone reason=unknown`.
pub async fn local_minute(db: &Db, time_zone: &str) -> Result<u16, ForgeError> {
    const QUERY: &str = "maintenance.local_minute";
    let minute: i32 = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT (extract(hour FROM t)::int * 60 + extract(minute FROM t)::int) \
                 FROM (SELECT clock_timestamp() AT TIME ZONE $1 AS t) s",
            )
            .bind(time_zone)
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| {
                // 22023 invalid_parameter_value: "time zone … not recognized".
                let unknown = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "22023");
                if unknown {
                    ForgeError::new(ErrorCode::ConfigInvalid)
                        .with_context("config_key", "maintenance.window_time_zone")
                        .with_context("reason", "unknown")
                } else {
                    read_query_error(QUERY, e)
                }
            })
        })
        .await?;
    u16::try_from(minute).map_err(|_| internal("local_minute_out_of_range"))
}

/// A repository [`schedule_due`] enqueued (or found already enqueued).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scheduled {
    pub repo_id: RepoId,
    pub enqueued: Enqueued,
}

/// The due repositories, oldest first, at most `limit` (1..=1000), each
/// enqueued (module docs). Ordered by `next_due_at`, a never maintained
/// repository by its `ready_at`, then by id. At most `limit` maintenance
/// jobs are ever live (queued or running): a pass enqueues only up to the
/// free room, so a slow night never piles jobs up in the queue (the
/// doctor's queue-depth alert stays meaningful). A due repository whose
/// job is already live comes back as `Deduped`.
pub async fn schedule_due(
    db: &Db,
    limit: i64,
    origin: &RequestId,
) -> Result<Vec<Scheduled>, ForgeError> {
    const QUERY: &str = "maintenance.due";
    let limit = limit.clamp(1, 1000);
    let mut live: i64 = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT count(*) FROM forge.jobs WHERE kind = $1 \
                 AND state IN ('queued', 'running')",
            )
            .bind(MAINTAIN_JOB)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT r.id, floor(extract(epoch FROM coalesce(m.next_due_at, r.ready_at)))::bigint \
                 FROM forge.repositories r \
                 LEFT JOIN forge.repository_maintenance m ON m.repo_id = r.id \
                 WHERE r.state = 'ready' \
                   AND coalesce(m.next_due_at, r.ready_at) <= clock_timestamp() \
                 ORDER BY coalesce(m.next_due_at, r.ready_at), r.id LIMIT $1",
            )
            .bind(limit)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let mut scheduled = Vec::with_capacity(rows.len());
    for (repo, due) in rows {
        let repo_id = RepoId::new(repo);
        let key = format!("{MAINTAIN_DEDUP_PREFIX}:{repo}:{due}");
        let job = maintain_job(repo_id, &key, origin.clone())?; // clone: each job keeps the pass's id
        let enqueued = if live >= limit {
            // No room: only a live job of this due time may answer (dedup).
            match enqueue_if_live(db, &job).await? {
                Some(found) => found,
                None => break,
            }
        } else {
            let e = enqueue(db, &job).await?;
            if matches!(e, Enqueued::Queued(_)) {
                live += 1;
            }
            e
        };
        scheduled.push(Scheduled { repo_id, enqueued });
    }
    Ok(scheduled)
}

/// `Deduped` when a live (queued / running) job holds `job`'s key, `None`
/// otherwise (nothing is written).
async fn enqueue_if_live(db: &Db, job: &NewJob) -> Result<Option<Enqueued>, ForgeError> {
    const QUERY: &str = "maintenance.live_key";
    let key = job.dedup_key.as_ref().map(DedupKey::as_str);
    let held: bool = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM forge.jobs WHERE kind = $1 AND dedup_key = $2 \
                 AND state IN ('queued', 'running'))",
            )
            .bind(MAINTAIN_JOB)
            .bind(key)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(held.then_some(Enqueued::Deduped))
}

/// Enqueue one maintenance of `repo` now (`forge admin repo maintain`).
/// `FG-REPO-0005`-style refusals are the caller's: this checks only that
/// the repository exists and is ready (`FG-REPO-0005 NotFound` otherwise,
/// with `state`).
pub async fn enqueue_now(
    db: &Db,
    repo: RepoId,
    origin: &RequestId,
) -> Result<Enqueued, ForgeError> {
    const QUERY: &str = "maintenance.enqueue_now";
    let state: Option<String> = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT state FROM forge.repositories WHERE id = $1")
                .bind(repo.get())
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    match state.as_deref() {
        Some("ready") => {}
        other => {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "repository")
                .with_context("repo_id", repo.get())
                .with_context("state", other.unwrap_or("missing")))
        }
    }
    let key = format!(
        "{MAINTAIN_DEDUP_PREFIX}:{}:admin:{}",
        repo.get(),
        origin.as_str()
    );
    enqueue(db, &maintain_job(repo, &key, origin.clone())?).await // clone: the job keeps it
}

/// What [`start`] read of the previous runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    pub project_id: ProjectId,
    /// The last fsck's result (`None`: never checked).
    pub fsck_ok: Option<bool>,
    /// Seconds since the last full fsck (`None`: never).
    pub full_fsck_age_secs: Option<i64>,
    /// Consecutive failed runs before this one (a lost run counted).
    pub failures: i32,
    /// The previous run was still `running` past [`STALE_RUNNING_AFTER`]:
    /// it crashed or was dead-lettered without a ledger write. It was
    /// counted as a failure here (the caller logs it once, FG-DB-0007).
    pub lost_previous: bool,
}

/// `(project_id, fsck_ok, full fsck age, failures, lost_previous)`.
type StartRow = (i64, Option<bool>, Option<i64>, i32, bool);

/// A row still `running` this long after its start belongs to a run that
/// is gone: the job's lease (its timeout plus the queue's slack) is over.
pub const STALE_RUNNING_AFTER: Duration = MAINTAIN_JOB_TIMEOUT.saturating_add(LEASE_SLACK);

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Mark the run of `repo` as started (`outcome = 'running'`), creating the
/// row on the first run. `None`: the repository is not `ready` — nothing
/// is written and the job does nothing.
///
/// - The repository row is read `FOR SHARE` (the purge's `FOR UPDATE` and
///   this insert never interleave: the purge deletes the ledger row after
///   it, review LOW).
/// - A first row gets `next_due_at = ready_at` — the due time the
///   scheduler keyed the job with — so a pass while it runs finds the same
///   key and dedups (review MEDIUM: `now()` here made the key drift and a
///   second job was enqueued on a repository's first run).
/// - A previous run still `running` past [`STALE_RUNNING_AFTER`] is
///   counted as a failure ([`Started::lost_previous`]).
pub async fn start(db: &Db, repo: RepoId) -> Result<Option<Started>, ForgeError> {
    const QUERY: &str = "maintenance.start";
    let row: Option<StartRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "WITH r AS (SELECT id, project_id, ready_at FROM forge.repositories \
                     WHERE id = $1 AND state = 'ready' FOR SHARE), \
                 prev AS (SELECT outcome = 'running' \
                     AND last_started_at < now() - make_interval(secs => $2::double precision) \
                     AS lost FROM forge.repository_maintenance WHERE repo_id = $1) \
                 INSERT INTO forge.repository_maintenance AS m \
                     (repo_id, project_id, last_started_at, outcome, next_due_at) \
                 SELECT r.id, r.project_id, now(), 'running', coalesce(r.ready_at, now()) \
                 FROM r \
                 ON CONFLICT (repo_id) DO UPDATE SET last_started_at = now(), \
                     outcome = 'running', error_code = NULL, skipped_reason = NULL, \
                     failures = CASE WHEN m.outcome = 'running' AND m.last_started_at \
                         < now() - make_interval(secs => $2::double precision) \
                         THEN m.failures + 1 ELSE m.failures END, \
                     updated_at = now() \
                 RETURNING m.project_id, m.fsck_ok, \
                     floor(extract(epoch FROM now() - m.last_full_fsck_at))::bigint, m.failures, \
                     coalesce((SELECT lost FROM prev), false)",
            )
            .bind(repo.get())
            .bind(secs(STALE_RUNNING_AFTER))
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))
        })
        .await?;
    Ok(row.map(|(project, fsck_ok, age, failures, lost)| Started {
        project_id: ProjectId::new(project),
        fsck_ok,
        full_fsck_age_secs: age,
        failures,
        lost_previous: lost,
    }))
}

/// End the rows still `running` past [`STALE_RUNNING_AFTER`] whose job is
/// no longer live (crashed, then dead-lettered): `failed` with
/// `FG-DB-0007` (the lease expired), `failures + 1`, due again after
/// `retry_after`. Without it such a row keeps its due time and the
/// scheduler enqueues it again every pass (review MEDIUM). At most 100 per
/// call; the scheduler runs it every pass, inside the window or not.
pub async fn reap_stale_running(db: &Db, retry_after: Duration) -> Result<Vec<RepoId>, ForgeError> {
    const QUERY: &str = "maintenance.reap_stale_running";
    let ids: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "UPDATE forge.repository_maintenance m SET outcome = 'failed', \
                     error_code = 'FG-DB-0007', skipped_reason = NULL, \
                     failures = m.failures + 1, last_finished_at = now(), \
                     next_due_at = now() + make_interval(secs => $3::double precision), \
                     updated_at = now() \
                 WHERE m.repo_id IN (SELECT s.repo_id FROM forge.repository_maintenance s \
                     WHERE s.outcome = 'running' \
                       AND s.last_started_at < now() - make_interval(secs => $2::double precision) \
                       AND NOT EXISTS (SELECT 1 FROM forge.jobs j WHERE j.kind = $1 \
                           AND j.state IN ('queued', 'running') \
                           AND j.payload->>'repo_id' = s.repo_id::text) \
                     ORDER BY s.repo_id LIMIT 100 FOR UPDATE SKIP LOCKED) \
                 RETURNING m.repo_id",
            )
            .bind(MAINTAIN_JOB)
            .bind(secs(STALE_RUNNING_AFTER))
            .bind(secs(retry_after))
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(ids.into_iter().map(RepoId::new).collect())
}

/// The end of a run, as [`finish`] records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finish {
    pub outcome: Outcome,
    /// Required exactly for `Failed` / `Corrupt`.
    pub error_code: Option<ErrorCode>,
    /// `[a-z][a-z0-9_]*`: why a run was skipped or a step left out.
    pub skipped_reason: Option<&'static str>,
    /// `(objects, size_pack_kib, refs)` when the counts ran.
    pub counts: Option<(u64, u64, u64)>,
    /// `(mode, ok)` when an fsck ran (`connectivity` / `full`).
    pub fsck: Option<(&'static str, bool)>,
    /// The next run is due this long after now.
    pub next_due_in: Duration,
}

/// Record the end of the run of `repo` (module docs). A row the purge
/// deleted meanwhile is not an error (`false`).
pub async fn finish(db: &Db, repo: RepoId, end: &Finish) -> Result<bool, ForgeError> {
    const QUERY: &str = "maintenance.finish";
    let failed = matches!(end.outcome, Outcome::Failed | Outcome::Corrupt);
    if failed != end.error_code.is_some() {
        return Err(internal("finish_error_code_mismatch"));
    }
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let (objects, size, refs) = end.counts.map_or((None, None, None), |(o, s, r)| {
        (Some(count(o)), Some(count(s)), Some(count(r)))
    });
    let (mode, ok) = end.fsck.map_or((None, None), |(m, ok)| (Some(m), Some(ok)));
    let next_secs = i64::try_from(end.next_due_in.as_secs()).unwrap_or(i64::MAX);
    let updated = db
        .within(QUERY, async {
            sqlx::query(
                "UPDATE forge.repository_maintenance SET last_finished_at = now(), \
                     outcome = $2, error_code = $3, skipped_reason = $4, \
                     failures = CASE WHEN $2 IN ('failed', 'corrupt') THEN failures + 1 \
                                     WHEN $2 = 'succeeded' THEN 0 ELSE failures END, \
                     objects = coalesce($5, objects), size_pack_kib = coalesce($6, size_pack_kib), \
                     refs = coalesce($7, refs), \
                     last_fsck_at = CASE WHEN $8::text IS NULL THEN last_fsck_at ELSE now() END, \
                     last_fsck_mode = coalesce($8, last_fsck_mode), \
                     fsck_ok = coalesce($9, fsck_ok), \
                     last_success_at = CASE WHEN $2 = 'succeeded' THEN now() \
                                            ELSE last_success_at END, \
                     last_full_fsck_at = CASE WHEN $8 = 'full' AND $9 THEN now() \
                                              ELSE last_full_fsck_at END, \
                     next_due_at = now() + make_interval(secs => $10::double precision), \
                     updated_at = now() \
                 WHERE repo_id = $1",
            )
            .bind(repo.get())
            .bind(end.outcome.as_str())
            .bind(end.error_code.map(ErrorCode::id))
            .bind(end.skipped_reason)
            .bind(objects)
            .bind(size)
            .bind(refs)
            .bind(mode)
            .bind(ok)
            .bind(next_secs)
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))
        })
        .await?;
    Ok(updated.rows_affected() == 1)
}

/// One ledger row (doctor, tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub repo_id: RepoId,
    pub outcome: Option<Outcome>,
    pub error_code: Option<String>,
    pub skipped_reason: Option<String>,
    pub failures: i32,
    pub objects: Option<i64>,
    pub size_pack_kib: Option<i64>,
    pub refs: Option<i64>,
    pub fsck_ok: Option<bool>,
    pub last_fsck_mode: Option<String>,
    pub full_fsck_done: bool,
    /// Seconds until the next run (negative: overdue).
    pub due_in_secs: i64,
}

type RawRow = (
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i32,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<bool>,
    Option<String>,
    bool,
    i64,
);

/// The ledger row of `repo` (read pool).
pub async fn row(db: &Db, repo: RepoId) -> Result<Option<LedgerRow>, ForgeError> {
    const QUERY: &str = "maintenance.row";
    let raw: Option<RawRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT repo_id, outcome, error_code, skipped_reason, failures, objects, \
                     size_pack_kib, refs, fsck_ok, last_fsck_mode, last_full_fsck_at IS NOT NULL, \
                     floor(extract(epoch FROM next_due_at - clock_timestamp()))::bigint \
                 FROM forge.repository_maintenance WHERE repo_id = $1",
            )
            .bind(repo.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    raw.map(|r| {
        let outcome = match r.1.as_deref() {
            None | Some("running") => None,
            Some(o) => Some(Outcome::parse(o).ok_or_else(|| internal("unknown_outcome"))?),
        };
        Ok(LedgerRow {
            repo_id: RepoId::new(r.0),
            outcome,
            error_code: r.2,
            skipped_reason: r.3,
            failures: r.4,
            objects: r.5,
            size_pack_kib: r.6,
            refs: r.7,
            fsck_ok: r.8,
            last_fsck_mode: r.9,
            full_fsck_done: r.10,
            due_in_secs: r.11,
        })
    })
    .transpose()
}

/// A repository a doctor check names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flagged {
    pub repo_id: RepoId,
    /// Overdue: seconds past the due time; failing: consecutive failures;
    /// corrupt: 0; refs: the ref count.
    pub value: i64,
    /// The last error code (failing, corrupt).
    pub error_code: Option<String>,
}

/// What `forge doctor` reports (each list at most [`REPORT_LIST_MAX`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaintenanceReport {
    /// Ready repositories more than one interval past their due time
    /// (never maintained: since `ready_at`), longest first.
    pub overdue: i64,
    pub overdue_list: Vec<Flagged>,
    /// Two or more consecutive failed runs.
    pub failing: i64,
    pub failing_list: Vec<Flagged>,
    /// The last fsck failed.
    pub corrupt: i64,
    pub corrupt_list: Vec<Flagged>,
    /// More refs than `refs_warn`.
    pub refs_over: i64,
    pub refs_list: Vec<Flagged>,
    /// The largest ref count measured.
    pub refs_max: i64,
    /// Rows still `running` past [`STALE_RUNNING_AFTER`] (value: seconds
    /// since the start): a run that is gone and not reaped yet.
    pub stale_running: i64,
    pub stale_running_list: Vec<Flagged>,
    /// Ready repositories whose last success (never: `ready_at`) is older
    /// than twice the interval (value: seconds since): skipped or failing
    /// every night without ever turning overdue.
    pub no_recent_success: i64,
    pub no_recent_success_list: Vec<Flagged>,
}

const OVERDUE_SQL: &str = "SELECT r.id, \
     floor(extract(epoch FROM clock_timestamp() - coalesce(m.next_due_at, r.ready_at)))::bigint \
         AS v, \
     m.error_code, count(*) OVER () \
     FROM forge.repositories r \
     LEFT JOIN forge.repository_maintenance m ON m.repo_id = r.id \
     WHERE r.state = 'ready' AND coalesce(m.next_due_at, r.ready_at) \
         < clock_timestamp() - make_interval(secs => $1::double precision) \
     ORDER BY v DESC, r.id LIMIT $2";
const FAILING_SQL: &str = "SELECT repo_id, failures::bigint, error_code, count(*) OVER () \
     FROM forge.repository_maintenance \
     WHERE failures >= 2 ORDER BY failures DESC, repo_id LIMIT $1";
const CORRUPT_SQL: &str = "SELECT repo_id, 0::bigint, error_code, count(*) OVER () \
     FROM forge.repository_maintenance \
     WHERE fsck_ok = false ORDER BY repo_id LIMIT $1";
const STALE_RUNNING_SQL: &str = "SELECT repo_id, \
     floor(extract(epoch FROM clock_timestamp() - last_started_at))::bigint AS v, NULL::text, \
     count(*) OVER () \
     FROM forge.repository_maintenance \
     WHERE outcome = 'running' \
         AND last_started_at < clock_timestamp() - make_interval(secs => $1::double precision) \
     ORDER BY v DESC, repo_id LIMIT $2";
const NO_RECENT_SUCCESS_SQL: &str = "SELECT r.id, \
     floor(extract(epoch FROM clock_timestamp() - coalesce(m.last_success_at, r.ready_at)))::bigint \
         AS v, \
     m.error_code, count(*) OVER () \
     FROM forge.repositories r \
     LEFT JOIN forge.repository_maintenance m ON m.repo_id = r.id \
     WHERE r.state = 'ready' AND coalesce(m.last_success_at, r.ready_at) \
         < clock_timestamp() - make_interval(secs => $1::double precision) \
     ORDER BY v DESC, r.id LIMIT $2";
const REFS_SQL: &str = "SELECT repo_id, refs, NULL::text, count(*) OVER () \
     FROM forge.repository_maintenance \
     WHERE refs > $1 ORDER BY refs DESC, repo_id LIMIT $2";

/// The doctor report: `interval` the configured maintenance interval,
/// `refs_warn` the ref count to warn above. Read pool only, seven bounded
/// statements. `None` before the migration of the ledger.
pub async fn report(
    db: &Db,
    interval: Duration,
    refs_warn: u64,
) -> Result<Option<MaintenanceReport>, ForgeError> {
    const QUERY: &str = "maintenance.report";
    let interval_secs = i64::try_from(interval.as_secs()).unwrap_or(i64::MAX);
    let refs_warn = i64::try_from(refs_warn).unwrap_or(i64::MAX);
    let pool = db.pool().executor();
    db.within(QUERY, async {
        let q = |e| read_query_error(QUERY, e);
        let migrated: bool = sqlx::query_scalar(
            "SELECT pg_catalog.to_regclass('forge.repository_maintenance') IS NOT NULL",
        )
        .fetch_one(pool)
        .await
        .map_err(q)?;
        if !migrated {
            return Ok(None);
        }
        // Each list: (id, value, error_code, total rows matching).
        let overdue: Vec<FlagRow> = sqlx::query_as(OVERDUE_SQL)
            .bind(interval_secs)
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let failing: Vec<FlagRow> = sqlx::query_as(FAILING_SQL)
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let corrupt: Vec<FlagRow> = sqlx::query_as(CORRUPT_SQL)
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let refs: Vec<FlagRow> = sqlx::query_as(REFS_SQL)
            .bind(refs_warn)
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let stale_running: Vec<FlagRow> = sqlx::query_as(STALE_RUNNING_SQL)
            .bind(secs(STALE_RUNNING_AFTER))
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let no_recent: Vec<FlagRow> = sqlx::query_as(NO_RECENT_SUCCESS_SQL)
            .bind(interval_secs.saturating_mul(2))
            .bind(REPORT_LIST_MAX)
            .fetch_all(pool)
            .await
            .map_err(q)?;
        let refs_max: Option<i64> =
            sqlx::query_scalar("SELECT max(refs) FROM forge.repository_maintenance")
                .fetch_one(pool)
                .await
                .map_err(q)?;
        let (overdue, overdue_list) = split(overdue);
        let (failing, failing_list) = split(failing);
        let (corrupt, corrupt_list) = split(corrupt);
        let (refs_over, refs_list) = split(refs);
        let (stale_running, stale_running_list) = split(stale_running);
        let (no_recent_success, no_recent_success_list) = split(no_recent);
        Ok(Some(MaintenanceReport {
            overdue,
            overdue_list,
            failing,
            failing_list,
            corrupt,
            corrupt_list,
            refs_over,
            refs_list,
            refs_max: refs_max.unwrap_or(0),
            stale_running,
            stale_running_list,
            no_recent_success,
            no_recent_success_list,
        }))
    })
    .await
}

/// `(id, value, error_code, total)` of a doctor list.
type FlagRow = (i64, i64, Option<String>, i64);

fn split(rows: Vec<FlagRow>) -> (i64, Vec<Flagged>) {
    let total = rows.first().map_or(0, |r| r.3);
    let list = rows
        .into_iter()
        .map(|(id, value, error_code, _)| Flagged {
            repo_id: RepoId::new(id),
            value,
            error_code,
        })
        .collect();
    (total, list)
}

#[cfg(test)]
mod tests;
