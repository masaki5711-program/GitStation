//! Reads of webhooks and deliveries on the read pool (which has no grant
//! on the sealed secrets).

use forge_core::ids::{ProjectId, UserId, WebhookDeliveryId, WebhookId};
use forge_errors::ForgeError;

use super::{disable_reason, parse_uuid, utc, Delivery, DeliveryState, Webhook, MAX_WEBHOOKS};
use crate::{read_query_error, Db};

/// The most deliveries one page lists.
pub const MAX_DELIVERIES_LISTED: i64 = 100;

const SELECT: &str = concat!(
    "SELECT w.id, w.project_id, w.owner_user_id, w.origin, w.event_types, w.enabled, ",
    "w.disabled_reason, w.secret_version, ",
    "CASE WHEN w.secret_prev_until > now() THEN ",
    utc!("w.secret_prev_until"),
    " END, w.consecutive_failures, ",
    utc!("w.failing_since"),
    ", ",
    utc!("w.last_delivery_at"),
    ", ",
    utc!("w.created_at"),
    ", ",
    utc!("w.updated_at"),
    " FROM forge.webhooks w "
);

type Row = (
    i64,
    i64,
    i64,
    String,
    Vec<String>,
    bool,
    Option<String>,
    i32,
    Option<String>,
    i32,
    Option<String>,
    Option<String>,
    String,
    String,
);

pub(super) fn webhook(row: Row) -> Result<Webhook, ForgeError> {
    let (
        id,
        project,
        owner,
        origin,
        mut event_types,
        enabled,
        disabled,
        secret_version,
        previous_secret_until,
        consecutive_failures,
        failing_since,
        last_delivery_at,
        created_at,
        updated_at,
    ) = row;
    event_types.sort();
    Ok(Webhook {
        id: WebhookId::new(id),
        project: ProjectId::new(project),
        owner: UserId::new(owner),
        origin,
        event_types,
        enabled,
        disabled_reason: disable_reason(id, disabled.as_deref())?,
        secret_version,
        previous_secret_until,
        consecutive_failures,
        failing_since,
        last_delivery_at,
        created_at,
        updated_at,
    })
}

/// The webhooks of `project`, by id (at most [`MAX_WEBHOOKS`]).
pub async fn list(db: &Db, project: ProjectId) -> Result<Vec<Webhook>, ForgeError> {
    const QUERY: &str = "webhooks.list";
    let rows: Vec<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!(
                "{SELECT} WHERE w.project_id = $1 ORDER BY w.id LIMIT $2"
            ))
            .bind(project.get())
            .bind(MAX_WEBHOOKS)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(webhook).collect()
}

/// Webhook `id` of `project` (`None`: absent, or another project's).
pub async fn get(
    db: &Db,
    project: ProjectId,
    id: WebhookId,
) -> Result<Option<Webhook>, ForgeError> {
    const QUERY: &str = "webhooks.get";
    let row: Option<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} WHERE w.project_id = $1 AND w.id = $2"))
                .bind(project.get())
                .bind(id.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(webhook).transpose()
}

type DeliveryRow = (
    i64,
    String,
    i64,
    i64,
    String,
    String,
    String,
    i32,
    Option<i32>,
    Option<String>,
    Option<String>,
    Option<i32>,
    i32,
    String,
    Option<String>,
    Option<String>,
);

const DELIVERY_SELECT: &str = concat!(
    "SELECT d.id, d.delivery_uid::text, d.webhook_id, d.event_seq, d.event_id::text, ",
    "d.event_type, d.state, d.attempts, d.last_status, d.last_reason, d.last_error_code, ",
    "d.last_duration_ms, d.redeliveries, ",
    utc!("d.created_at"),
    ", ",
    utc!("d.last_attempt_at"),
    ", ",
    utc!("d.finished_at"),
    " FROM forge.webhook_deliveries d "
);

