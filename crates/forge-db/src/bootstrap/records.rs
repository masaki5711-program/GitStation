//! Guards of the migration records (ADR-0018 §3 and its amendment): every
//! db.migrate audit row belongs to a shipped, applied migration with the
//! guarded_changes its file declares, and the DDL log holds no change that
//! neither a bootstrap run nor a migration explains.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::guard_missing;
use super::schema::schema_applied;
use crate::read_query_error;

/// Unacknowledged DDL that neither a bootstrap run nor a migration explains
/// (`forge_audit.ddl_audit_status()`, owned by forge_audit). Acknowledge a
/// reviewed row by inserting it into forge_audit.ddl_audit_ack (forge_maint).
pub(super) async fn check_unexpected_ddl(conn: &mut PgConnection) -> Result<(), ForgeError> {
    // The function reads forge.audit_log, which the first migration creates.
    if !schema_applied(conn).await? {
        return Ok(());
    }
    let (count, first): (i64, Option<i64>) = sqlx::query_as(
        "SELECT count(*), min(id) FROM forge_audit.ddl_audit_status() \
         WHERE NOT expected AND NOT acknowledged",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.unexpected_ddl", e))?;
    match first {
        Some(first) if count > 0 => Err(forge_errors::ForgeError::new(
            forge_errors::ErrorCode::AppendOnlyViolation,
        )
        .with_context("reason", "unexpected_ddl")
        .with_context("count", count)
        .with_context("first_id", first)),
        Some(_) | None => Ok(()),
    }
}

/// The `guarded_changes` array a migration file declares in its own
/// db.migrate row (the last statement; the root gate requires it).
fn declared_changes(sql: &str) -> Option<&str> {
    let key = "\"guarded_changes\":";
    let start = sql.rfind(key)? + key.len();
    let rest = sql.get(start..)?;
    let open = rest.find('[')?;
    let close = rest.find(']')?;
    rest.get(open..=close)
}

/// Every db.migrate row belongs to a migration built into this binary and
/// recorded as applied, one row per version, with the guarded_changes its
/// file declares. A row written outside a migration file (the migration
/// login can insert into audit_log) would otherwise make any DDL in its
/// transaction look expected.
pub(super) async fn check_migration_records(conn: &mut PgConnection) -> Result<(), ForgeError> {
    if !schema_applied(conn).await? {
        return Ok(());
    }
    let shipped: Vec<(String, &str)> = crate::MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .filter_map(|m| Some((m.version.to_string(), declared_changes(m.sql.as_ref())?)))
        .collect();
    let versions: Vec<&str> = shipped.iter().map(|(v, _)| v.as_str()).collect();
    let stray: Option<String> = sqlx::query_scalar(
        "SELECT coalesce(details ->> 'version', 'none') FROM ONLY forge.audit_log \
         WHERE action = 'db.migrate' \
           AND NOT coalesce(details ->> 'version' = ANY($1), false) LIMIT 1",
    )
    .bind(&versions)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.migration_records", e))?;
    if let Some(version) = stray {
        return Err(guard_missing("migration_records").with_context("version", version));
    }
    for (version, changes) in &shipped {
        let (applied, rows, matching): (bool, i64, i64) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM forge._sqlx_migrations \
                            WHERE version = $1::bigint AND success), \
                    count(a.txid), \
                    count(a.txid) FILTER (WHERE a.details -> 'guarded_changes' = $2::jsonb) \
             FROM ONLY forge.audit_log AS a \
             WHERE a.action = 'db.migrate' AND a.details ->> 'version' = $1",
        )
        .bind(version)
        .bind(*changes)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.migration_records", e))?;
        let expected_rows = i64::from(applied);
        if rows != expected_rows || matching != expected_rows {
            return Err(guard_missing("migration_records").with_context("version", version));
        }
    }
    Ok(())
}
