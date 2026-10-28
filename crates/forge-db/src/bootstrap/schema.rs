//! Guards of the migrated schema (ADR-0016 §1, ADR-0018 §3): the append-only
//! triggers of events and audit_log and the exact privileges of the tables
//! the first migration creates. Before that migration the tables do not
//! exist; the schema check reports the pending migration instead.

use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use super::acl::{compare, table_grants_sql};
use super::guard_missing;
use crate::read_query_error;

/// The guard functions of the migrated schema.
const GUARD_FUNCTIONS: &[&str] = &[
    "forge.events_guard()",
    "forge.audit_log_guard()",
    "forge.event_feed_state_guard()",
    "forge.events_audit_present()",
    // ── C5: derived-event checks (migration 20261027140000) ──
    "forge.events_lineage()",
    "forge.events_derived_progress()",
    // ── end C5 ──
];

/// Every trigger of each guarded table, as `pg_get_triggerdef` prints it
/// (with `search_path = pg_catalog`): no other trigger may exist.
const GUARDED_TRIGGERS: &[(&str, &[(&str, &str)])] = &[
    (
        "events",
        &[
            (
                "events_append_only",
                "CREATE TRIGGER events_append_only BEFORE INSERT OR DELETE OR UPDATE ON \
                 forge.events FOR EACH ROW EXECUTE FUNCTION forge.events_guard()",
            ),
            (
                "events_no_truncate",
                "CREATE TRIGGER events_no_truncate BEFORE TRUNCATE ON forge.events FOR EACH \
                 STATEMENT EXECUTE FUNCTION forge.events_guard()",
            ),
            // ── C5: plain events only again, plus the derived-event checks ──
            (
                "events_audit_present",
                "CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NULL)) \
                 EXECUTE FUNCTION forge.events_audit_present()",
            ),
            (
                "events_lineage",
                "CREATE TRIGGER events_lineage BEFORE INSERT ON forge.events FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NOT NULL)) \
                 EXECUTE FUNCTION forge.events_lineage()",
            ),
            (
                "events_derived_progress",
                "CREATE CONSTRAINT TRIGGER events_derived_progress AFTER INSERT ON \
                 forge.events DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                 WHEN ((new.caused_by_seq IS NOT NULL)) \
                 EXECUTE FUNCTION forge.events_derived_progress()",
            ),
            // ── end C5 ──
        ],
    ),
    (
        "audit_log",
        &[
            (
                "audit_log_append_only",
                "CREATE TRIGGER audit_log_append_only BEFORE INSERT OR DELETE OR UPDATE ON \
                 forge.audit_log FOR EACH ROW EXECUTE FUNCTION forge.audit_log_guard()",
            ),
            (
                "audit_log_no_truncate",
                "CREATE TRIGGER audit_log_no_truncate BEFORE TRUNCATE ON forge.audit_log FOR \
                 EACH STATEMENT EXECUTE FUNCTION forge.audit_log_guard()",
            ),
        ],
    ),
    (
        "event_feed_state",
        &[
            (
                "event_feed_state_append_only",
                "CREATE TRIGGER event_feed_state_append_only BEFORE DELETE OR UPDATE ON \
                 forge.event_feed_state FOR EACH ROW EXECUTE FUNCTION \
                 forge.event_feed_state_guard()",
            ),
            (
                "event_feed_state_no_truncate",
                "CREATE TRIGGER event_feed_state_no_truncate BEFORE TRUNCATE ON \
                 forge.event_feed_state FOR EACH STATEMENT EXECUTE FUNCTION \
                 forge.event_feed_state_guard()",
            ),
        ],
    ),
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
                columns("forge_maint", "SELECT", &["txid", "action", "details"]),
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
            [
                plain(&["forge_app:SELECT", "forge_purge:SELECT", "forge_ro:SELECT"]),
                columns(
                    "forge_app",
                    "INSERT",
                    &["name", "ordering", "last_seq", "last_txid", "updated_at"],
                ),
                columns(
                    "forge_app",
                    "UPDATE",
                    &[
                        "last_seq",
                        "last_txid",
                        "updated_at",
                        "failures",
                        "failing_since",
                        "last_error_code",
                    ],
                ),
            ]
            .concat(),
        ),
        (
            "forge.subscriber_dead_letters",
            plain(&[
                "forge_app:SELECT",
                "forge_app:INSERT",
                "forge_purge:SELECT",
                "forge_purge:DELETE",
                "forge_ro:SELECT",
            ]),
        ),
        ("forge.jobs", operational()),
    ]
}

/// The first schema migration (events, audit_log, …).
const FIRST_SCHEMA_VERSION: i64 = 20261027100000;

/// Every table the first migration creates.
const SCHEMA_TABLES: &[&str] = &[
    "events",
    "audit_log",
    "event_feed_state",
    "event_subscribers",
    "subscriber_dead_letters",
    "jobs",
];

