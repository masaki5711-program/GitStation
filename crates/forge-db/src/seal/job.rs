//! The body of the `audit.seal` work of `forge worker` (ADR-0018 §4): seal
//! the numbered audit rows after the last seal, at most [`MAX_SEAL_ROWS`]
//! per call — at once ([`seal_pending`]) or only when due ([`seal_due`]:
//! a minute after the last seal, or sooner when more than 1000 rows wait).

use std::time::Duration;

use forge_core::config::SealKeys;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::Connection as _;

use super::chain::{self, SealFields, SealKind};
use super::{gap_in, hash_rows, mismatch, v1, LOCK_KEY};
use crate::sequencer::SeqRange;
use crate::{query_error, Db};

/// Rows per seal: bounds one call's work (read, hash, insert) well inside
/// the client-side bound. A larger backlog takes several calls (`more`).
pub const MAX_SEAL_ROWS: i64 = 5000;

/// The epoch of the first seal (C9's restore seal increments it).
const FIRST_EPOCH: i16 = 1;

/// When [`seal_due`] seals (ADR-0018 §4: every minute, or sooner when more
/// than 1000 rows wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealPolicy {
    /// Seal once the latest seal is at least this old.
    pub interval: Duration,
    /// Seal at once when more than this many numbered rows wait.
    pub eager_rows: i64,
}

impl SealPolicy {
    /// The policy of `forge worker`.
    pub const WORKER: Self = Self {
        interval: Duration::from_secs(60),
        eager_rows: 1000,
    };
}

/// audit_high_seq and high_seq (none without the feed-state row), the
/// latest seal's last_seq, seal_sha256, seal_epoch and whether it is older
/// than the policy's interval (none before the first seal), and whether
/// any audit row has a number.
type State = (
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i16>,
    Option<bool>,
    bool,
);

/// What one call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOutcome {
    /// Another caller holds the seal lock (it is sealing right now).
    Busy,
    /// Every numbered audit row is sealed (or nothing is numbered yet).
    UpToDate,
    /// [`seal_due`] only: rows wait, but the policy says not yet.
    NotDue,
    Sealed(Sealed),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sealed {
    pub seal_id: i64,
    pub range: SeqRange,
    /// Numbered rows remain after this seal: call [`seal_pending`] again now.
    pub more: bool,
}

/// One seal step, whatever the policy (the drain after `more`, tests).
/// See [`seal_due`] for everything else.
pub async fn seal_pending(db: &Db, keys: &SealKeys) -> Result<SealOutcome, ForgeError> {
    seal(db, keys, None).await
}

/// One seal step on the write pool, in one transaction under
/// `pg_try_advisory_xact_lock` (concurrent callers never seal twice: the
/// loser returns [`SealOutcome::Busy`]), only when `policy` says it is due.
/// Bounded on the client by [`Db::within`] (`FG-DB-0001
/// reason=operation_timeout`) and on the server by the statement and lock
/// timeouts; the hashing runs on the blocking pool (at most
/// [`MAX_SEAL_ROWS`] rows).
///
/// Cancel-safe and idempotent: dropping the future drops the transaction
/// (rolled back), and nothing is written outside it. A timeout during the
/// commit leaves its outcome unknown (committed or not); calling again is
/// always safe — the next call starts after whatever the last committed
/// seal is.
///
/// Errors: a hole in the numbers of the range is `FG-DB-0011 reason=gap`;
/// the feed-state row gone while audit rows are numbered, or
/// `audit_high_seq` below the last sealed seq, is `FG-DB-0011 reason=chain`
/// (`detail` = `feed_state_missing` / `audit_high_below_seal`) — nothing
/// is sealed in each case. A row that hash version 1 cannot express is
/// `FG-SYS-0001` (operation `seal.v1`, with `seq`).
pub async fn seal_due(
    db: &Db,
    keys: &SealKeys,
    policy: SealPolicy,
) -> Result<SealOutcome, ForgeError> {
    seal(db, keys, Some(policy)).await
}

/// End a call that sealed nothing: roll back explicitly so the seal lock is
/// released before the call returns. A dropped sqlx transaction is rolled
/// back only when a spawned task returns its connection to the pool; under
/// load that task runs late, the transaction-scoped advisory lock stays held,
/// and the next caller on another pooled connection gets `Busy` although
/// nobody is sealing.
async fn finish(
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
    outcome: SealOutcome,
) -> Result<SealOutcome, ForgeError> {
    tx.rollback()
        .await
        .map_err(|e| query_error("seal.rollback", e))?;
    Ok(outcome)
}

