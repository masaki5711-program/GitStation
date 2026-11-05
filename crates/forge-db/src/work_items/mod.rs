//! Work items and their links (M3 I1b, plan 02 §2 / §5.7 / §5.8 / §8.2,
//! ADR-0016 Amendment I1b; migration `20261116100000`). Three derived
//! tables:
//!
//! - `forge.work_items`: one kanflow work item per (integration, external
//!   project, `PBI-<n>`); never updated or deleted.
//! - `forge.merge_request_work_items`: intervals of a merge request <-> work
//!   item link (`source` auto / manual). Ending one sets `unlinked_at` once;
//!   rows are never deleted but by the project purge (the guard
//!   `merge_request_work_items_guard`). At most one open interval per pair.
//! - `forge.event_work_items`: the work items each event concerns (the
//!   feed's filter, I1c).
//!
//! The ordered subscriber [`WorkItemSync`] (`work-item-sync`, `sync.rs`)
//! writes them from the outbox; a person's link and unlink are the review
//! commands `link_work_item_mr_decided` / `unlink_work_item_mr_decided`
//! (forge-db `review::work_item_links`, called by forge-review only), which
//! write the link row with a `work_item.*` audit row and event.

use forge_core::ids::{IntegrationId, MergeRequestId, ProjectId, UserId};
use forge_core::{IntegrationKind, LinkActorKind, LinkSource, UnlinkSource, WorkItem, WorkItemRef};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db};

mod sync;

pub use sync::{WorkItemSync, WORK_ITEM_SYNC};

/// The most work items one merge request is linked to automatically (its
/// title and source branch together): more references are logged at warn
/// (`forge.work_items.detected_capped`) and not linked. Bounded so one
/// event never derives more than `subscribers::MAX_DERIVED_PER_ROOT`
/// events (links + unlinks, even across a rebinding).
pub const MAX_AUTO_LINKS: usize = 20;
/// The most open manual links of one merge request (`FG-INT-0019`).
pub const MAX_MANUAL_LINKS: i64 = 50;
/// The most work items one event is recorded with
/// (`forge.work_items.event_items_capped` at warn beyond).
pub const MAX_EVENT_WORK_ITEMS: usize = 100;

/// The binding of a project, as the work items need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundTo {
    pub integration: IntegrationId,
    pub kind: IntegrationKind,
    pub external_project_id: String,
}

impl BoundTo {
    /// Whether `item` is a work item of this binding.
    pub(crate) fn owns(&self, item: &WorkItem) -> bool {
        item.integration == self.integration && item.external_project_id == self.external_project_id
    }
}

fn stored_invalid(field: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "stored_work_item_invalid")
        .with_context("field", field)
        .with_context("id", id)
}

/// The project's binding (`None`: not bound; work items are not detected).
pub(crate) async fn binding_of(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Option<BoundTo>, ForgeError> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT b.integration_id, i.kind, b.external_project_id \
         FROM forge.integration_bindings AS b \
         JOIN forge.integrations AS i ON i.id = b.integration_id \
         WHERE b.project_id = $1",
    )
    .bind(project.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("work_items.binding", e))?;
    row.map(|(integration, kind, external_project_id)| {
        Ok(BoundTo {
            integration: IntegrationId::new(integration),
            kind: IntegrationKind::parse(&kind)
                .ok_or_else(|| stored_invalid("integration_kind", integration))?,
            external_project_id,
        })
    })
    .transpose()
}

/// The work item `reference` of `bound`, created if new.
pub(crate) async fn work_item(
    conn: &mut PgConnection,
    bound: &BoundTo,
    reference: &WorkItemRef,
) -> Result<WorkItem, ForgeError> {
    // forge_app has no UPDATE here: DO NOTHING, then read an existing row
    // (a concurrent insert of the same item is waited for and then seen).
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.work_items (integration_id, external_project_id, ref) \
         VALUES ($1, $2, $3) ON CONFLICT (integration_id, external_project_id, ref) \
         DO NOTHING RETURNING id",
    )
    .bind(bound.integration.get())
    .bind(&bound.external_project_id)
    .bind(reference.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("work_items.insert", e))?;
    let id = match inserted {
        Some(id) => id,
        None => sqlx::query_scalar(
            "SELECT id FROM forge.work_items \
             WHERE integration_id = $1 AND external_project_id = $2 AND ref = $3",
        )
        .bind(bound.integration.get())
        .bind(&bound.external_project_id)
        .bind(reference.as_str())
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error("work_items.select", e))?,
    };
    Ok(WorkItem {
        id,
        integration: bound.integration,
        kind: bound.kind,
        external_project_id: bound.external_project_id.clone(), // clone: the item owns its id
        reference: reference.clone(),
    })
}

