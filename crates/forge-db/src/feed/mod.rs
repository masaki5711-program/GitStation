//! The event feed of an integration (M3 I1c; plan 02 §5.7, §11.2; ADR-0016
//! §2, §7 and its Amendment I1c): the snapshot the feed reads in, the
//! material of its decision, the page read, the read records, retention and
//! the feed's epoch.
//!
//! - [`open`] starts a `REPEATABLE READ READ ONLY` transaction on the read
//!   pool ([`FeedSnapshot`]): the feed state, the decision's material and
//!   the events are read from one snapshot, so a binding that changes in
//!   between cannot pair an old decision with new rows.
//! - [`FeedSnapshot::material`] reads the integration, its bindings (with
//!   their generation) and the service account's access facts on each bound
//!   project with the namespace resolver's own statement
//!   (`resolve::PROJECT_ACCESS_SQL`; the role is the role loaders' `native`).
//!   Nothing here decides: `forge_auth::prove_visible` does.
//! - [`FeedSnapshot::read_feed_page`] turns only the [`VisibleSet`]'s project ids
//!   and event types into SQL: `seq > after AND seq <= bound`, in `seq`
//!   order, at most `limit` events and about `max_bytes` of payload. Events
//!   without a project and types outside the set never match. Each event
//!   comes with its work items of this integration (`event_work_items`) and
//!   its actor (ids, username, display name — the read pool never sees
//!   e-mail or employee numbers: [`personal_facts`] reads them on the write
//!   pool, only for a token with the scope).
//! - The bound is `min(high_seq, last_seq of work-item-sync)` ([`FeedState`]):
//!   an event is never shown before its work items are derived.
//! - [`record_read`] writes `integration_feed_reads` at most once a minute
//!   per integration (derived; a failure is the caller's to log, the answer
//!   still goes out). [`lag`] reads it for doctor (`FG-INT-0009`).
//! - [`run_retention`] runs one batch of `forge.events_retention`
//!   (`forge_purge`, the worker's `events.retention` task); [`new_epoch`]
//!   runs `forge.feed_new_epoch` on a `forge_maint` connection
//!   ([`Db::connect_maint`]).
//!
//! Only forge-integ calls [`FeedSnapshot::read_feed_page`] (forge-integ
//! `tests/decided_callers.rs`), with a set only forge-auth can build.

use std::collections::BTreeMap;

use forge_core::config::DatabaseConfig;
use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::{FeedBinding, FeedMaterial, VisibleSet};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::{PgConnection, Postgres};
use sqlx::{ConnectOptions as _, Connection as _};

use crate::resolve::{interpret_project, ProjectAccessRow, PROJECT_ACCESS_SQL};
use crate::roles::CAP;
use crate::work_items::WORK_ITEM_SYNC;
use crate::{query_error, read_query_error, Db};

#[cfg(test)]
mod tests;

/// The most events one page holds (plan 02 §11.2).
pub const MAX_LIMIT: usize = 500;
/// `limit` when the request names none.
pub const DEFAULT_LIMIT: usize = 100;
/// The most bytes one answer carries (plan 02 §11.2: 2 MiB).
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
/// What one event costs besides its payload in the page budget (the
/// envelope: id, type, time, actor, cursor; a work item costs
/// [`WORK_ITEM_BYTES`] more).
pub const EVENT_ENVELOPE_BYTES: usize = 1024;
/// The budget of one work item of an event.
pub const WORK_ITEM_BYTES: usize = 160;
/// Rows fetched per statement while a page fills.
const CHUNK: i64 = 100;
/// The most bindings read for one decision (`integrations::MAX_BINDINGS`).
const MAX_BINDINGS: i64 = crate::integrations::MAX_BINDINGS;
/// How often a read record is written per integration (ADR-0016 §7).
pub const READ_RECORD_PERIOD_SECS: i64 = 60;
/// A retention batch (ADR-0016 §2: at most 10 000 rows per call).
pub const RETENTION_BATCH: i32 = 10_000;

/// The snapshot one feed request reads in.
pub struct FeedSnapshot {
    tx: sqlx::Transaction<'static, Postgres>,
    db: Db,
}

impl std::fmt::Debug for FeedSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedSnapshot").finish_non_exhaustive()
    }
}

