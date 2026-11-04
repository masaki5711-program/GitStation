//! The per repository merge lock (ADR-0024 §4.2 step 2): the merge job
//! takes `pg_advisory_xact_lock` on `merge:<repo_id>` in a transaction of its
//! own and holds it around the trial merge, the commit, the fence and the
//! ref update, so the merges of one repository run one at a time and the
//! reconciliation never acts on an attempt a running job still works on.
//!
//! The wait is bounded ([`MERGE_LOCK_WAIT`]: `55P03` → FG-DB-0002, the job
//! retries); so is the hold ([`MERGE_LOCK_HOLD`]: every git call under it
//! is bounded by the time left). [`MergeLock::alive`] proves on the lock's
//! own connection that the lock still stands right before the ref moves
//! (the fence's part (a)). One write-pool connection per running merge,
//! idle in its transaction, plus the short statements of the steps.

use std::time::Duration;

use forge_core::RepoId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::Postgres;
use sqlx::Transaction;

use crate::{query_error, Db};

/// The longest wait for another merge of the same repository.
pub const MERGE_LOCK_WAIT: Duration = Duration::from_secs(30);
/// The longest a merge holds the lock (from the moment it is taken).
pub const MERGE_LOCK_HOLD: Duration = Duration::from_secs(180);
/// The lock's transaction outlives an idle gap of the hold, plus slack.
const IDLE_LIMIT: Duration = Duration::from_secs(200);
/// The bound of the liveness ping and of the explicit ROLLBACK.
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// The merge lock of one repository (module docs). Dropping it without
/// [`MergeLock::release`] still releases it when the connection's
/// transaction ends, but late: release it explicitly.
pub struct MergeLock {
    tx: Transaction<'static, Postgres>,
    repo: RepoId,
    taken: tokio::time::Instant,
}

impl std::fmt::Debug for MergeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeLock")
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

/// The `reason` of a busy merge lock (another merge holds it past the
/// wait: `55P03`, or our own bound around the wait).
pub const MERGE_LOCK_BUSY: &str = "merge_lock_busy";

/// Whether `e` is the merge lock being held by another merge — the only
/// failure the reconciliation may skip silently (review round 1).
pub fn is_lock_busy(e: &ForgeError) -> bool {
    e.code() == ErrorCode::DbQueryFailed
        && e.context()
            .iter()
            .any(|(k, v)| *k == "reason" && v == MERGE_LOCK_BUSY)
}

fn busy(repo: RepoId, wait: Duration) -> ForgeError {
    ForgeError::new(ErrorCode::DbQueryFailed)
        .with_context("reason", MERGE_LOCK_BUSY)
        .with_context("retryable", true)
        .with_context("repo_id", repo.get())
        .with_context("wait_ms", wait.as_millis())
}

/// Slack above the lock wait for the statements around it.
const SLACK: Duration = Duration::from_secs(5);

/// Take the merge lock of `repo`, waiting at most `wait`: another merge
/// holding it longer is `FG-DB-0002 reason=merge_lock_busy` (retry later,
/// [`is_lock_busy`]); any other failure keeps its own code and reason. The
/// lock transaction's `statement_timeout` is raised above the wait, and the
/// whole acquisition is bounded by our own timeout (`wait` + 2 × slack).
pub async fn lock_merges(db: &Db, repo: RepoId, wait: Duration) -> Result<MergeLock, ForgeError> {
    const QUERY: &str = "review.merge_lock";
    let bound = wait.saturating_add(SLACK.saturating_mul(2));
    let taking = async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        for setting in [
            format!("SET LOCAL lock_timeout = '{}ms'", wait.as_millis()),
            format!(
                "SET LOCAL statement_timeout = '{}ms'",
                wait.saturating_add(SLACK).as_millis()
            ),
            format!(
                "SET LOCAL idle_in_transaction_session_timeout = '{}ms'",
                IDLE_LIMIT.as_millis()
            ),
        ] {
            sqlx::query(&setting)
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('merge:' || $1::text, 0))")
            .bind(repo.get())
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                let lock_timeout = e
                    .as_database_error()
                    .and_then(|d| d.code())
                    .is_some_and(|c| c == "55P03");
                if lock_timeout {
                    busy(repo, wait)
                } else {
                    query_error(QUERY, e).with_context("repo_id", repo.get())
                }
            })?;
        Ok(MergeLock {
            tx,
            repo,
            taken: tokio::time::Instant::now(),
        })
    };
    match tokio::time::timeout(bound, taking).await {
        Ok(taken) => taken,
        Err(_) => Err(busy(repo, wait).with_context("detail", "client_bound")),
    }
}

