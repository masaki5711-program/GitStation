//! In-app notifications (M3 U4, ADR-0016 Amendment U4): the SQL of the
//! items the `notify` subscriber records beside the mail record, a
//! person's list and unread items, marking them read, and the retention.
//!
//! Nothing here decides who may see an item: forge-notify decides again on
//! every read (`forge_auth::prove` on `MergeRequestRead`) with the merge
//! requests [`merge_requests`] reads, and lists only what passes. The
//! routes act only for the proof's owner; every statement here is scoped
//! to that user, so another person's item is never touched.

use std::collections::HashMap;

use forge_core::ids::{ProjectId, UserId};
use forge_core::NotificationKind;
use forge_errors::{ErrorCode, ForgeError};

use crate::review::{Row as MergeRequestRow, StoredMergeRequest, COLUMNS};
use crate::subscribers::{DerivedTx, SourceEvent};
use crate::{query_error, Db};

/// The largest page (`limit`).
pub const MAX_PAGE: i64 = 100;
/// The unread count stops at this many (the bell shows "99+").
pub const COUNT_CAP: i64 = 100;
/// Read items are deleted this many days after they were read.
pub const READ_RETENTION_DAYS: i32 = 30;
/// Every item is deleted this many days after it was created.
pub const MAX_AGE_DAYS: i32 = 180;
/// The most rows one retention call deletes.
pub const RETENTION_BATCH: i64 = 1000;

/// One item as stored, with the actor's display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub kind: NotificationKind,
    pub project: ProjectId,
    pub iid: i64,
    pub actor: Option<UserId>,
    /// The actor's display name (`None`: no actor, the system).
    pub actor_name: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub read_at: Option<String>,
}

/// id, kind, project, iid, actor, actor name, created, read.
type ItemRow = (
    i64,
    String,
    i64,
    i64,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
);

/// A page of everything (`$1` user, `$2` the cursor, `$3` the limit).
const ALL_PAGE_SQL: &str =
    "SELECT n.id, n.kind, n.project_id, n.merge_request_iid, n.actor_user_id, \
     (SELECT u.display_name FROM forge.users u WHERE u.id = n.actor_user_id), \
     to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
     to_char(n.read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
     FROM forge.in_app_notifications n WHERE n.user_id = $1 AND n.id < $2 \
     ORDER BY n.id DESC LIMIT $3";
/// A page of the unread ones (the same columns; the partial index).
const UNREAD_PAGE_SQL: &str =
    "SELECT n.id, n.kind, n.project_id, n.merge_request_iid, n.actor_user_id, \
     (SELECT u.display_name FROM forge.users u WHERE u.id = n.actor_user_id), \
     to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
     to_char(n.read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
     FROM forge.in_app_notifications n WHERE n.user_id = $1 AND n.id < $2 \
       AND n.read_at IS NULL \
     ORDER BY n.id DESC LIMIT $3";

const ITEM_COLUMNS: &str = "n.id, n.kind, n.project_id, n.merge_request_iid, n.actor_user_id, \
     (SELECT u.display_name FROM forge.users u WHERE u.id = n.actor_user_id), \
     to_char(n.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
     to_char(n.read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')";

fn parse_item(row: ItemRow) -> Result<Item, ForgeError> {
    let (id, kind, project, iid, actor, actor_name, created_at, read_at) = row;
    Ok(Item {
        id,
        kind: NotificationKind::parse(&kind).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_kind_invalid")
                .with_context("in_app_notification_id", id)
        })?,
        project: ProjectId::new(project),
        iid,
        actor: actor.map(UserId::new),
        actor_name,
        created_at,
        read_at,
    })
}

/// One item to record for the event being handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItem {
    pub user: UserId,
    pub kind: NotificationKind,
    pub project: ProjectId,
    pub iid: i64,
    pub actor: Option<UserId>,
}

/// Record `new` in the subscriber's transaction. `false`: this event
/// already has an item for this person (a repeated step), nothing written.
pub async fn record(
    tx: &mut DerivedTx<'_>,
    event: &SourceEvent,
    new: &NewItem,
) -> Result<bool, ForgeError> {
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.in_app_notifications (user_id, kind, event_id, event_seq, \
           project_id, merge_request_iid, actor_user_id) \
         VALUES ($1, $2, $3::uuid, $4, $5, $6, $7) \
         ON CONFLICT (event_id, user_id) DO NOTHING RETURNING id",
    )
    .bind(new.user.get())
    .bind(new.kind.as_str())
    .bind(event.event_id.to_string())
    .bind(event.seq)
    .bind(new.project.get())
    .bind(new.iid)
    .bind(new.actor.map(UserId::get))
    .fetch_optional(tx.conn())
    .await
    .map_err(|e| query_error("notify.in_app.record", e))?;
    Ok(id.is_some())
}

/// What a page asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageQuery {
    /// Only items with a smaller id (the last id of the previous page).
    pub before: Option<i64>,
    /// Only unread items.
    pub unread_only: bool,
    /// At most this many rows (clamped to 1..=[`MAX_PAGE`]).
    pub limit: i64,
}

