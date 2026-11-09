//! The `in_app_notification_retention` task of `forge worker` (M3 U4,
//! ADR-0016 Amendment U4): every [`PERIOD`] it deletes one batch of in-app
//! notifications read more than 30 days ago or created more than 180 days
//! ago (forge-db `notify::in_app::expire`; derived rows, no audit row). A
//! full batch runs again after [`BATCH_PAUSE`], so a backlog drains without
//! waiting a period per batch.
//!
//! A failed run (the database: `FG-DB-0001` / `FG-DB-0002`) is returned and
//! logged once per failure streak by the supervisor with `error_code` and
//! `task=in_app_notification_retention`, then retried after the backoff; a
//! batch is one statement, so nothing is half done.

use std::time::Duration;

use forge_db::notify::in_app::{expire, MAX_AGE_DAYS, READ_RETENTION_DAYS, RETENTION_BATCH};
use forge_db::Db;
use forge_errors::ForgeError;

use super::supervise::Next;

/// The task's name in the supervisor's logs.
pub(crate) const NAME: &str = "in_app_notification_retention";
/// How often a batch runs when nothing is left.
pub(crate) const PERIOD: Duration = Duration::from_secs(3600);
/// The pause after a full batch before the next one.
const BATCH_PAUSE: Duration = Duration::from_millis(500);

/// One run: one batch; a full one asks to run again after a short pause.
pub(crate) async fn run_in_app_retention(db: &Db) -> Result<Next, ForgeError> {
    let deleted = expire(db, RETENTION_BATCH).await?;
    if deleted > 0 {
        tracing::info!(
            target: "forge_cli",
            task = NAME,
            deleted,
            read_older_than_days = READ_RETENTION_DAYS,
            older_than_days = MAX_AGE_DAYS,
            "forge.worker.in_app_notifications_expired"
        );
    }
    if i64::try_from(deleted).unwrap_or(i64::MAX) >= RETENTION_BATCH {
        tokio::time::sleep(BATCH_PAUSE).await;
        return Ok(Next::Immediately);
    }
    Ok(Next::AfterPeriod)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;

    /// A run on a database with nothing due waits for the period.
    #[tokio::test]
    async fn a_run_with_nothing_due_waits_for_the_period() {
        let t = FreshDb::migrated().await;
        assert_eq!(
            run_in_app_retention(&t.db).await.expect("run"),
            Next::AfterPeriod
        );
        t.drop_now().await;
    }
}