/// The feed's counters as the snapshot saw them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedState {
    /// The feed's epoch (all zero before the sequencer created the row).
    pub epoch: [u8; 16],
    pub high_seq: i64,
    /// Events below it were deleted by retention.
    pub retained_from: i64,
    /// `last_seq` of `work-item-sync` (0 when it is not registered).
    pub work_items_seq: i64,
    /// `retention_policy.events_days`.
    pub retention_days: i32,
}

impl FeedState {
    /// The read's upper bound: never past an event whose work items are
    /// not derived yet.
    pub fn bound(&self) -> i64 {
        self.high_seq.min(self.work_items_seq).max(0)
    }
}

/// The actor of an event as the feed shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedActor {
    /// `user`, `service_account`, `user_via_service_account` or `system`.
    pub kind: String,
    pub user: Option<FeedUser>,
    /// The service account that acted for `user` (act-as).
    pub via: Option<FeedUser>,
}

/// A user an event names (no personal data: see [`personal_facts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedUser {
    pub id: UserId,
    /// `None`: the user row is gone.
    pub username: Option<String>,
    pub display_name: Option<String>,
}

/// One work item of an event, as the consumer names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedWorkItem {
    /// The integration's kind (`kanflow`).
    pub provider: String,
    /// The external (kanflow) project id.
    pub project_id: String,
    /// `PBI-<n>`.
    pub reference: String,
}

/// One event of a page.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedEvent {
    pub seq: i64,
    /// The event's UUID (`evt_<simple>` outside).
    pub event_id: String,
    pub event_type: String,
    pub schema_version: i16,
    /// UTC, RFC 3339 with milliseconds.
    pub occurred_at: String,
    pub project_id: ProjectId,
    pub actor: FeedActor,
    pub work_items: Vec<FeedWorkItem>,
    /// The stored `data` of its version.
    pub payload: serde_json::Value,
}

/// One page of the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedPage {
    pub events: Vec<FeedEvent>,
    /// Visible events remain after the last one returned (the limit or the
    /// byte budget ended the page).
    pub has_more: bool,
}

/// Open the feed's snapshot on the read pool (`FG-DB-0001` / `FG-DB-0002`).
pub async fn open(db: &Db) -> Result<FeedSnapshot, ForgeError> {
    const QUERY: &str = "feed.open";
    let tx = db
        .within(QUERY, async {
            let mut tx = db
                .pool()
                .executor()
                .begin()
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
                .execute(&mut *tx)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            Ok(tx)
        })
        .await?;
    Ok(FeedSnapshot { tx, db: db.clone() })
}

fn epoch_bytes(raw: &str) -> Result<[u8; 16], ForgeError> {
    uuid::Uuid::parse_str(raw)
        .map(|u| *u.as_bytes())
        .map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "feed_epoch_unreadable")
        })
}

type EventRow = (
    i64,
    String,
    String,
    i16,
    String,
    i64,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    String,
    i32,
);

impl FeedSnapshot {
    fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// The feed's state (`None`: the sequencer has not created it yet).
    pub async fn state(&mut self) -> Result<Option<FeedState>, ForgeError> {
        const QUERY: &str = "feed.state";
        let db = self.db.clone();
        let conn = self.conn();
        let row: Option<(String, i64, i64, Option<i64>, i32)> = db
            .within(QUERY, async {
                sqlx::query_as(
                    "SELECT s.epoch::text, s.high_seq, s.retained_from, \
                       (SELECT sub.last_seq FROM forge.event_subscribers sub WHERE sub.name = $1), \
                       (SELECT p.events_days FROM forge.retention_policy p WHERE p.singleton) \
                     FROM forge.event_feed_state s WHERE s.singleton",
                )
                .bind(WORK_ITEM_SYNC)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
            })
            .await?;
        row.map(|(epoch, high_seq, retained_from, work_items, days)| {
            Ok(FeedState {
                epoch: epoch_bytes(&epoch)?,
                high_seq,
                retained_from,
                work_items_seq: work_items.unwrap_or(0),
                retention_days: days,
            })
        })
        .transpose()
    }

    /// The retention period when there is no feed state yet.
    pub async fn retention_days(&mut self) -> Result<i32, ForgeError> {
        const QUERY: &str = "feed.retention_days";
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            sqlx::query_scalar("SELECT events_days FROM forge.retention_policy WHERE singleton")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await
    }