/// `user`'s items, newest first, as stored (not yet checked for
/// visibility).
pub async fn page(db: &Db, user: UserId, q: PageQuery) -> Result<Vec<Item>, ForgeError> {
    const QUERY: &str = "notify.in_app.page";
    // Two static statements (not one with `NOT $3 OR …`), so the unread
    // one can use the partial index `in_app_notifications_unread`; the
    // cursor is always bound (`i64::MAX` = from the newest).
    let sql = if q.unread_only {
        UNREAD_PAGE_SQL
    } else {
        ALL_PAGE_SQL
    };
    let rows: Vec<ItemRow> = db
        .within(QUERY, async {
            sqlx::query_as(sql)
                .bind(user.get())
                .bind(q.before.unwrap_or(i64::MAX))
                .bind(q.limit.clamp(1, MAX_PAGE))
                .fetch_all(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter().map(parse_item).collect()
}

/// The merge requests of `keys` (project, iid) that still exist, keyed by
/// them; one statement.
pub async fn merge_requests(
    db: &Db,
    keys: &[(ProjectId, i64)],
) -> Result<HashMap<(ProjectId, i64), StoredMergeRequest>, ForgeError> {
    const QUERY: &str = "notify.in_app.merge_requests";
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let projects: Vec<i64> = keys.iter().map(|(p, _)| p.get()).collect();
    let iids: Vec<i64> = keys.iter().map(|(_, iid)| *iid).collect();
    let rows: Vec<MergeRequestRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM forge.merge_requests \
                 WHERE (project_id, iid) IN \
                   (SELECT * FROM unnest($1::bigint[], $2::bigint[]))"
            ))
            .bind(&projects)
            .bind(&iids)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|row| {
            let mr = row.parse()?;
            Ok(((mr.project_id, mr.iid), mr))
        })
        .collect()
}

/// The owner of item `id` (`None`: no such item), for the route's loader.
pub async fn owner(db: &Db, id: i64) -> Result<Option<UserId>, ForgeError> {
    const QUERY: &str = "notify.in_app.owner";
    let owner: Option<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT user_id FROM forge.in_app_notifications WHERE id = $1")
                .bind(id)
                .fetch_optional(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(owner.map(UserId::new))
}

/// Mark `user`'s item `id` read (kept as it was when it already is).
/// `None`: `user` has no such item.
pub async fn mark_read(db: &Db, user: UserId, id: i64) -> Result<Option<Item>, ForgeError> {
    const QUERY: &str = "notify.in_app.mark_read";
    let row: Option<ItemRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "WITH marked AS (UPDATE forge.in_app_notifications \
                   SET read_at = coalesce(read_at, greatest(now(), created_at)) \
                   WHERE id = $1 AND user_id = $2 RETURNING *) \
                 SELECT {ITEM_COLUMNS} FROM marked n"
            ))
            .bind(id)
            .bind(user.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(parse_item).transpose()
}

/// Mark every unread item of `user` read, up to id `up_to` when given (the
/// newest the person saw: an item that arrived since stays unread).
/// Returns how many were marked.
pub async fn mark_all_read(db: &Db, user: UserId, up_to: Option<i64>) -> Result<u64, ForgeError> {
    const QUERY: &str = "notify.in_app.mark_all_read";
    let done = db
        .within(QUERY, async {
            sqlx::query(
                "UPDATE forge.in_app_notifications \
                 SET read_at = greatest(now(), created_at) \
                 WHERE user_id = $1 AND read_at IS NULL AND ($2::bigint IS NULL OR id <= $2)",
            )
            .bind(user.get())
            .bind(up_to)
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(done.rows_affected())
}

/// Delete up to `batch` items read more than [`READ_RETENTION_DAYS`] days
/// ago or created more than [`MAX_AGE_DAYS`] days ago; returns how many
/// were deleted (derived rows: no audit row).
pub async fn expire(db: &Db, batch: i64) -> Result<u64, ForgeError> {
    const QUERY: &str = "notify.in_app.expire";
    let batch = batch.clamp(1, 10_000);
    // Two statements, each skipping rows another transaction holds (a
    // person marking them read at the same moment): the next run takes them.
    let mut deleted = 0;
    for (sql, days) in [
        (
            "DELETE FROM forge.in_app_notifications WHERE id IN ( \
               SELECT id FROM forge.in_app_notifications \
               WHERE read_at < now() - make_interval(days => $1) \
               ORDER BY read_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
            READ_RETENTION_DAYS,
        ),
        (
            "DELETE FROM forge.in_app_notifications WHERE id IN ( \
               SELECT id FROM forge.in_app_notifications \
               WHERE created_at < now() - make_interval(days => $1) \
               ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
            MAX_AGE_DAYS,
        ),
    ] {
        let done = db
            .within(QUERY, async {
                sqlx::query(sql)
                    .bind(days)
                    .bind(batch)
                    .execute(db.write_pool())
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        deleted += done.rows_affected();
    }
    Ok(deleted)
}
