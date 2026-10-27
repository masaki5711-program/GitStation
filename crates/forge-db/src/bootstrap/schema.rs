//! Guards of the migrated schema (ADR-0016 §1, ADR-0018 §3): the append-only
//! triggers of events and audit_log and the exact privileges of the tables
//! the first migration creates. Before that migration the tables do not
//! exist; the schema check reports the pending migration instead.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::acl::{compare, table_grants_sql};
use super::guard_missing;
use crate::read_query_error;

/// `pg_trigger.tgtype` bits: ROW 1, BEFORE 2, INSERT 4, DELETE 8, UPDATE
/// 16, TRUNCATE 32.
const GUARD_ROW_TYPE: i16 = 1 | 2 | 4 | 8 | 16;
const GUARD_TRUNCATE_TYPE: i16 = 2 | 32;

/// Guarded tables and their guard function.
const GUARDED: &[(&str, &str)] = &[
    ("events", "forge.events_guard()"),
    ("audit_log", "forge.audit_log_guard()"),
];

const EVENT_INSERT: &[&str] = &[
    "event_id",
    "event_type",
    "schema_version",
    "aggregate_type",
    "aggregate_id",
    "project_id",
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "request_id",
    "caused_by_seq",
    "root_seq",
    "causation_depth",
    "dedup_key",
    "payload",
];

const AUDIT_INSERT: &[&str] = &[
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "credential_id",
    "act_as_claimed",
    "act_as_key_id",
    "via_assertion_id",
    "effective_scope",
    "actor_label",
    "action",
    "outcome",
    "resource_type",
    "resource_id",
    "project_id",
    "source_ip",
    "user_agent",
    "request_id",
    "error_code",
    "occurrences",
    "details",
];

/// The read pool's columns of audit_log (tables.toml `ro_columns`).
const AUDIT_RO: &[&str] = &[
    "id",
    "seq",
    "sequenced_at",
    "txid",
    "occurred_at",
    "actor_kind",
    "actor_user_id",
    "actor_via_id",
    "credential_id",
    "via_assertion_id",
    "effective_scope",
    "action",
    "outcome",
    "resource_type",
    "resource_id",
    "project_id",
    "request_id",
    "error_code",
    "occurrences",
    "details",
];

fn columns(role: &str, privilege: &str, cols: &[&str]) -> Vec<String> {
    cols.iter()
        .map(|c| format!("{role}:{privilege}({c})"))
        .collect()
}

fn plain(grants: &[&str]) -> Vec<String> {
    grants.iter().map(|g| (*g).to_owned()).collect()
}

/// The grants of the first migration, per table.
fn expectations() -> Vec<(&'static str, Vec<String>)> {
    let dml = |role: &str| {
        ["SELECT", "INSERT", "UPDATE", "DELETE"]
            .iter()
            .map(|p| format!("{role}:{p}"))
            .collect::<Vec<_>>()
    };
    let operational = || [dml("forge_app"), plain(&["forge_ro:SELECT"])].concat();
    vec![
        (
            "forge.events",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_purge:SELECT",
                    "forge_purge:DELETE",
                    "forge_ro:SELECT",
                ]),
                columns("forge_app", "INSERT", EVENT_INSERT),
                columns("forge_seq", "UPDATE", &["seq", "sequenced_at"]),
                columns("forge_purge", "UPDATE", &["payload"]),
            ]
            .concat(),
        ),
        (
            "forge.audit_log",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_purge:SELECT",
                    "forge_purge:DELETE",
                ]),
                columns("forge_app", "INSERT", AUDIT_INSERT),
                columns("forge_seq", "INSERT", AUDIT_INSERT),
                columns("forge_purge", "INSERT", AUDIT_INSERT),
                columns("forge_seq", "UPDATE", &["seq", "sequenced_at"]),
                columns(
                    "forge_purge",
                    "UPDATE",
                    &["actor_label", "source_ip", "user_agent"],
                ),
                columns("forge_ro", "SELECT", AUDIT_RO),
            ]
            .concat(),
        ),
        (
            "forge.event_feed_state",
            [
                plain(&[
                    "forge_app:SELECT",
                    "forge_seq:SELECT",
                    "forge_seq:INSERT",
                    "forge_seq:UPDATE",
                    "forge_purge:SELECT",
                    "forge_ro:SELECT",
                ]),
                columns("forge_purge", "UPDATE", &["retained_from"]),
            ]
            .concat(),
        ),
        (
            "forge.event_subscribers",
            [operational(), plain(&["forge_purge:SELECT"])].concat(),
        ),
        ("forge.subscriber_dead_letters", operational()),
        ("forge.jobs", operational()),
    ]
}

pub(super) async fn check_schema_objects(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let migrated: bool = sqlx::query_scalar("SELECT to_regclass('forge.events') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.schema_migrated", e))?;
    if !migrated {
        return Ok(());
    }
    for (table, function) in GUARDED {
        let rows: Vec<(String, String, i16, bool)> = sqlx::query_as(&format!(
            "SELECT t.tgname::text, t.tgenabled::text, t.tgtype, \
             (t.tgfoid = '{function}'::regprocedure AND t.tgnargs = 0 AND t.tgqual IS NULL \
              AND coalesce(pg_catalog.array_length(t.tgattr::int2[], 1), 0) = 0) \
             FROM pg_catalog.pg_trigger t \
             WHERE t.tgrelid = 'forge.{table}'::regclass AND NOT t.tgisinternal"
        ))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.schema_triggers", e))?;
        let expected = [
            (format!("{table}_append_only"), GUARD_ROW_TYPE),
            (format!("{table}_no_truncate"), GUARD_TRUNCATE_TYPE),
        ];
        for (name, kind) in &expected {
            let ok = rows.iter().any(|(n, enabled, tgtype, shape)| {
                n == name && enabled == "A" && tgtype == kind && *shape
            });
            if !ok {
                return Err(guard_missing("log_triggers").with_context("trigger", name));
            }
        }
        if let Some((extra, ..)) = rows
            .iter()
            .find(|(n, ..)| !expected.iter().any(|(name, _)| name == n))
        {
            return Err(guard_missing("log_triggers").with_context("trigger", extra));
        }
    }
    for (table, expected) in expectations() {
        compare(conn, table, &table_grants_sql(table), &expected).await?;
    }
    Ok(())
}

/// Unacknowledged DDL that neither a bootstrap run nor a migration explains
/// (`forge.ddl_audit_status`, second migration). Acknowledge a reviewed row
/// with `forge admin ddl-audit ack` (forge_maint inserts into
/// forge_audit.ddl_audit_ack).
pub(super) async fn check_unexpected_ddl(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let has_view: bool =
        sqlx::query_scalar("SELECT to_regclass('forge.ddl_audit_status') IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("bootstrap.ddl_status_exists", e))?;
    if !has_view {
        return Ok(());
    }
    let (count, first): (i64, Option<i64>) = sqlx::query_as(
        "SELECT count(*), min(id) FROM forge.ddl_audit_status \
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