    /// What `prove_visible` decides on (`None`: no such integration).
    pub async fn material(
        &mut self,
        integration: IntegrationId,
    ) -> Result<Option<FeedMaterial>, ForgeError> {
        const QUERY: &str = "feed.material";
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            let row: Option<(i64, bool, Vec<String>)> = sqlx::query_as(
                "SELECT service_account_id, enabled, event_types FROM forge.integrations \
                 WHERE id = $1",
            )
            .bind(integration.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            let Some((service_account, enabled, event_types)) = row else {
                return Ok(None);
            };
            let bound: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT project_id, generation FROM forge.integration_bindings \
                 WHERE integration_id = $1 ORDER BY project_id LIMIT $2",
            )
            .bind(integration.get())
            .bind(MAX_BINDINGS)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            let viewer = UserId::new(service_account);
            let mut bindings = Vec::with_capacity(bound.len());
            for (project, generation) in bound {
                let project = ProjectId::new(project);
                let row: ProjectAccessRow = sqlx::query_as(PROJECT_ACCESS_SQL)
                    .bind(project.get())
                    .bind(viewer.get())
                    .bind(CAP)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(|e| read_query_error("feed.project_access", e))?;
                let access = interpret_project(row, project, Some(viewer))?.map(|p| p.access);
                bindings.push(FeedBinding {
                    project,
                    generation,
                    access,
                });
            }
            Ok(Some(FeedMaterial {
                integration,
                service_account: viewer,
                enabled,
                event_types,
                bindings,
            }))
        })
        .await
    }

    /// The visible events with `after < seq <= bound`, in `seq` order: at
    /// most `limit` (1 ..= [`MAX_LIMIT`]) and, past the first event, no more
    /// than `max_bytes` by the page budget ([`EVENT_ENVELOPE_BYTES`] +
    /// payload + [`WORK_ITEM_BYTES`] per work item).
    pub async fn read_feed_page(
        &mut self,
        visible: &VisibleSet,
        after: i64,
        bound: i64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<FeedPage, ForgeError> {
        if limit == 0 || limit > MAX_LIMIT {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "feed_limit_out_of_range")
                .with_context("limit", limit));
        }
        if visible.shows_nothing() || after >= bound {
            return Ok(FeedPage {
                events: Vec::new(),
                has_more: false,
            });
        }
        const QUERY: &str = "feed.read_page";
        let projects = visible.project_ids();
        let types: Vec<String> = visible
            .event_type_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let integration = visible.integration().get();
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            let mut events: Vec<FeedEvent> = Vec::new();
            let mut used = 0usize;
            let mut position = after;
            loop {
                let rows: Vec<EventRow> = sqlx::query_as(
                    "SELECT e.seq, e.event_id::text, e.event_type, e.schema_version, \
                       to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                       e.project_id, e.actor_kind, \
                       e.actor_user_id, u.username, u.display_name, \
                       e.actor_via_id, v.username, v.display_name, \
                       e.payload::text, octet_length(e.payload::text) \
                     FROM forge.events e \
                     LEFT JOIN forge.users u ON u.id = e.actor_user_id \
                     LEFT JOIN forge.users v ON v.id = e.actor_via_id \
                     WHERE e.seq > $1 AND e.seq <= $2 \
                       AND e.project_id = ANY($3) AND e.event_type = ANY($4) \
                     ORDER BY e.seq LIMIT $5",
                )
                .bind(position)
                .bind(bound)
                .bind(&projects)
                .bind(&types)
                .bind(CHUNK)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
                let exhausted = i64::try_from(rows.len()).unwrap_or(i64::MAX) < CHUNK;
                let seqs: Vec<i64> = rows.iter().map(|r| r.0).collect();
                let mut items = work_items_of(&mut *conn, integration, &seqs).await?;
                for row in rows {
                    let seq = row.0;
                    let work_items = items.remove(&seq).unwrap_or_default();
                    let cost = EVENT_ENVELOPE_BYTES
                        + usize::try_from(row.14).unwrap_or(usize::MAX)
                        + WORK_ITEM_BYTES * work_items.len();
                    if events.len() >= limit
                        || (!events.is_empty() && used.saturating_add(cost) > max_bytes)
                    {
                        return Ok(FeedPage {
                            events,
                            has_more: true,
                        });
                    }
                    used = used.saturating_add(cost);
                    events.push(event(row, work_items)?);
                    position = seq;
                }
                if exhausted {
                    return Ok(FeedPage {
                        events,
                        has_more: false,
                    });
                }
            }
        })
        .await
    }

    /// When the first visible event after `after` (and `<= bound`)
    /// happened, UTC RFC 3339 (`None`: none is left).
    pub async fn oldest_unread_at(
        &mut self,
        visible: &VisibleSet,
        after: i64,
        bound: i64,
    ) -> Result<Option<String>, ForgeError> {
        if visible.shows_nothing() || after >= bound {
            return Ok(None);
        }
        const QUERY: &str = "feed.oldest_unread";
        let projects = visible.project_ids();
        let types: Vec<String> = visible
            .event_type_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let db = self.db.clone();
        let conn = self.conn();
        db.within(QUERY, async {
            sqlx::query_scalar(
                "SELECT to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') \
                 FROM forge.events e WHERE e.seq > $1 AND e.seq <= $2 \
                   AND e.project_id = ANY($3) AND e.event_type = ANY($4) \
                 ORDER BY e.seq LIMIT 1",
            )
            .bind(after)
            .bind(bound)
            .bind(&projects)
            .bind(&types)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await
    }

    /// End the snapshot (read only: a rollback). A failed goodbye is not a
    /// fault of the request.
    pub async fn close(self) {
        if let Err(e) = self.tx.rollback().await {
            tracing::debug!(target: "forge_db", error = %e, "feed snapshot rollback failed");
        }
    }
}

/// The work items of `seqs` that belong to `integration`, by seq.
async fn work_items_of(
    conn: &mut PgConnection,
    integration: i64,
    seqs: &[i64],
) -> Result<BTreeMap<i64, Vec<FeedWorkItem>>, ForgeError> {
    let mut out: BTreeMap<i64, Vec<FeedWorkItem>> = BTreeMap::new();
    if seqs.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT ew.seq, i.kind, w.external_project_id, w.ref \
         FROM forge.event_work_items ew \
         JOIN forge.work_items w ON w.id = ew.work_item_id \
         JOIN forge.integrations i ON i.id = w.integration_id \
         WHERE ew.seq = ANY($1) AND w.integration_id = $2 \
         ORDER BY ew.seq, w.id",
    )
    .bind(seqs)
    .bind(integration)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("feed.work_items", e))?;
    for (seq, provider, project_id, reference) in rows {
        out.entry(seq).or_default().push(FeedWorkItem {
            provider,
            project_id,
            reference,
        });
    }
    Ok(out)
}