/// One open link of a merge request, with its work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpenLink {
    pub link: i64,
    pub source: LinkSource,
    pub work_item: WorkItem,
}

type LinkRow = (i64, String, i64, i64, String, String, String);

const OPEN_LINKS: &str = "SELECT l.id, l.source, w.id, w.integration_id, i.kind, \
     w.external_project_id, w.ref \
     FROM forge.merge_request_work_items AS l \
     JOIN forge.work_items AS w ON w.id = l.work_item_id \
     JOIN forge.integrations AS i ON i.id = w.integration_id \
     WHERE l.merge_request_id = $1 AND l.unlinked_at IS NULL ORDER BY l.id";

fn open_link(row: LinkRow) -> Result<OpenLink, ForgeError> {
    let (link, source, id, integration, kind, external_project_id, reference) = row;
    Ok(OpenLink {
        link,
        source: LinkSource::parse(&source).ok_or_else(|| stored_invalid("source", link))?,
        work_item: WorkItem {
            id,
            integration: IntegrationId::new(integration),
            kind: IntegrationKind::parse(&kind).ok_or_else(|| stored_invalid("kind", id))?,
            external_project_id,
            reference: WorkItemRef::parse(&reference).ok_or_else(|| stored_invalid("ref", id))?,
        },
    })
}

/// The open links of `merge_request` (the caller holds its row lock).
pub(crate) async fn open_links(
    conn: &mut PgConnection,
    merge_request: MergeRequestId,
) -> Result<Vec<OpenLink>, ForgeError> {
    let rows: Vec<LinkRow> = sqlx::query_as(OPEN_LINKS)
        .bind(merge_request.get())
        .fetch_all(conn)
        .await
        .map_err(|e| query_error("work_items.open_links", e))?;
    rows.into_iter().map(open_link).collect()
}

/// Record that the event `seq` concerns `work_items` (at most
/// [`MAX_EVENT_WORK_ITEMS`]; the caller logs the overflow). A row already
/// there is kept.
pub(crate) async fn record_event_items(
    conn: &mut PgConnection,
    seq: i64,
    work_items: &[i64],
) -> Result<(), ForgeError> {
    if work_items.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO forge.event_work_items (seq, work_item_id) \
         SELECT $1, w FROM pg_catalog.unnest($2::bigint[]) AS w \
         ON CONFLICT (seq, work_item_id) DO NOTHING",
    )
    .bind(seq)
    .bind(work_items)
    .execute(conn)
    .await
    .map_err(|e| query_error("work_items.event_items", e))?;
    Ok(())
}

/// One link (open or ended) of a merge request, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLink {
    pub id: i64,
    pub work_item: WorkItem,
    pub source: LinkSource,
    /// Who linked it by hand and their kind (`None` for a detected link).
    pub linked_by: Option<(UserId, LinkActorKind)>,
    pub linked_at: String,
    /// `None` while the link is open.
    pub unlinked_at: Option<String>,
    pub unlink_source: Option<UnlinkSource>,
    /// Who ended it and their kind (`None` while open or for detection).
    pub unlinked_by: Option<(UserId, LinkActorKind)>,
}

/// The most link rows [`links`] returns (newest first beyond it are cut;
/// `truncated`).
pub const MAX_LISTED_LINKS: i64 = 500;

/// A merge request's links, open and ended (history), oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Links {
    pub links: Vec<StoredLink>,
    /// More than [`MAX_LISTED_LINKS`] rows exist.
    pub truncated: bool,
}

type StoredRow = (
    i64,
    String,
    Option<i64>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
    (i64, i64, String, String, String),
);