/// Take the merge lock of `repo` only if it is free right now
/// (`pg_try_advisory_xact_lock`, never waiting): `None` while a merge holds
/// it. M3 O3: the maintenance's `pack-refs` runs under it, so it never
/// competes with a merge's `update-ref` for the ref locks — a merge in
/// progress makes the maintenance skip pack-refs for that run. The lock
/// key `repo_maint:<repo_id>` is reserved for O1 (backup ↔ maintenance).
pub async fn try_lock_merges(db: &Db, repo: RepoId) -> Result<Option<MergeLock>, ForgeError> {
    const QUERY: &str = "review.merge_try_lock";
    let taking = async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        sqlx::query(&format!(
            "SET LOCAL idle_in_transaction_session_timeout = '{}ms'",
            IDLE_LIMIT.as_millis()
        ))
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let taken: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended('merge:' || $1::text, 0))",
        )
        .bind(repo.get())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e).with_context("repo_id", repo.get()))?;
        if !taken {
            // Nothing to keep: end the empty transaction now.
            tx.rollback().await.map_err(|e| query_error(QUERY, e))?;
            return Ok(None);
        }
        Ok(Some(MergeLock {
            tx,
            repo,
            taken: tokio::time::Instant::now(),
        }))
    };
    match tokio::time::timeout(SLACK.saturating_mul(2), taking).await {
        Ok(taken) => taken,
        Err(_) => Err(busy(repo, Duration::ZERO).with_context("detail", "client_bound")),
    }
}

/// The advisory lock key reserved for the repository backup (M3 O1) to
/// keep a backup and the maintenance of one repository apart:
/// `hashtextextended('repo_maint:' || <repo_id>, 0)`. Not taken by O3.
pub const REPO_MAINT_LOCK_PREFIX: &str = "repo_maint:";

impl MergeLock {
    pub fn repo(&self) -> RepoId {
        self.repo
    }

    /// When the hold ends ([`MERGE_LOCK_HOLD`] after the lock was taken).
    pub fn deadline(&self) -> tokio::time::Instant {
        self.taken + MERGE_LOCK_HOLD
    }

    /// The time left of the hold (zero once it is over).
    pub fn left(&self) -> Duration {
        self.deadline()
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// The lock still stands: its connection answers inside its transaction
    /// and the advisory lock is held by that backend (`pg_locks` of its own
    /// pid). `FG-DB-0002 reason=merge_lock_lost` otherwise.
    pub async fn alive(&mut self) -> Result<(), ForgeError> {
        let repo = self.repo.get();
        let left = self.left();
        let probe = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_locks WHERE locktype = 'advisory' \
             AND pid = pg_catalog.pg_backend_pid() AND granted AND objsubid = 1 \
             AND database = (SELECT oid FROM pg_catalog.pg_database \
                 WHERE datname = pg_catalog.current_database()) \
             AND ((classid::bigint << 32) | objid::bigint) \
                 = hashtextextended('merge:' || $1::text, 0))",
        )
        .bind(repo)
        .fetch_one(&mut *self.tx);
        let lost = |detail: &'static str| {
            ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "merge_lock_lost")
                .with_context("detail", detail)
                .with_context("retryable", true)
                .with_context("repo_id", repo)
        };
        match tokio::time::timeout(left.min(PING_TIMEOUT), probe).await {
            Ok(Ok(true)) if !left.is_zero() => Ok(()),
            Ok(Ok(true)) | Err(_) => Err(lost("hold_exceeded")),
            Ok(Ok(false)) => Err(lost("not_held")),
            Ok(Err(e)) => Err(query_error("review.merge_lock", e)
                .with_context("reason", "merge_lock_lost")
                .with_context("repo_id", repo)),
        }
    }

    /// Release the lock now (explicit ROLLBACK, bounded). A failed release
    /// is logged: the server ends the transaction with the connection.
    pub async fn release(self) {
        let repo = self.repo.get();
        let failure = match tokio::time::timeout(PING_TIMEOUT, self.tx.rollback()).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(query_error("review.merge_unlock", e)),
            Err(_) => Some(
                ForgeError::new(ErrorCode::DbQueryFailed)
                    .with_context("query", "review.merge_unlock")
                    .with_context("reason", "rollback_timeout"),
            ),
        };
        if let Some(e) = failure {
            tracing::warn!(
                target: "forge_db",
                error_code = e.code().id(),
                repo_id = repo,
                error = %e,
                "releasing a merge lock failed; the connection ends it"
            );
        }
    }
}