fn user(id: Option<i64>, username: Option<String>, display: Option<String>) -> Option<FeedUser> {
    id.map(|id| FeedUser {
        id: UserId::new(id),
        username,
        display_name: display,
    })
}

fn event(row: EventRow, work_items: Vec<FeedWorkItem>) -> Result<FeedEvent, ForgeError> {
    let (
        seq,
        event_id,
        event_type,
        schema_version,
        occurred_at,
        project_id,
        actor_kind,
        actor,
        actor_name,
        actor_display,
        via,
        via_name,
        via_display,
        payload,
        _,
    ) = row;
    let payload = serde_json::from_str(&payload).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "invalid_event_payload")
            .with_context("seq", seq)
    })?;
    Ok(FeedEvent {
        seq,
        event_id,
        event_type,
        schema_version,
        occurred_at,
        project_id: ProjectId::new(project_id),
        actor: FeedActor {
            kind: actor_kind,
            user: user(actor, actor_name, actor_display),
            via: user(via, via_name, via_display),
        },
        work_items,
        payload,
    })
}

/// The personal facts of `users` (e-mail, employee number) on the write
/// pool — the read pool cannot see those columns (ADR-0018 §2). Called
/// only for a token holding `user_email` / `user_employee_number`.
pub async fn personal_facts(
    db: &Db,
    users: &[UserId],
) -> Result<BTreeMap<UserId, (Option<String>, Option<String>)>, ForgeError> {
    const QUERY: &str = "feed.personal_facts";
    if users.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let rows: Vec<(i64, Option<String>, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as("SELECT id, email, employee_number FROM forge.users WHERE id = ANY($1)")
                .bind(&ids)
                .fetch_all(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, email, employee)| (UserId::new(id), (email, employee)))
        .collect())
}

