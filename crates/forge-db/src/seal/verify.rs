//! Recomputing seals (ADR-0018 §4): `forge doctor audit.seal_verify` checks
//! the seals of the last 24 hours; C9's `audit.seal_reverify` walks every
//! seal range by range.
//!
//! Each seal is checked in its own transaction on the write pool (the
//! sealed act-as columns are not readable by the read pool's forge_ro) that
//! only SELECTs and always rolls back, with `search_path = pg_catalog,
//! pg_temp`. It runs at the default READ COMMITTED: a sealed range and its
//! seal row never change legitimately between its statements (redaction
//! touches unsealed columns only; shipped_at is not read). The root gate
//! db-write-boundary keeps `SET TRANSACTION` inside ChangeTx, so the
//! transaction is not marked READ ONLY / REPEATABLE READ (C9's full
//! reverify, which runs beside purge, may want the gate to allow it). The
//! order of the checks, the first failure being `FG-DB-0011` with
//! `reason` / `detail`:
//! hmac (mismatch | unknown_key_id; skipped without keys) → chain
//! (prev_seal_sha256 | first_seq) → recompute (hash_version | kind |
//! seal_sha256) → gap (missing_rows) → recompute (rows_sha256).
//!
//! `sealed_at` is not authenticated (neither in seal_sha256 nor in the
//! HMAC: it is set by the guard, and only a party that can disable the
//! guard can change it). Backdating a seal moves it out of the doctor's
//! 24-hour window ([`VerifyScope::SealedWithin`]); its rows are then
//! rechecked by the walk over every seal (C9 `audit.seal_reverify`,
//! ≤ 7 days, [`VerifyScope::Seqs`]), and the chain of the newer seals
//! still binds its seal_sha256. Each list is bounded by
//! [`MAX_VERIFY_SEALS`] (oldest first; `Verified::truncated` says the
//! rest was not checked).

use std::time::Duration;

use forge_core::config::SealKeys;
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use super::chain::{self, SealFields, SealKind};
use super::{gap_in, hash_rows, mismatch, v1};
use crate::sequencer::SeqRange;
use crate::{query_error, Db};

/// Which seals to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyScope {
    /// Seals written within this long before now (doctor: 24 h).
    SealedWithin(Duration),
    /// Seals whose range overlaps this one.
    Seqs(SeqRange),
}

/// The window of `forge doctor audit.seal_verify`.
pub const DOCTOR_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// The most seals one call checks: a day of one-minute seals with room
/// for drains (each seal is its own bounded transaction).
pub const MAX_VERIFY_SEALS: i64 = 5000;

/// Whether the HMACs were checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacCheck {
    Checked,
    /// No `audit.seal_keys_file` here: values and chain only.
    NoKeys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    pub seals: usize,
    pub rows: i64,
    pub hmac: HmacCheck,
    /// More than [`MAX_VERIFY_SEALS`] seals were in scope: the newest
    /// ones were not checked.
    pub truncated: bool,
}

/// Verify the seals in `scope`, oldest first. Read-only. Each seal is
/// bounded on the client by [`Db::within`].
pub async fn verify_range(
    db: &Db,
    keys: Option<&SealKeys>,
    scope: VerifyScope,
) -> Result<Verified, ForgeError> {
    let ids = db
        .within("seal.verify_list", async {
            let mut conn = write_conn(db).await?;
            let mut tx = begin(&mut conn).await?;
            let ids: Vec<i64> = match scope {
                VerifyScope::SealedWithin(window) => {
                    sqlx::query_scalar(
                        "SELECT s.id FROM ONLY forge.audit_seal AS s \
                     WHERE s.sealed_at >= pg_catalog.now() \
                           - $1::double precision * interval '1 millisecond' \
                     ORDER BY s.last_seq LIMIT $2",
                    )
                    .bind(window.as_millis() as f64)
                    .bind(MAX_VERIFY_SEALS + 1)
                    .fetch_all(&mut *tx)
                    .await
                }
                VerifyScope::Seqs(range) => {
                    sqlx::query_scalar(
                        "SELECT s.id FROM ONLY forge.audit_seal AS s \
                     WHERE s.last_seq >= $1 AND s.first_seq <= $2 ORDER BY s.last_seq \
                     LIMIT $3",
                    )
                    .bind(range.first())
                    .bind(range.last())
                    .bind(MAX_VERIFY_SEALS + 1)
                    .fetch_all(&mut *tx)
                    .await
                }
            }
            .map_err(|e| query_error("seal.verify_list", e))?;
            end(tx).await?;
            Ok(ids)
        })
        .await?;
    let limit = usize::try_from(MAX_VERIFY_SEALS).unwrap_or(usize::MAX);
    let truncated = ids.len() > limit;
    let ids = ids.get(..limit).unwrap_or(&ids);
    let mut rows: i64 = 0;
    for id in ids {
        let n = db
            .within("seal.verify", async {
                let mut conn = write_conn(db).await?;
                let mut tx = begin(&mut conn).await?;
                let n = verify_one(&mut tx, keys, *id).await?;
                end(tx).await?;
                Ok(n)
            })
            .await?;
        rows = rows.saturating_add(n);
    }
    Ok(Verified {
        seals: ids.len(),
        rows,
        hmac: keys.map_or(HmacCheck::NoKeys, |_| HmacCheck::Checked),
        truncated,
    })
}

