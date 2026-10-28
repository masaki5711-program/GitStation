//! The audit seal (ADR-0018 §4): `forge.audit_seal` chains SHA-256 seals
//! over consecutive ranges of numbered `forge.audit_log` rows, each with an
//! HMAC under a key kept outside the database (`audit.seal_keys_file`).
//!
//! - [`v1`] — hash version 1: the sealed columns, their SELECT and the
//!   canonical (RFC 8785) row bytes, in one place.
//! - [`seal_pending`] — the body of the `audit.seal` job (forge worker):
//!   seal from the last seal's `last_seq + 1` to `audit_high_seq`.
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
pub use job::{seal_pending, SealOutcome, Sealed, MAX_SEAL_ROWS};
pub use verify::{verify_range, HmacCheck, Verified, VerifyScope, DOCTOR_WINDOW};

/// The key of the seal job's transaction advisory lock
/// (`pg_try_advisory_xact_lock(1718579815, 8)`; the sequencer uses 1).
pub(crate) const LOCK_KEY: (i32, i32) = (1_718_579_815, 8);

fn mismatch(reason: &'static str, detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AuditSealMismatch)
        .with_context("reason", reason)
        .with_context("detail", detail)
}

/// The first number of `range` missing from `seqs` (ascending, as SELECTed).
fn gap_in(range: SeqRange, seqs: impl IntoIterator<Item = i64>) -> Option<i64> {
    let mut expected = range.first();
    for seq in seqs {
        if seq != expected {
            return Some(expected);
        }
        expected += 1;
    }
    (expected <= range.last()).then_some(expected)
}

#[cfg(test)]
mod golden;
#[cfg(test)]
mod tamper_tests;
#[cfg(test)]
mod tests;

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