pub(super) fn delivery(row: DeliveryRow) -> Result<Delivery, ForgeError> {
    let (
        id,
        uid,
        webhook,
        event_seq,
        event_id,
        event_type,
        state,
        attempts,
        last_status,
        last_reason,
        last_error_code,
        last_duration_ms,
        redeliveries,
        created_at,
        last_attempt_at,
        finished_at,
    ) = row;
    Ok(Delivery {
        id: WebhookDeliveryId::new(id),
        delivery_uid: parse_uuid(&uid, "delivery_uid")?,
        webhook: WebhookId::new(webhook),
        event_seq,
        event_id: parse_uuid(&event_id, "event_id")?,
        event_type,
        state: DeliveryState::parse(&state)?,
        attempts,
        last_status,
        last_reason,
        last_error_code,
        last_duration_ms,
        redeliveries,
        created_at,
        last_attempt_at,
        finished_at,
    })
}

/// The deliveries of webhook `id` of `project`, newest first, before
/// delivery `before` when given, at most `limit` (1..=100). `None`: the
/// webhook is absent (or another project's).
pub async fn deliveries(
    db: &Db,
    project: ProjectId,
    id: WebhookId,
    before: Option<WebhookDeliveryId>,
    limit: i64,
) -> Result<Option<Vec<Delivery>>, ForgeError> {
    const QUERY: &str = "webhooks.deliveries";
    let limit = limit.clamp(1, MAX_DELIVERIES_LISTED);
    let found: Option<Vec<DeliveryRow>> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM forge.webhooks WHERE id = $1 AND project_id = $2)",
            )
            .bind(id.get())
            .bind(project.get())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            if !exists {
                return Ok(None);
            }
            let rows = sqlx::query_as(&format!(
                "{DELIVERY_SELECT} WHERE d.webhook_id = $1 AND ($2::bigint IS NULL OR d.id < $2) \
                 ORDER BY d.id DESC LIMIT $3"
            ))
            .bind(id.get())
            .bind(before.map(WebhookDeliveryId::get))
            .bind(limit)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            Ok(Some(rows))
        })
        .await?;
    found
        .map(|rows| rows.into_iter().map(delivery).collect())
        .transpose()
}

/// What `forge doctor`'s `webhook.deliveries` reports (the read pool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhooksHealth {
    pub webhooks: i64,
    /// Deliveries that ended `failed` in the last 24 hours (`FG-INT-0004`).
    pub failed_24h: i64,
    /// Webhooks the worker disabled in the last 24 hours (`FG-INT-0011`).
    pub disabled_24h: i64,
    /// The oldest pending delivery's age in seconds.
    pub oldest_pending_s: Option<i64>,
    /// Pending deliveries without a live job for [`super::STUCK_AFTER`]
    /// (the sweep fails them: their job died).
    pub stuck_pending: i64,
}

/// [`WebhooksHealth`] now.
pub async fn health(db: &Db) -> Result<WebhooksHealth, ForgeError> {
    const QUERY: &str = "webhooks.health";
    let row: (i64, i64, i64, Option<i64>, i64) = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!(
                "SELECT (SELECT count(*) FROM forge.webhooks), \
                 (SELECT count(*) FROM forge.webhook_deliveries \
                   WHERE state = 'failed' AND finished_at > now() - interval '24 hours'), \
                 (SELECT count(*) FROM forge.webhooks WHERE NOT enabled \
                   AND disabled_reason IN ('failures', 'failing_24h') \
                   AND updated_at > now() - interval '24 hours'), \
                 (SELECT extract(epoch FROM now() - min(created_at))::bigint \
                   FROM forge.webhook_deliveries WHERE state = 'pending'),                  (SELECT count(*) FROM forge.webhook_deliveries d WHERE {})",
                super::STUCK_WHERE
            ))
            .bind(super::STUCK_AFTER.as_secs_f64())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(WebhooksHealth {
        webhooks: row.0,
        failed_24h: row.1,
        disabled_24h: row.2,
        oldest_pending_s: row.3,
        stuck_pending: row.4,
    })
}