type Conn = sqlx::pool::PoolConnection<sqlx::Postgres>;

async fn write_conn(db: &Db) -> Result<Conn, ForgeError> {
    db.write_pool()
        .acquire()
        .await
        .map_err(|e| query_error("seal.acquire", e))
}

async fn begin(conn: &mut Conn) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, ForgeError> {
    let mut tx = conn
        .begin()
        .await
        .map_err(|e| query_error("seal.verify_begin", e))?;
    // Names resolve in pg_catalog only (forge belongs to forge_owner, who
    // could plant a function that makes a comparison pass).
    sqlx::query("SET LOCAL search_path = pg_catalog, pg_temp")
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error("seal.verify_begin", e))?;
    Ok(tx)
}

async fn end(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), ForgeError> {
    // Read-only: nothing to keep.
    tx.rollback()
        .await
        .map_err(|e| query_error("seal.verify_end", e))
}

type SealRow = (
    String,
    i16,
    i16,
    i16,
    i64,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
);

/// One seal; returns its row count.
async fn verify_one(
    conn: &mut PgConnection,
    keys: Option<&SealKeys>,
    id: i64,
) -> Result<i64, ForgeError> {
    let (kind, version, epoch, key_id, first, last, events_high, rows_hash, prev, seal, mac): SealRow =
        sqlx::query_as(
            "SELECT s.kind, s.hash_version, s.seal_epoch, s.key_id, s.first_seq, s.last_seq, \
                    s.events_high_seq, s.rows_sha256, s.prev_seal_sha256, s.seal_sha256, s.hmac \
             FROM ONLY forge.audit_seal AS s WHERE s.id = $1",
        )
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error("seal.verify_seal", e))?;
    let found = |reason, detail| {
        mismatch(reason, detail)
            .with_context("seal_id", id)
            .with_context("first_seq", first)
            .with_context("last_seq", last)
    };
    let stored_seal = chain::digest("seal_sha256", &seal)?;
    if let Some(keys) = keys {
        let Some(key) = keys.get(key_id) else {
            return Err(found("hmac", "unknown_key_id").with_context("key_id", key_id));
        };
        if !chain::hmac_matches(key, &stored_seal, &mac)? {
            return Err(found("hmac", "mismatch").with_context("key_id", key_id));
        }
    }
    let expected_prev: Option<Vec<u8>> = if first == 1 {
        Some(chain::genesis().to_vec())
    } else {
        sqlx::query_scalar(
            "SELECT s.seal_sha256 FROM ONLY forge.audit_seal AS s WHERE s.last_seq = $1",
        )
        .bind(first.saturating_sub(1))
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("seal.verify_prev", e))?
    };
    match expected_prev {
        None => return Err(found("chain", "first_seq")),
        Some(p) if p != prev => return Err(found("chain", "prev_seal_sha256")),
        Some(_) => {}
    }
    // The version decides the columns; only version 1 exists.
    if version != v1::VERSION {
        return Err(found("recompute", "hash_version").with_context("hash_version", version));
    }
    let kind = SealKind::parse(&kind).ok_or_else(|| found("recompute", "kind"))?;
    let range = SeqRange::new(first, last).ok_or_else(|| found("recompute", "seal_sha256"))?;
    let fields = SealFields {
        hash_version: version,
        seal_epoch: epoch,
        key_id,
        kind,
        first_seq: first,
        last_seq: last,
        events_high_seq: events_high,
        rows_sha256: chain::digest("rows_sha256", &rows_hash)?,
        prev_seal_sha256: chain::digest("prev_seal_sha256", &prev)?,
    };
    if fields.seal_sha256() != stored_seal {
        return Err(found("recompute", "seal_sha256"));
    }
    let rows = sqlx::query(v1::SELECT)
        .bind(first)
        .bind(last)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| query_error("seal.verify_rows", e))?
        .iter()
        .map(v1::Row::from_pg)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| query_error("seal.verify_rows", e))?;
    if let Some(missing) = gap_in(range, rows.iter().map(|r| r.seq)) {
        return Err(found("gap", "missing_rows").with_context("missing_seq", missing));
    }
    let hashes = hash_rows(rows).await?;
    if chain::rows_sha256(&hashes) != fields.rows_sha256 {
        return Err(found("recompute", "rows_sha256"));
    }
    Ok(range.count())
}
