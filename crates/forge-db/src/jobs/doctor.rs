//! What `forge doctor` reports about the job queue, read on the read pool:
//!
//! - `jobs.dead` — `FG-DB-0006 reason=dead_lettered` when a job died within
//!   [`DEAD_WINDOW`] (older dead rows are only counted).
//! - `jobs.expired_leases` — `FG-DB-0017 reason=reaper_behind` when a lease
//!   expired more than [`LEASE_GRACE`] ago and is still `running`: no reaper
//!   runs (it would have reaped it within 30 s).
//! - `jobs.queue_depth` — `FG-DB-0017 reason=queue_depth` when a kind has
//!   more than [`QUEUE_DEPTH_ALERT`] ready jobs (the queue is misused, see
//!   the module docs), `reason=queue_wait` when its oldest ready job waits
//!   longer than [`QUEUE_WAIT_ALERT`] (no runner claims the kind). Counted
//!   per kind (bounded), the oldest ready job per kind via `jobs_ready_kind`.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::sequencer::{shown, COUNT_CAP};
use crate::{read_query_error, Db};

/// A dead job alerts for this long after it died.
pub const DEAD_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// An expired lease left unreaped for longer than this means no reaper runs.
pub const LEASE_GRACE: Duration = Duration::from_secs(60);
/// More ready jobs of one kind than this is an alert.
pub const QUEUE_DEPTH_ALERT: i64 = 1000;
/// A ready job waiting longer than this is an alert.
pub const QUEUE_WAIT_ALERT: Duration = Duration::from_secs(600);
/// At most this many kinds are read (the queue has tens).
const KINDS_CAP: i64 = 200;

/// The newest dead job of the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewestDead {
    pub job_id: i64,
    pub kind: String,
    pub attempts: i32,
    pub last_error_code: String,
    pub origin_request_id: String,
}

/// The oldest lease expired past the grace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredLease {
    pub job_id: i64,
    pub kind: String,
    pub worker_id: Option<String>,
    pub attempt: i32,
    pub expired_ms: i64,
}

/// Ready (`queued`, `run_after <= now()`) jobs of one kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindDepth {
    pub kind: String,
    /// At most `COUNT_CAP + 1` per kind.
    pub ready: i64,
    pub oldest_wait_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobsHealth {
    /// Dead within [`DEAD_WINDOW`] (capped), and in total (capped).
    pub dead_recent: i64,
    pub dead_total: i64,
    pub newest_dead: Option<NewestDead>,
    /// Running with a lease expired past [`LEASE_GRACE`] (capped).
    pub expired_leases: i64,
    pub oldest_expired: Option<ExpiredLease>,
    /// By kind, in kind order.
    pub depth: Vec<KindDepth>,
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl JobsHealth {
    pub fn check_dead(&self) -> Result<(), ForgeError> {
        let Some(newest) = self.newest_dead.as_ref().filter(|_| self.dead_recent > 0) else {
            return Ok(());
        };
        Err(ForgeError::new(ErrorCode::JobDeadLettered)
            .with_context("reason", "dead_lettered")
            .with_context("dead_recent", shown(self.dead_recent))
            .with_context("job_id", newest.job_id)
            .with_context("kind", &newest.kind)
            .with_context("attempts", newest.attempts)
            .with_context("last_error_code", &newest.last_error_code)
            .with_context("origin_request_id", &newest.origin_request_id))
    }

    pub fn check_expired_leases(&self) -> Result<(), ForgeError> {
        let Some(oldest) = self.oldest_expired.as_ref() else {
            return Ok(());
        };
        Err(ForgeError::new(ErrorCode::JobQueueBacklog)
            .with_context("reason", "reaper_behind")
            .with_context("expired_leases", shown(self.expired_leases))
            .with_context("job_id", oldest.job_id)
            .with_context("kind", &oldest.kind)
            .with_context("worker_id", oldest.worker_id.as_deref().unwrap_or("none"))
            .with_context("attempt", oldest.attempt)
            .with_context("expired_ms", oldest.expired_ms))
    }

    /// The first kind over a threshold (depth before wait).
    pub fn check_queue_depth(&self) -> Result<(), ForgeError> {
        let over = |reason: &'static str, k: &KindDepth| {
            ForgeError::new(ErrorCode::JobQueueBacklog)
                .with_context("reason", reason)
                .with_context("kind", &k.kind)
                .with_context("ready", shown(k.ready))
                .with_context("oldest_wait_ms", k.oldest_wait_ms)
        };
        if let Some(k) = self.depth.iter().find(|k| k.ready > QUEUE_DEPTH_ALERT) {
            return Err(over("queue_depth", k));
        }
        if let Some(k) = self
            .depth
            .iter()
            .find(|k| k.oldest_wait_ms > ms(QUEUE_WAIT_ALERT))
        {
            return Err(over("queue_wait", k));
        }
        Ok(())
    }
}

type DeadRow = (
    i64,
    i64,
    Option<i64>,
    Option<String>,
    Option<i32>,
    Option<String>,
    Option<String>,
);
type ExpiredRow = (
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i32>,
    Option<i64>,
);