/// One read of an integration's feed, for the lag check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedRead {
    pub integration: IntegrationId,
    /// The position the read answered (its next cursor).
    pub cursor_seq: i64,
    pub bound: i64,
    /// UTC RFC 3339 of the first visible event after `cursor_seq`.
    pub oldest_unread_at: Option<String>,
}

/// Write the read record unless one was written less than a minute ago
/// (`true`: written). Derived state on the write pool, no audit row.
pub async fn record_read(db: &Db, read: &FeedRead) -> Result<bool, ForgeError> {
    const QUERY: &str = "feed.record_read";
    let written: Option<bool> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "INSERT INTO forge.integration_feed_reads AS r \
                   (integration_id, last_read_at, cursor_seq, read_bound_seq, oldest_unread_at) \
                 VALUES ($1, now(), $2, GREATEST($3, $2), $4::timestamptz) \
                 ON CONFLICT (integration_id) DO UPDATE SET \
                   last_read_at = EXCLUDED.last_read_at, cursor_seq = EXCLUDED.cursor_seq, \
                   read_bound_seq = EXCLUDED.read_bound_seq, \
                   oldest_unread_at = EXCLUDED.oldest_unread_at \
                 WHERE r.last_read_at <= now() - make_interval(secs => $5) \
                 RETURNING true",
            )
            .bind(read.integration.get())
            .bind(read.cursor_seq)
            .bind(read.bound)
            .bind(read.oldest_unread_at.as_deref())
            .bind(READ_RECORD_PERIOD_SECS as f64)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(written.unwrap_or(false))
}

/// Why an integration counts as lagging (`FG-INT-0009 reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LagReason {
    /// No read for 24 hours (or never, 24 hours after the first binding).
    NotReading,
    /// The oldest unread event is older than half the retention period.
    Behind,
}

impl LagReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotReading => "not_reading",
            Self::Behind => "behind",
        }
    }
}

/// One lagging integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lagging {
    pub integration: IntegrationId,
    pub reason: LagReason,
    /// UTC RFC 3339, `None`: never read.
    pub last_read_at: Option<String>,
    /// How old the oldest unread event is (behind only).
    pub behind_seconds: Option<i64>,
}

/// The lag of every enabled integration with a binding (doctor
/// `integrations.feed_lag`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedLag {
    pub integrations: i64,
    pub retention_days: i32,
    pub lagging: Vec<Lagging>,
}

/// How long a reader may stay away (ADR-0016 §11: "N 時間").
pub const NOT_READING_AFTER_HOURS: i64 = 24;

impl FeedLag {
    /// `FG-INT-0009` naming the first lagging integration (and the count).
    pub fn check(&self) -> Result<(), ForgeError> {
        let Some(first) = self.lagging.first() else {
            return Ok(());
        };
        Err(lag_error(first, self.retention_days).with_context("lagging", self.lagging.len()))
    }
}

/// `FG-INT-0009` for one lagging integration.
pub fn lag_error(lagging: &Lagging, retention_days: i32) -> ForgeError {
    let err = ForgeError::new(ErrorCode::FeedConsumerLag)
        .with_context("reason", lagging.reason.as_str())
        .with_context("integration_id", lagging.integration.get())
        .with_context(
            "last_read_at",
            lagging.last_read_at.as_deref().unwrap_or("never"),
        )
        .with_context("retention_days", retention_days);
    match lagging.behind_seconds {
        Some(s) => err.with_context("behind_seconds", s),
        None => err,
    }
}