async fn seal(
    db: &Db,
    keys: &SealKeys,
    policy: Option<SealPolicy>,
) -> Result<SealOutcome, ForgeError> {
    db.within("seal.seal_pending", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("seal.acquire", e))?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| query_error("seal.begin", e))?;
        // Locked first: the reads below (READ COMMITTED, one snapshot per
        // statement) then see the previous holder's committed seal.
        let locked: bool =
            sqlx::query_scalar("SELECT pg_catalog.pg_try_advisory_xact_lock($1, $2)")
                .bind(LOCK_KEY.0)
                .bind(LOCK_KEY.1)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| query_error("seal.lock", e))?;
        if !locked {
            return Ok(SealOutcome::Busy);
        }
        // The serialization's casts and operators resolve in pg_catalog
        // only, never in forge (writable by forge_owner).
        sqlx::query("SET LOCAL search_path = pg_catalog, pg_temp")
            .execute(&mut *tx)
            .await
            .map_err(|e| query_error("seal.search_path", e))?;
        let interval_ms = policy.map_or(0.0, |p| p.interval.as_millis() as f64);
        let state: State = sqlx::query_as(
            "SELECT f.audit_high_seq, f.high_seq, p.last_seq, p.seal_sha256, p.seal_epoch, \
                    p.sealed_at <= pg_catalog.now() \
                        - $1::double precision * interval '1 millisecond', \
                    EXISTS (SELECT 1 FROM ONLY forge.audit_log AS a WHERE a.seq IS NOT NULL) \
             FROM (SELECT 1) AS one \
             LEFT JOIN ONLY forge.event_feed_state AS f ON f.singleton \
             LEFT JOIN LATERAL (SELECT s.last_seq, s.seal_sha256, s.seal_epoch, s.sealed_at \
                                FROM ONLY forge.audit_seal AS s \
                                ORDER BY s.last_seq DESC LIMIT 1) AS p ON true",
        )
        .bind(interval_ms)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error("seal.state", e))?;
        let (audit_high, events_high, prev_last, prev_hash, prev_epoch, interval_passed, numbered) =
            state;
        let chain_broken = |detail| {
            mismatch("chain", detail)
                .with_context("last_sealed_seq", shown(prev_last))
                .with_context("audit_high_seq", shown(audit_high))
        };
        let (Some(audit_high), Some(events_high)) = (audit_high, events_high) else {
            // assign_seq creates the row before it numbers anything: without
            // it, nothing may be numbered or sealed yet.
            if prev_last.is_some() || numbered {
                return Err(chain_broken("feed_state_missing"));
            }
            return finish(tx, SealOutcome::UpToDate).await;
        };
        let sealed_to = prev_last.unwrap_or(0);
        if audit_high < sealed_to {
            // audit_high_seq never decreases (forge_seq's guard): the feed
            // state or the seals were changed or restored.
            return Err(chain_broken("audit_high_below_seal"));
        }
        if audit_high == sealed_to {
            return finish(tx, SealOutcome::UpToDate).await;
        }
        let waiting = audit_high - sealed_to;
        if let Some(p) = policy {
            let due =
                prev_last.is_none() || interval_passed == Some(true) || waiting > p.eager_rows;
            if !due {
                return finish(tx, SealOutcome::NotDue).await;
            }
        }
        let first = sealed_to + 1; // sealed_to < audit_high <= i64::MAX
        let last = audit_high.min(first.saturating_add(MAX_SEAL_ROWS - 1));
        let range = SeqRange::new(first, last).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "seal.range")
                .with_context("reason", "empty_range")
                .with_context("first_seq", first)
                .with_context("last_seq", last)
        })?;
        let rows = sqlx::query(v1::SELECT)
            .bind(range.first())
            .bind(range.last())
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| query_error("seal.rows", e))?
            .iter()
            .map(v1::Row::from_pg)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| query_error("seal.rows", e))?;
        if let Some(missing) = gap_in(range, rows.iter().map(|r| r.seq)) {
            return Err(mismatch("gap", "missing_rows")
                .with_context("first_seq", range.first())
                .with_context("last_seq", range.last())
                .with_context("missing_seq", missing));
        }
        let hashes = hash_rows(rows).await?;
        let prev = match prev_hash {
            Some(bytes) => chain::digest("seal_sha256", &bytes)?,
            None => chain::genesis(),
        };
        let key = keys.signing();
        let fields = SealFields {
            hash_version: v1::VERSION,
            seal_epoch: prev_epoch.unwrap_or(FIRST_EPOCH),
            key_id: key.id(),
            kind: SealKind::Normal,
            first_seq: range.first(),
            last_seq: range.last(),
            events_high_seq: events_high,
            rows_sha256: chain::rows_sha256(&hashes),
            prev_seal_sha256: prev,
        };
        let seal = fields.seal_sha256();
        let mac = chain::hmac(key, &seal)?;
        let seal_id: i64 = sqlx::query_scalar(
            "INSERT INTO forge.audit_seal (kind, hash_version, seal_epoch, key_id, first_seq, \
             last_seq, row_count, events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, \
             hmac) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING id",
        )
        .bind(fields.kind.as_str())
        .bind(fields.hash_version)
        .bind(fields.seal_epoch)
        .bind(fields.key_id)
        .bind(fields.first_seq)
        .bind(fields.last_seq)
        .bind(range.count())
        .bind(fields.events_high_seq)
        .bind(fields.rows_sha256.as_slice())
        .bind(fields.prev_seal_sha256.as_slice())
        .bind(seal.as_slice())
        .bind(mac.as_slice())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error("seal.insert", e))?;
        tx.commit()
            .await
            .map_err(|e| query_error("seal.commit", e))?;
        Ok(SealOutcome::Sealed(Sealed {
            seal_id,
            range,
            more: range.last() < audit_high,
        }))
    })
    .await
}

fn shown(v: Option<i64>) -> String {
    v.map_or_else(|| "none".to_owned(), |n| n.to_string())
}
