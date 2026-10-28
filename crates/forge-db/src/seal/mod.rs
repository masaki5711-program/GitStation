//! The audit seal (ADR-0018 §4): `forge.audit_seal` chains SHA-256 seals
//! over consecutive ranges of numbered `forge.audit_log` rows, each with an
//! HMAC under a key kept outside the database (`audit.seal_keys_file`).
//!
//! - [`v1`] — hash version 1: the sealed columns, their SELECT and the
//!   canonical (RFC 8785) row bytes, in one place.
//! - [`seal_due`] / [`seal_pending`] — the body of the `audit_seal` task of
//!   `forge worker` (with / without [`SealPolicy`]): seal from the last
//!   seal's `last_seq + 1` towards `audit_high_seq`.
//! - [`verify_range`] — recompute seals (`forge doctor audit.seal_verify`).
//! - [`backlog`] — the oldest unsealed row (`audit.seal_backlog`,
//!   `FG-DB-0015`).
//!
//! Mismatches are `FG-DB-0011 AuditSealMismatch` with `reason` = recompute /
//! chain / hmac / gap and a `detail`. The sink (shipping, comparison,
//! resync) is step C9.

use forge_errors::{ErrorCode, ForgeError};

use crate::sequencer::SeqRange;

mod backlog;
pub mod chain;
mod job;
pub mod v1;
mod verify;

pub use backlog::{backlog, SealBacklog, BACKLOG_AFTER};
pub use job::{seal_due, seal_pending, SealOutcome, SealPolicy, Sealed, MAX_SEAL_ROWS};
pub use verify::{verify_range, HmacCheck, Verified, VerifyScope, DOCTOR_WINDOW, MAX_VERIFY_SEALS};

/// The key of the seal job's transaction advisory lock
/// (`pg_try_advisory_xact_lock(1718579815, 8)`; the sequencer uses 1).
pub(crate) const LOCK_KEY: (i32, i32) = (1_718_579_815, 8);

fn mismatch(reason: &'static str, detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AuditSealMismatch)
        .with_context("reason", reason)
        .with_context("detail", detail)
}

/// The row hashes of `rows` (seq order), computed on the blocking pool:
/// canonicalizing and hashing up to a seal of rows is CPU work that must
/// not hold a tokio worker. Cancel-safe: a dropped caller only discards the
/// result (the closure has no side effects).
async fn hash_rows(rows: Vec<v1::Row>) -> Result<Vec<chain::Digest32>, ForgeError> {
    tokio::task::spawn_blocking(move || rows.iter().map(v1::Row::sha256).collect())
        .await
        .map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "seal.hash")
                .with_context("reason", "hash_task_failed")
                .with_source(e)
        })?
}

/// The first number of `range` missing from `seqs` (ascending, as SELECTed).
fn gap_in(range: SeqRange, seqs: impl IntoIterator<Item = i64>) -> Option<i64> {
    let mut expected = range.first();
    for seq in seqs {
        if seq != expected {
            return Some(expected);
        }
        // seq = i64::MAX ends every range.
        expected = expected.checked_add(1)?;
    }
    (expected <= range.last()).then_some(expected)
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod job_tests;
#[cfg(test)]
mod tamper_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod verify_tests;

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn a_gap_is_the_first_missing_number() {
        let r = SeqRange::new(3, 6).expect("range");
        assert_eq!(gap_in(r, [3, 4, 5, 6]), None);
        assert_eq!(gap_in(r, [3, 5, 6]), Some(4));
        assert_eq!(gap_in(r, [3, 4, 5]), Some(6));
        assert_eq!(gap_in(r, []), Some(3));
    }
}
