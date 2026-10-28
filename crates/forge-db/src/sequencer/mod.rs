//! The outbox sequencer (ADR-0016 §7, ADR-0018 §3): [`run_once`] calls
//! `forge.assign_seq` on the write pool — it numbers committed events (and,
//! when asked and enabled, audit rows) gap-free in id order — and, in the
//! same transaction, sends `NOTIFY forge_events_sequenced` when it numbered
//! events (delivered at COMMIT; writers never notify). `forge worker` runs
//! it every 200 ms.
//!
//! [`backlog`] reads what waits for a number (read pool) for `forge doctor`
//! (`outbox.unsequenced_count`, `outbox.sequencer_lag` → `FG-DB-0005`).

use forge_errors::{ErrorCode, ForgeError};
use sqlx::Connection as _;

use crate::{query_error, Db};

mod lag;

pub use lag::{backlog, shown, Backlog, COUNT_CAP, LAG_AFTER};

/// The channel `run_once` notifies; the payload is the batch's last seq.
pub const CHANNEL: &str = "forge_events_sequenced";

/// The key of the sequencer's transaction advisory lock
/// (`pg_advisory_xact_lock(1718579815, 1)` in forge.assign_seq).
pub(crate) const LOCK_KEY: (i64, i64) = (1_718_579_815, 1);

/// Rows numbered per run: 1..=1000 (the function and the feed-state guard
/// allow no more).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchSize(u16);

impl BatchSize {
    /// The full batch the worker uses.
    pub const MAX: Self = Self(1000);

    pub fn new(n: u16) -> Option<Self> {
        (1..=Self::MAX.0).contains(&n).then_some(Self(n))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

/// Whether this run may number audit rows. The function numbers them only
/// when this is `IfEnabled` AND `event_feed_state.audit_sequencing_enabled`
/// (ADR-0018 §5: a process passes `IfEnabled` only once its own sink
/// comparison succeeded — that state arrives in C9; until then the worker
/// passes `Skip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSequencing {
    Skip,
    IfEnabled,
}

impl AuditSequencing {
    fn requested(self) -> bool {
        match self {
            Self::Skip => false,
            Self::IfEnabled => true,
        }
    }
}

/// A non-empty, ascending range of sequence numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    first: i64,
    last: i64,
}

impl SeqRange {
    pub fn new(first: i64, last: i64) -> Option<Self> {
        (first >= 1 && first <= last).then_some(Self { first, last })
    }

    pub fn first(self) -> i64 {
        self.first
    }

    pub fn last(self) -> i64 {
        self.last
    }

    pub fn count(self) -> i64 {
        self.last - self.first + 1
    }
}

/// What one run numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Assigned {
    pub events: Option<SeqRange>,
    pub audit: Option<SeqRange>,
}

impl Assigned {
    pub fn is_empty(&self) -> bool {
        self.events.is_none() && self.audit.is_none()
    }
}

/// One sequencer step, bounded on the client by [`Db::within`]
/// (`FG-DB-0001 reason=operation_timeout`; the database's lock and
/// statement timeouts bound it on the server). A failure rolls back: the
/// rows keep waiting and the next run numbers them. A COMMIT whose answer is
/// lost may or may not have numbered a batch — harmless, the next run
/// continues from `high_seq` either way.
pub async fn run_once(
    db: &Db,
    batch: BatchSize,
    audit: AuditSequencing,
) -> Result<Assigned, ForgeError> {
    db.within("sequencer.run_once", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("sequencer.acquire", e))?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| query_error("sequencer.begin", e))?;
        let row: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT from_seq, to_seq, audit_from, audit_to FROM forge.assign_seq($1, $2)",
        )
        .bind(i32::from(batch.get()))
        .bind(audit.requested())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| query_error("sequencer.assign_seq", e))?;
        let assigned = Assigned {
            events: range_of(row.0, row.1, "events")?,
            audit: range_of(row.2, row.3, "audit")?,
        };
        // Subscribers read events only; an audit-only batch wakes nobody.
        if let Some(events) = assigned.events {
            sqlx::query("SELECT pg_catalog.pg_notify($1, $2)")
                .bind(CHANNEL)
                .bind(events.last().to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error("sequencer.notify", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| query_error("sequencer.commit", e))?;
        Ok(assigned)
    })
    .await
}

/// Both ends or neither (the function sets them together).
fn range_of(
    first: Option<i64>,
    last: Option<i64>,
    rows: &'static str,
) -> Result<Option<SeqRange>, ForgeError> {
    let inconsistent = || {
        ForgeError::new(ErrorCode::Internal)
            .with_context("operation", "sequencer.assign_seq")
            .with_context("reason", "inconsistent_range")
            .with_context("rows", rows)
    };
    match (first, last) {
        (None, None) => Ok(None),
        (Some(first), Some(last)) => SeqRange::new(first, last)
            .map(Some)
            .ok_or_else(inconsistent),
        (Some(_), None) | (None, Some(_)) => Err(inconsistent()),
    }
}

#[cfg(test)]
mod doctor_tests;
#[cfg(test)]
mod failure_tests;
#[cfg(test)]
pub(crate) mod tests;