/// Whether the first schema migration is recorded as applied. Before it,
/// the schema check reports it pending; after it, a missing table is a
/// guard failure, never a skip.
pub(super) async fn schema_applied(conn: &mut PgConnection) -> Result<bool, ForgeError> {
    // Either the bookkeeping says so, or the tables are there (forge_owner
    // can delete the bookkeeping row with plain DML, which the DDL log does
    // not see).
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge._sqlx_migrations WHERE version = $1 AND success) \
         OR to_regclass('forge.audit_log') IS NOT NULL",
    )
    .bind(FIRST_SCHEMA_VERSION)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.schema_applied", e))
}

pub(super) async fn check_schema_objects(conn: &mut PgConnection) -> Result<(), ForgeError> {
    if !schema_applied(conn).await? {
        return Ok(());
    }
    // Present, owned by forge_owner, no rules, no row security.
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT c.relname::text, \
         pg_catalog.pg_get_userbyid(c.relowner) = 'forge_owner' \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_rewrite rw WHERE rw.ev_class = c.oid) \
         AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity \
         AND c.relkind = 'r' AND NOT c.relispartition \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits i \
                         WHERE i.inhparent = c.oid OR i.inhrelid = c.oid) \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge' AND c.relname = ANY($1)",
    )
    .bind(
        [SCHEMA_TABLES, &["_sqlx_migrations"]]
            .concat()
            .iter()
            .map(|t| (*t).to_owned())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.schema_tables", e))?;
    for table in SCHEMA_TABLES.iter().chain(&["_sqlx_migrations"]) {
        if !rows.iter().any(|(name, ok)| name == table && *ok) {
            return Err(
                guard_missing("object_owner").with_context("object", format!("forge.{table}"))
            );
        }
    }
    for function in GUARD_FUNCTIONS {
        check_guard_function(conn, function).await?;
    }
    for (table, expected) in GUARDED_TRIGGERS {
        // The whole definition (timing, events, level, WHEN, columns,
        // deferral, function) as PostgreSQL prints it, and enabled ALWAYS.
        let rows: Vec<(String, String, String)> = sqlx::query_as(&format!(
            "SELECT t.tgname::text, t.tgenabled::text, pg_catalog.pg_get_triggerdef(t.oid) \
             FROM pg_catalog.pg_trigger t \
             WHERE t.tgrelid = 'forge.{table}'::regclass AND NOT t.tgisinternal"
        ))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| read_query_error("bootstrap.schema_triggers", e))?;
        for (name, definition) in *expected {
            let ok = rows
                .iter()
                .any(|(n, enabled, def)| n == name && enabled == "A" && def == definition);
            if !ok {
                return Err(guard_missing("log_triggers").with_context("trigger", *name));
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

/// A guard function as the migrations ship it: owned by forge_owner, not
/// SECURITY DEFINER, plpgsql, `search_path = pg_catalog, pg_temp`, and the
/// body of its last definition in the migrations built into this binary.
async fn check_guard_function(conn: &mut PgConnection, signature: &str) -> Result<(), ForgeError> {
    let name = signature
        .trim_start_matches("forge.")
        .trim_end_matches("()");
    let found: Option<(bool, String)> = sqlx::query_as(
        "SELECT pg_catalog.pg_get_userbyid(p.proowner) = 'forge_owner' AND NOT p.prosecdef \
         AND p.proconfig = ARRAY['search_path=pg_catalog, pg_temp'] \
         AND p.prolang = (SELECT l.oid FROM pg_catalog.pg_language l WHERE l.lanname = 'plpgsql'), \
         p.prosrc \
         FROM pg_catalog.pg_proc p WHERE p.oid = pg_catalog.to_regprocedure($1)",
    )
    .bind(signature)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| read_query_error("bootstrap.guard_function", e))?;
    let Some((shape, body)) = found else {
        return Err(guard_missing("function_owner").with_context("function", name));
    };
    if !shape || migration_body(name).as_deref() != Some(body.as_str()) {
        return Err(guard_missing("function_body").with_context("function", name));
    }
    Ok(())
}

/// The body of `forge.<name>()` in the last migration that defines it.
fn migration_body(name: &str) -> Option<String> {
    let marker = format!("FUNCTION forge.{name}()");
    crate::MIGRATOR.iter().rev().find_map(|m| {
        let sql = m.sql.as_ref();
        // A definition, not a trigger's `EXECUTE FUNCTION …`.
        let start = sql
            .rmatch_indices(marker.as_str())
            .map(|(i, _)| i)
            .find(|&i| {
                let before = sql.get(..i).unwrap_or_default();
                before.ends_with("CREATE ") || before.ends_with("CREATE OR REPLACE ")
            })?;
        let rest = sql.get(start..)?;
        let open = rest.find("AS $$")? + "AS $$".len();
        let body = rest.get(open..)?;
        let close = body.find("$$")?;
        body.get(..close).map(str::to_owned)
    })
}