/// The feed lag (`None`: the schema is not migrated this far).
pub async fn lag(db: &Db) -> Result<Option<FeedLag>, ForgeError> {
    const QUERY: &str = "feed.lag";
    let migrated: bool = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT to_regclass('forge.integration_feed_reads') IS NOT NULL")
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    if !migrated {
        return Ok(None);
    }
    let days: i32 = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT events_days FROM forge.retention_policy WHERE singleton")
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    type LagRow = (i64, Option<String>, bool, Option<i64>, bool);
    let rows: Vec<LagRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT i.id, \
                   to_char(r.last_read_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                   coalesce(r.last_read_at, b.first_bound) < now() - make_interval(hours => $1), \
                   CASE WHEN r.oldest_unread_at IS NOT NULL \
                     THEN extract(epoch FROM now() - r.oldest_unread_at)::bigint END, \
                   coalesce(r.oldest_unread_at < now() - make_interval(days => $2) / 2, false) \
                 FROM forge.integrations i \
                 JOIN (SELECT integration_id, min(created_at) AS first_bound \
                         FROM forge.integration_bindings GROUP BY integration_id) b \
                   ON b.integration_id = i.id \
                 LEFT JOIN forge.integration_feed_reads r ON r.integration_id = i.id \
                 WHERE i.enabled ORDER BY i.id",
            )
            .bind(i32::try_from(NOT_READING_AFTER_HOURS).unwrap_or(i32::MAX))
            .bind(days)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let integrations = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    let lagging = rows
        .into_iter()
        .filter_map(|(id, last, not_reading, behind_seconds, behind)| {
            let reason = if not_reading {
                LagReason::NotReading
            } else if behind {
                LagReason::Behind
            } else {
                return None;
            };
            Some(Lagging {
                integration: IntegrationId::new(id),
                reason,
                last_read_at: last,
                behind_seconds: if reason == LagReason::Behind {
                    behind_seconds
                } else {
                    None
                },
            })
        })
        .collect();
    Ok(Some(FeedLag {
        integrations,
        retention_days: days,
        lagging,
    }))
}

/// The `work-item-sync` subscriber as the feed's bound sees it (doctor
/// `subscribers.work-item-sync`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatermarkHealth {
    /// `None`: not registered (the feed returns nothing yet).
    pub last_seq: Option<i64>,
    pub enabled: bool,
    pub high_seq: i64,
}

/// The feed's watermark (`None`: no feed state or schema yet).
pub async fn watermark(db: &Db) -> Result<Option<WatermarkHealth>, ForgeError> {
    const QUERY: &str = "feed.watermark";
    let row: Option<(i64, Option<i64>, Option<bool>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT s.high_seq, sub.last_seq, sub.enabled FROM forge.event_feed_state s \
                 LEFT JOIN forge.event_subscribers sub ON sub.name = $1 WHERE s.singleton",
            )
            .bind(WORK_ITEM_SYNC)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|(high_seq, last_seq, enabled)| WatermarkHealth {
        last_seq,
        enabled: enabled.unwrap_or(false),
        high_seq,
    }))
}

/// What one retention batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionBatch {
    pub deleted: i64,
    /// The seq range removed (`None`: nothing was due).
    pub range: Option<(i64, i64)>,
}