/// The queue's health, or `None` before the first schema migration.
pub async fn health(db: &Db) -> Result<Option<JobsHealth>, ForgeError> {
    db.within("jobs.health", async {
        let mut conn = db.acquire().await?;
        let migrated: bool =
            sqlx::query_scalar("SELECT pg_catalog.to_regclass('forge.jobs') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| read_query_error("jobs.health", e))?;
        if !migrated {
            return Ok(None);
        }
        let cap = COUNT_CAP + 1;
        let dead: DeadRow = sqlx::query_as(
            "SELECT (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.jobs \
                     WHERE state = 'dead' AND finished_at > pg_catalog.now() \
                       - pg_catalog.make_interval(secs => $1) LIMIT $2) AS w), \
                    (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.jobs \
                     WHERE state = 'dead' LIMIT $2) AS w), \
                    n.id, n.kind, n.attempts, n.last_error_code, n.origin_request_id \
             FROM (SELECT 1) AS one LEFT JOIN LATERAL ( \
                 SELECT id, kind, attempts, last_error_code, origin_request_id \
                 FROM ONLY forge.jobs WHERE state = 'dead' \
                 ORDER BY finished_at DESC, id DESC LIMIT 1) AS n ON true",
        )
        .bind(DEAD_WINDOW.as_secs_f64())
        .bind(cap)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("jobs.health_dead", e))?;
        let expired: ExpiredRow = sqlx::query_as(
            "SELECT (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.jobs \
                     WHERE state = 'running' AND lease_until < pg_catalog.now() \
                       - pg_catalog.make_interval(secs => $1) LIMIT $2) AS w), \
                    o.id, o.kind, o.worker_id, o.attempts, \
                    (EXTRACT(epoch FROM pg_catalog.now() - o.lease_until) * 1000)::bigint \
             FROM (SELECT 1) AS one LEFT JOIN LATERAL ( \
                 SELECT id, kind, worker_id, attempts, lease_until FROM ONLY forge.jobs \
                 WHERE state = 'running' AND lease_until < pg_catalog.now() \
                   - pg_catalog.make_interval(secs => $1) \
                 ORDER BY lease_until LIMIT 1) AS o ON true",
        )
        .bind(LEASE_GRACE.as_secs_f64())
        .bind(cap)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("jobs.health_leases", e))?;
        // Each kind with ready jobs (a skip scan over jobs_ready_kind), then
        // per kind a bounded count and its oldest ready job.
        let depth: Vec<(String, i64, i64)> = sqlx::query_as(
            "WITH RECURSIVE kinds(kind) AS ( \
                 (SELECT j.kind FROM ONLY forge.jobs AS j \
                   WHERE j.state = 'queued' AND j.run_after <= pg_catalog.now() \
                   ORDER BY j.kind LIMIT 1) \
                 UNION ALL \
                 SELECT (SELECT j.kind FROM ONLY forge.jobs AS j \
                          WHERE j.state = 'queued' AND j.run_after <= pg_catalog.now() \
                            AND j.kind > k.kind ORDER BY j.kind LIMIT 1) \
                   FROM kinds AS k WHERE k.kind IS NOT NULL) \
             SELECT k.kind, \
                    (SELECT pg_catalog.count(*) FROM (SELECT 1 FROM ONLY forge.jobs AS j \
                      WHERE j.state = 'queued' AND j.kind = k.kind \
                        AND j.run_after <= pg_catalog.now() LIMIT $1) AS w), \
                    (SELECT (EXTRACT(epoch FROM pg_catalog.now() - j.run_after) * 1000)::bigint \
                       FROM ONLY forge.jobs AS j \
                      WHERE j.state = 'queued' AND j.kind = k.kind \
                        AND j.run_after <= pg_catalog.now() \
                      ORDER BY j.run_after LIMIT 1) \
             FROM kinds AS k WHERE k.kind IS NOT NULL ORDER BY k.kind LIMIT $2",
        )
        .bind(cap)
        .bind(KINDS_CAP)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("jobs.health_depth", e))?;
        Ok(Some(assemble(dead, expired, depth)))
    })
    .await
}

fn assemble(dead: DeadRow, expired: ExpiredRow, depth: Vec<(String, i64, i64)>) -> JobsHealth {
    let (dead_recent, dead_total, d_id, d_kind, d_attempts, d_code, d_origin) = dead;
    let newest_dead = match (d_id, d_kind, d_attempts, d_code, d_origin) {
        (Some(job_id), Some(kind), Some(attempts), code, Some(origin_request_id)) => {
            Some(NewestDead {
                job_id,
                kind,
                attempts,
                last_error_code: code.unwrap_or_else(|| "none".to_owned()),
                origin_request_id,
            })
        }
        _ => None,
    };
    let (expired_leases, e_id, e_kind, worker_id, e_attempt, e_ms) = expired;
    let oldest_expired = match (e_id, e_kind, e_attempt, e_ms) {
        (Some(job_id), Some(kind), Some(attempt), Some(expired_ms)) => Some(ExpiredLease {
            job_id,
            kind,
            worker_id,
            attempt,
            expired_ms,
        }),
        _ => None,
    };
    JobsHealth {
        dead_recent,
        dead_total,
        newest_dead,
        expired_leases,
        oldest_expired,
        depth: depth
            .into_iter()
            .map(|(kind, ready, oldest_wait_ms)| KindDepth {
                kind,
                ready,
                oldest_wait_ms,
            })
            .collect(),
    }
}
