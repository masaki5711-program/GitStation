//! The `act_as_sweep` task of `forge worker` (M3 S2, ADR-0025 §5.1): every
//! [`PERIOD`] it rejects the act-as assertions still `pending` past their
//! five minutes (`reject_reason = expired`, one `act_as_assertion.rejected`
//! audit row each, as `system`; forge-db `act_as::sweep_expired`, one short
//! transaction of at most `SWEEP_BATCH` rows). A full batch runs again at
//! once. The claim already refuses an expired assertion (`claim_expired`):
//! the sweep only closes the rows.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`) is returned and
//! logged once per failure streak by the supervisor with `error_code` and
//! `task=act_as_sweep`, then retried after the backoff.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::act_as::{sweep_expired, SWEEP_BATCH};
use forge_db::change::{Actor, ChangeContext};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "act_as_sweep";
/// How often a batch runs when nothing is left.
pub(crate) const PERIOD: Duration = Duration::from_secs(60);
/// The audit label of the worker's changes.
const ACTOR_LABEL: &str = "forge-worker";

/// One run: one batch; a full one asks to run again.
pub(crate) async fn run_act_as_sweep(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: logged below
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    let rejected = sweep_expired(db, &ctx, SWEEP_BATCH).await?;
    if rejected > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            request_id = %request_id,
            rejected,
            "forge.worker.act_as_assertions_expired"
        );
    }
    if rejected >= SWEEP_BATCH.unsigned_abs() {
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run on a database with nothing expired waits for the period.
    #[tokio::test]
    async fn a_sweep_with_nothing_expired_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_act_as_sweep(&t.db).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