/// One batch of `forge.events_retention` (at most `max_rows` seqs).
pub async fn run_retention(
    db: &Db,
    request_id: &forge_core::RequestId,
    max_rows: i32,
) -> Result<RetentionBatch, ForgeError> {
    const QUERY: &str = "events.retention";
    let (deleted, from, to): (Option<i64>, Option<i64>, Option<i64>) = db
        .within(QUERY, async {
            sqlx::query_as("SELECT deleted, from_seq, to_seq FROM forge.events_retention($1, $2)")
                .bind(max_rows)
                .bind(request_id.as_str())
                .fetch_one(db.write_pool())
                .await
                .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(RetentionBatch {
        deleted: deleted.unwrap_or(0),
        range: from.zip(to),
    })
}

/// The retention state (doctor `events.retention`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionHealth {
    pub events_days: i32,
    pub retained_from: i64,
    /// How old the oldest retained event is, in seconds (`None`: no event).
    pub oldest_age_seconds: Option<i64>,
}

impl RetentionHealth {
    /// Retention keeps up: nothing older than 1.5 × the period (plus a
    /// day of slack for the task's period) is left.
    pub fn keeps_up(&self) -> bool {
        let limit = i64::from(self.events_days) * 86_400 * 3 / 2 + 86_400;
        self.oldest_age_seconds.is_none_or(|age| age <= limit)
    }
}

/// The retention state (`None`: not migrated this far or no feed state).
pub async fn retention(db: &Db) -> Result<Option<RetentionHealth>, ForgeError> {
    const QUERY: &str = "feed.retention_health";
    let migrated: bool = db
        .within(QUERY, async {
            sqlx::query_scalar("SELECT to_regclass('forge.retention_policy') IS NOT NULL")
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    if !migrated {
        return Ok(None);
    }
    let row: Option<(i32, i64, Option<i64>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.events_days, s.retained_from, \
                   (SELECT extract(epoch FROM now() - e.occurred_at)::bigint \
                      FROM forge.events e WHERE e.seq >= s.retained_from AND e.seq IS NOT NULL \
                      ORDER BY e.seq LIMIT 1) \
                 FROM forge.retention_policy p, forge.event_feed_state s \
                 WHERE p.singleton AND s.singleton",
            )
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(
        |(events_days, retained_from, oldest_age_seconds)| RetentionHealth {
            events_days,
            retained_from,
            oldest_age_seconds,
        },
    ))
}

/// A maintenance connection (`forge_maint`, `database.maint_url`): one
/// connection for an approved runbook step (`forge admin events new-epoch`).
#[derive(Debug)]
pub struct MaintConn {
    conn: PgConnection,
}

impl MaintConn {
    /// Close the connection, bounded; a failed goodbye is not a fault.
    pub async fn close(self) {
        let close = self.conn.close();
        match tokio::time::timeout(std::time::Duration::from_secs(5), close).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!(target: "forge_db", error = %e, "maint close failed"),
            Err(_) => tracing::debug!(target: "forge_db", "maint close timed out"),
        }
    }
}

impl Db {
    /// Open the maintenance connection as `forge_maint`. A missing
    /// `database.maint_url` is `FG-SYS-0002`; connection failures are
    /// `FG-DB-0001`.
    pub async fn connect_maint(cfg: &DatabaseConfig) -> Result<MaintConn, ForgeError> {
        let url = cfg.maint_url()?;
        let target = crate::options::preflight(cfg, url)?;
        let password = cfg.maint_password()?;
        let options = crate::options::connect_options(
            cfg,
            &crate::options::Login {
                url,
                password: password.as_ref(),
                application_name: "forge-admin",
                settings: &[],
            },
        );
        let open = async {
            options
                .connect()
                .await
                .map_err(|e| crate::error_map::connect_error(&target, e))
        };
        let conn = crate::pool::bounded(cfg, &target, open).await?;
        Ok(MaintConn { conn })
    }
}

/// The epochs before and after [`new_epoch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewEpoch {
    pub old: [u8; 16],
    pub new: [u8; 16],
}

/// `FG-DB-0014` FeedEpochChanged (warn) for a rotation: what `forge admin
/// events new-epoch` logs.
pub fn epoch_changed(rotated: &NewEpoch) -> ForgeError {
    let show = |b: &[u8; 16]| uuid::Uuid::from_bytes(*b).to_string();
    ForgeError::new(ErrorCode::FeedEpochChanged)
        .with_context("reason", "manual")
        .with_context("old_epoch", show(&rotated.old))
        .with_context("new_epoch", show(&rotated.new))
}

/// `forge.feed_new_epoch('manual', request_id)` on the maintenance
/// connection: every integration's cursor answers 410 epoch_mismatch
/// afterwards. No feed state yet is `FG-DB-0002 reason=no_feed_state`.
pub async fn new_epoch(
    maint: &mut MaintConn,
    request_id: &forge_core::RequestId,
) -> Result<NewEpoch, ForgeError> {
    const QUERY: &str = "feed.new_epoch";
    let fut = sqlx::query_as::<_, (String, String)>(
        "SELECT old_epoch::text, new_epoch::text FROM forge.feed_new_epoch('manual', $1)",
    )
    .bind(request_id.as_str())
    .fetch_one(&mut maint.conn);
    let (old, new) = match tokio::time::timeout(std::time::Duration::from_secs(30), fut).await {
        Ok(Ok(row)) => row,
        Ok(Err(e)) => {
            let no_state = e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|c| c == "P0002");
            let err = query_error(QUERY, e);
            return Err(if no_state {
                err.with_context("reason", "no_feed_state")
            } else {
                err
            });
        }
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::DbUnavailable)
                .with_context("reason", "operation_timeout")
                .with_context("operation", QUERY))
        }
    };
    Ok(NewEpoch {
        old: epoch_bytes(&old)?,
        new: epoch_bytes(&new)?,
    })
}
