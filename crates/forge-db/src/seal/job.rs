//! The body of the `audit.seal` job (ADR-0018 §4): seal the numbered audit
//! rows after the last seal, at most [`MAX_SEAL_ROWS`] per call.

use forge_core::config::SealKeys;
use forge_errors::ForgeError;
use sqlx::Connection as _;

use super::chain::{self, SealFields};
use super::{gap_in, mismatch, v1, LOCK_KEY};
use crate::sequencer::SeqRange;
use crate::{query_error, Db};

/// Rows per seal: bounds one call's work (read, hash, insert) well inside
/// the client-side bound. A larger backlog takes several calls (`more`).
pub const MAX_SEAL_ROWS: i64 = 5000;

/// The epoch of the first seal (C9's restore seal increments it).
const FIRST_EPOCH: i16 = 1;

/// audit_high_seq, high_seq, and the latest seal's last_seq, seal_sha256
/// and seal_epoch (none before the first seal).
type State = (i64, i64, Option<i64>, Option<Vec<u8>>, Option<i16>);

/// What one call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealOutcome {
    /// Another caller holds the seal lock (it is sealing right now).
    Busy,
    /// Every numbered audit row is sealed (or nothing is numbered yet).
    UpToDate,
    Sealed(Sealed),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sealed {
    pub seal_id: i64,
    pub range: SeqRange,
    /// Numbered rows remain after this seal: call again now.
    pub more: bool,
}

/// One seal step on the write pool, in one transaction under
/// `pg_try_advisory_xact_lock` (concurrent callers never seal twice: the
/// loser returns [`SealOutcome::Busy`]), bounded on the client by
/// [`Db::within`] (`FG-DB-0001 reason=operation_timeout`) and on the server
/// by the statement and lock timeouts. Cancel-safe: dropping the future
/// drops the transaction (rolled back), and nothing is written outside it.
///
/// A hole in the numbers of the range is `FG-DB-0011 reason=gap` and
/// nothing is sealed. A row that hash version 1 cannot express is
/// `FG-SYS-0001` (operation `seal.v1`, with `seq`).
pub async fn seal_pending(db: &Db, keys: &SealKeys) -> Result<SealOutcome, ForgeError> {
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
        let state: Option<State> = sqlx::query_as(
            "SELECT f.audit_high_seq, f.high_seq, p.last_seq, p.seal_sha256, p.seal_epoch \
             FROM ONLY forge.event_feed_state AS f \
             LEFT JOIN LATERAL (SELECT s.last_seq, s.seal_sha256, s.seal_epoch \
                                FROM ONLY forge.audit_seal AS s \
                                ORDER BY s.last_seq DESC LIMIT 1) AS p ON true \
             WHERE f.singleton",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error("seal.state", e))?;
        let Some((audit_high, events_high, prev_last, prev_hash, prev_epoch)) = state else {
            return Ok(SealOutcome::UpToDate);
        };
        let first = prev_last.unwrap_or(0) + 1;
        let Some(range) = SeqRange::new(first, audit_high.min(first + MAX_SEAL_ROWS - 1)) else {
            return Ok(SealOutcome::UpToDate);
        };
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
        let hashes = rows
            .iter()
            .map(v1::Row::sha256)
            .collect::<Result<Vec<_>, _>>()?;
        let prev = match prev_hash {
            Some(bytes) => chain::digest("seal_sha256", &bytes)?,
            None => chain::genesis(),
        };
        let fields = SealFields {
            hash_version: v1::VERSION,
            seal_epoch: prev_epoch.unwrap_or(FIRST_EPOCH),
            first_seq: range.first(),
            last_seq: range.last(),
            events_high_seq: events_high,
            rows_sha256: chain::rows_sha256(&hashes),
            prev_seal_sha256: prev,
        };
        let seal = fields.seal_sha256();
        let key = keys.signing();
        let mac = chain::hmac(key, &seal)?;
        let seal_id: i64 = sqlx::query_scalar(
            "INSERT INTO forge.audit_seal (kind, hash_version, seal_epoch, key_id, first_seq, \
             last_seq, row_count, events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, \
             hmac) VALUES ('normal', $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
        )
        .bind(fields.hash_version)
        .bind(fields.seal_epoch)
        .bind(key.id())
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
