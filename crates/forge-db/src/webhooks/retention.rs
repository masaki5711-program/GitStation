//! The delivery history's retention (`webhook.delivery_retention_days`,
//! the worker's `webhook_delivery_retention` task): finished deliveries
//! older than the period are deleted, a bounded batch per call (derived
//! rows: no audit row). Pending deliveries are never deleted here.

use forge_errors::ForgeError;

use crate::{query_error, Db};

/// The most rows one call deletes (the caller runs again at once when a
/// call deleted this many).
pub const RETENTION_BATCH: i64 = 1000;

/// Delete up to `batch` deliveries finished more than `days` days ago;
/// returns how many were deleted.
pub async fn purge_deliveries(db: &Db, days: u32, batch: i64) -> Result<u64, ForgeError> {
    const QUERY: &str = "webhooks.purge_deliveries";
    db.within(QUERY, async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("webhooks.acquire", e))?;
        let done = sqlx::query(
            "DELETE FROM forge.webhook_deliveries WHERE id IN ( \
             SELECT id FROM forge.webhook_deliveries \
             WHERE finished_at < now() - make_interval(days => $1) \
             ORDER BY finished_at LIMIT $2)",
        )
        .bind(i32::try_from(days).unwrap_or(i32::MAX))
        .bind(batch.clamp(1, 10_000))
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        Ok(done.rows_affected())
    })
    .await
}
