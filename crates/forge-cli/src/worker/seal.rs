//! The `audit_seal` task of `forge worker` (ADR-0018 §4, the `audit.seal`
//! work): every [`SEAL_POLL_PERIOD`] it asks forge-db to seal when due
//! ([`SealPolicy::WORKER`]: a minute after the last seal, or sooner when
//! more than 1000 numbered rows wait). After a seal that left rows
//! (`more`), the next run follows at once and seals without the policy, so
//! a backlog drains in steps of `MAX_SEAL_ROWS` without a pause.
//!
//! A supervised task, not a job of the C6 queue (ADR-0018 C8 amendment):
//! the work is one periodic singleton (the advisory lock already keeps two
//! workers from sealing twice), it would otherwise need a scheduler to
//! enqueue it every minute, and it must not wait behind other jobs on the
//! one-job-at-a-time runner. A failed run (FG-DB-0011 reason=gap, …) is
//! returned and logged once per failure streak by the supervisor with
//! `error_code`, `task=audit_seal` and the streak's `request_id`, then
//! retried after the backoff — sealing stays stopped at the failure until
//! it is resolved, every other task keeps running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use forge_core::config::SealKeys;
use forge_db::seal::{self, SealOutcome, SealPolicy};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// How often the task looks whether a seal is due.
pub(crate) const SEAL_POLL_PERIOD: Duration = Duration::from_secs(10);

/// One run: seal when due (or at once while draining).
pub(crate) async fn run_seal(
    db: &Db,
    keys: &SealKeys,
    draining: &AtomicBool,
) -> Result<Next, ForgeError> {
    let outcome = if draining.load(Ordering::Relaxed) {
        seal::seal_pending(db, keys).await
    } else {
        seal::seal_due(db, keys, SealPolicy::WORKER).await
    };
    // A failure ends the drain: the retry starts from the policy again.
    let outcome = outcome.inspect_err(|_| draining.store(false, Ordering::Relaxed))?;
    let more = match outcome {
        SealOutcome::Sealed(s) => {
            tracing::debug!(
                task = "audit_seal",
                seal_id = s.seal_id,
                first_seq = s.range.first(),
                last_seq = s.range.last(),
                more = s.more,
                "forge.worker.sealed"
            );
            s.more
        }
        SealOutcome::Busy => {
            // Another worker (or a manual call) holds the seal lock.
            tracing::debug!(task = "audit_seal", "forge.worker.seal_busy");
            false
        }
        SealOutcome::UpToDate | SealOutcome::NotDue => false,
    };
    draining.store(more, Ordering::Relaxed);
    Ok(if more {
        Next::Immediately
    } else {
        Next::AfterPeriod
    })
}