/// The links of `merge_request` on the read pool (at most
/// [`MAX_LISTED_LINKS`], oldest first).
pub async fn links(db: &Db, merge_request: MergeRequestId) -> Result<Links, ForgeError> {
    const QUERY: &str = "work_items.links";
    let rows: Vec<StoredRow> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT l.id, l.source, l.linked_by, l.linked_by_kind, \
                 to_char(l.linked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 to_char(l.unlinked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 l.unlink_source, l.unlinked_by, l.unlinked_by_kind, \
                 ROW(w.id, w.integration_id, i.kind, w.external_project_id, w.ref) \
                 FROM forge.merge_request_work_items AS l \
                 JOIN forge.work_items AS w ON w.id = l.work_item_id \
                 JOIN forge.integrations AS i ON i.id = w.integration_id \
                 WHERE l.merge_request_id = $1 ORDER BY l.id LIMIT $2",
            )
            .bind(merge_request.get())
            .bind(MAX_LISTED_LINKS + 1)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_LISTED_LINKS;
    let links = rows
        .into_iter()
        .take(usize::try_from(MAX_LISTED_LINKS).unwrap_or(usize::MAX))
        .map(stored_link)
        .collect::<Result<_, _>>()?;
    Ok(Links { links, truncated })
}

fn stored_link(row: StoredRow) -> Result<StoredLink, ForgeError> {
    let (
        id,
        source,
        linked_by,
        linked_by_kind,
        linked_at,
        unlinked_at,
        unlink_source,
        unlinked_by,
        unlinked_by_kind,
        (item, integration, kind, external_project_id, reference),
    ) = row;
    let unlink_source = unlink_source
        .map(|s| UnlinkSource::parse(&s).ok_or_else(|| stored_invalid("unlink_source", id)))
        .transpose()?;
    let actor = |user: Option<i64>, kind: Option<String>, field: &'static str| match (user, kind) {
        (None, None) => Ok(None),
        (Some(user), Some(kind)) => LinkActorKind::parse(&kind)
            .map(|k| Some((UserId::new(user), k)))
            .ok_or_else(|| stored_invalid(field, id)),
        _ => Err(stored_invalid(field, id)),
    };
    let linked_by = actor(linked_by, linked_by_kind, "linked_by_kind")?;
    let unlinked_by = actor(unlinked_by, unlinked_by_kind, "unlinked_by_kind")?;
    Ok(StoredLink {
        id,
        work_item: WorkItem {
            id: item,
            integration: IntegrationId::new(integration),
            kind: IntegrationKind::parse(&kind).ok_or_else(|| stored_invalid("kind", item))?,
            external_project_id,
            reference: WorkItemRef::parse(&reference).ok_or_else(|| stored_invalid("ref", item))?,
        },
        source: LinkSource::parse(&source).ok_or_else(|| stored_invalid("source", id))?,
        linked_by,
        linked_at,
        unlinked_at,
        unlink_source,
        unlinked_by,
    })
}

/// The work items of event `seq` (oldest first; tests and the feed, I1c).
pub async fn event_work_items(db: &Db, seq: i64) -> Result<Vec<i64>, ForgeError> {
    const QUERY: &str = "work_items.event_items_read";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        sqlx::query_scalar(
            "SELECT work_item_id FROM forge.event_work_items WHERE seq = $1 \
             ORDER BY work_item_id LIMIT $2",
        )
        .bind(seq)
        .bind(i64::try_from(MAX_EVENT_WORK_ITEMS).unwrap_or(i64::MAX))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

#[cfg(test)]
mod tests;

/// Whether merge request `$1` has an open link to a work item of its
/// project's **current** binding (M3 I1d: the merge rule's
/// `require_work_item_link`, ADR-0024 §7 / §11). A link to a work item of
/// another integration or another external project — none survives an
/// unbind (I1b), but the join is the condition itself — never counts; a
/// project without a binding never has one.
pub(crate) const LINKED_SQL: &str = "SELECT EXISTS (SELECT 1 \
     FROM forge.merge_request_work_items AS l \
     JOIN forge.work_items AS w ON w.id = l.work_item_id \
     JOIN forge.integration_bindings AS b ON b.project_id = l.project_id \
       AND b.integration_id = w.integration_id \
       AND b.external_project_id = w.external_project_id \
     WHERE l.merge_request_id = $1 AND l.unlinked_at IS NULL)";

/// [`LINKED_SQL`] inside the caller's transaction (the merge gate: the
/// request and every gate of the job).
pub(crate) async fn linked_on(
    conn: &mut PgConnection,
    merge_request: MergeRequestId,
) -> Result<bool, ForgeError> {
    sqlx::query_scalar(LINKED_SQL)
        .bind(merge_request.get())
        .fetch_one(conn)
        .await
        .map_err(|e| query_error("work_items.linked", e))
}

/// [`LINKED_SQL`] on the read pool (the merge status).
pub async fn work_item_linked(db: &Db, merge_request: MergeRequestId) -> Result<bool, ForgeError> {
    const QUERY: &str = "work_items.linked_read";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        sqlx::query_scalar(LINKED_SQL)
            .bind(merge_request.get())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
