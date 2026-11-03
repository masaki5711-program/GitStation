//! Reads of integrations and bindings on the read pool.

use forge_core::ids::{IntegrationId, ProjectId, UserId};
use forge_core::IntegrationKind;
use forge_errors::{ErrorCode, ForgeError};

use super::{Binding, Integration, Listed, MAX_BINDINGS, MAX_LISTED};
use crate::{read_query_error, Db};

/// The columns of [`get`] and [`list`].
const SELECT: &str = concat!(
    "SELECT i.id, i.kind, i.service_account_id, u.username, i.enabled, i.event_types, ",
    "(SELECT count(*) FROM forge.integration_bindings b WHERE b.integration_id = i.id), ",
    "i.created_by, ",
    "to_char(i.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), ",
    "to_char(i.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') ",
    "FROM forge.integrations i JOIN forge.users u ON u.id = i.service_account_id "
);

type Row = (
    i64,
    String,
    i64,
    String,
    bool,
    Vec<String>,
    i64,
    Option<i64>,
    String,
    String,
);

/// A stored kind this build does not know: `FG-SYS-0001` (the CHECK
/// `integrations_kind_known` lists the same names).
fn kind_of(id: i64, raw: &str) -> Result<IntegrationKind, ForgeError> {
    IntegrationKind::parse(raw).ok_or_else(|| {
        let raw: String = raw.chars().take(32).collect();
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_integration_kind")
            .with_context("integration_id", id)
            .with_context("raw", raw)
    })
}

fn integration(row: Row) -> Result<Integration, ForgeError> {
    let (
        id,
        kind,
        service_account,
        username,
        enabled,
        mut event_types,
        bindings,
        created_by,
        created_at,
        updated_at,
    ) = row;
    event_types.sort();
    Ok(Integration {
        id: IntegrationId::new(id),
        kind: kind_of(id, &kind)?,
        service_account: UserId::new(service_account),
        service_account_username: username,
        enabled,
        event_types,
        bindings,
        created_by: created_by.map(UserId::new),
        created_at,
        updated_at,
    })
}

/// Integration `id` (`None`: absent).
pub async fn get(db: &Db, id: IntegrationId) -> Result<Option<Integration>, ForgeError> {
    const QUERY: &str = "integrations.get";
    let row: Option<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} WHERE i.id = $1"))
                .bind(id.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(integration).transpose()
}

/// Every integration by id (at most [`MAX_LISTED`]; `truncated` says
/// there are more).
pub async fn list(db: &Db) -> Result<Listed, ForgeError> {
    const QUERY: &str = "integrations.list";
    let rows: Vec<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} ORDER BY i.id LIMIT $1"))
                .bind(MAX_LISTED + 1)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_LISTED;
    let mut items = rows
        .into_iter()
        .map(integration)
        .collect::<Result<Vec<_>, _>>()?;
    items.truncate(usize::try_from(MAX_LISTED).unwrap_or(usize::MAX));
    Ok(Listed { items, truncated })
}

/// The columns of [`bindings`] and [`project_bindings`].
const BINDING_SELECT: &str = concat!(
    "SELECT b.integration_id, i.kind, b.project_id, b.external_project_id, ",
    "b.granted_membership, b.created_by, ",
    "to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') ",
    "FROM forge.integration_bindings b JOIN forge.integrations i ON i.id = b.integration_id "
);

type BindingRow = (i64, String, i64, String, bool, Option<i64>, String);

fn binding(row: BindingRow) -> Result<Binding, ForgeError> {
    let (integration, kind, project, external_project_id, granted, created_by, created_at) = row;
    Ok(Binding {
        integration: IntegrationId::new(integration),
        kind: kind_of(integration, &kind)?,
        project: ProjectId::new(project),
        external_project_id,
        granted_membership: granted,
        created_by: created_by.map(UserId::new),
        created_at,
    })
}

async fn binding_rows(
    db: &Db,
    query: &'static str,
    filter: &str,
    id: i64,
) -> Result<Vec<Binding>, ForgeError> {
    let sql = format!("{BINDING_SELECT} WHERE {filter} = $1 ORDER BY b.project_id LIMIT $2");
    let rows: Vec<BindingRow> = db
        .within(query, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&sql)
                .bind(id)
                .bind(MAX_BINDINGS)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(query, e))
        })
        .await?;
    rows.into_iter().map(binding).collect()
}

/// The projects bound to `integration` (at most [`MAX_BINDINGS`]), by
/// project id.
pub async fn bindings(db: &Db, integration: IntegrationId) -> Result<Vec<Binding>, ForgeError> {
    binding_rows(
        db,
        "integrations.bindings",
        "b.integration_id",
        integration.get(),
    )
    .await
}

/// The binding of `project` (none or one).
pub async fn project_bindings(db: &Db, project: ProjectId) -> Result<Vec<Binding>, ForgeError> {
    binding_rows(
        db,
        "integrations.project_bindings",
        "b.project_id",
        project.get(),
    )
    .await
}
