//! The `events_retention` task of `forge worker` (M3 I1c, ADR-0016 §2):
//! every [`PERIOD`] it runs one batch of `forge.events_retention` (owned by
//! forge_purge; forge-db `feed::run_retention`): events older than
//! `forge.retention_policy.events_days` (default 90) go with their work
//! items, held back by the retention-blocking subscribers until 1.5 × the
//! period, and `retained_from` moves past them (one `events.retention`
//! audit row per batch that deleted something). A batch that reached its
//! window's end runs again after a 500 ms pause, so a backlog drains without waiting a
//! period per batch. Consumers whose cursor was older answer 410
//! `retention` (FG-INT-0007) and resync.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`) is returned and
//! logged once per failure streak by the supervisor with `error_code` and
//! `task=events_retention`, then retried after the backoff; a batch is one
//! transaction, so nothing is half done.

use std::time::Duration;

use forge_core::RequestId;
use forge_db::feed::{run_retention, RETENTION_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "events_retention";
/// How often a batch runs when nothing is left.
pub(crate) const PERIOD: Duration = Duration::from_secs(3600);
/// The pause after a full batch before the next one.
pub(crate) const BATCH_PAUSE: Duration = Duration::from_millis(500);

/// One run: one batch; a full window asks to run again at once.
pub(crate) async fn run_events_retention(db: &Db) -> Result<Next, ForgeError> {
    let request_id = RequestId::generate();
    let batch = run_retention(db, &request_id, RETENTION_BATCH).await?;
    let Some((from, to)) = batch.range else {
        return Ok(Next::AfterPeriod);
    };
    tracing::info!(
        target: "forge_cli",
        task = NAME,
        request_id = %request_id,
        from_seq = from,
        to_seq = to,
        deleted = batch.deleted,
        "forge.worker.events_retained"
    );
    if to - from + 1 >= i64::from(RETENTION_BATCH) {
        // A short pause between full batches (review I1c): the sequencer
        // and the feed get the database between them.
        tokio::time::sleep(BATCH_PAUSE).await;
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run on a migrated database without a feed state does nothing and
    /// waits for the period.
    #[tokio::test]
    async fn a_run_without_events_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_events_retention(&t.db).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
