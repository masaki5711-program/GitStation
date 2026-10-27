//! The product schema (migrations/) against the ledger and ADR-0018's
//! append-only rules, each in a fresh, migrated database.

use std::collections::BTreeSet;
use std::time::Duration;

use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use crate::test_support::*;
use crate::{bootstrap, migrate, query_error, Db, MIGRATOR};

const LONG: Duration = Duration::from_secs(60);

async fn migrated() -> FreshDb {
    let t = FreshDb::new().await;
    let mut mc = Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator");
    migrate(&mut mc, &MIGRATOR, LONG).await.expect("migrate");
    mc.close().await;
    t
}

/// The SQLSTATE of a failed statement.
async fn state_of(conn: &mut PgConnection, sql: &str) -> Option<String> {
    match sqlx::raw_sql(sql).execute(conn).await {
        Ok(_) => None,
        Err(e) => e
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned()),
    }
}

fn ledger() -> toml::Table {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tables.toml"))
        .expect("tables.toml");
    text.parse().expect("tables.toml parses")
}

fn ledger_tables(guarded_only: bool) -> BTreeSet<String> {
    ledger()["tables"]
        .as_table()
        .expect("[tables]")
        .iter()
        .filter(|(_, t)| !guarded_only || t["guarded"].as_bool() == Some(true))
        .map(|(name, _)| name.clone())
        .collect()
}

const INSERT_EVENT: &str = "INSERT INTO forge.events (event_id, event_type, schema_version, \
     aggregate_type, aggregate_id, actor_kind, request_id, payload) VALUES \
     (gen_random_uuid(), 'repository.created', 1, 'repository', 1, 'system', 'r-1', '{}')";

#[tokio::test]
async fn migrations_keep_every_bootstrap_guard_intact() {
    let _lock = ClusterLock::shared().await;
    let t = migrated().await;
    bootstrap::check(&t.db, &[])
        .await
        .expect("guards after migrate");
    t.drop_now().await;
}

#[tokio::test]
async fn every_table_is_in_the_ledger() {
    let t = migrated().await;
    let found: BTreeSet<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'forge' AND c.relkind IN ('r', 'p')",
    )
    .fetch_all(t.db.pool().executor())
    .await
    .expect("tables")
    .into_iter()
    .collect();
    assert_eq!(found, ledger_tables(false));
    t.drop_now().await;
}

#[tokio::test]
async fn guarded_objects_match_the_ledger() {
    let t = migrated().await;
    let registered: BTreeSet<String> = sqlx::query_scalar(
        "SELECT object_identity FROM forge_audit.guarded_objects \
         WHERE object_type = 'table' AND object_identity LIKE 'forge.%'",
    )
    .fetch_all(t.db.pool().executor())
    .await
    .expect("registry")
    .into_iter()
    .map(|name: String| name.trim_start_matches("forge.").to_owned())
    .collect();
    assert_eq!(registered, ledger_tables(true));
    t.drop_now().await;
}

#[tokio::test]
async fn the_migration_records_itself_with_its_guarded_changes() {
    let t = migrated().await;
    let (txid, changes): (String, String) = sqlx::query_as(
        "SELECT txid::text, (details->'guarded_changes')::text FROM forge.audit_log \
         WHERE action = 'db.migrate'",
    )
    .fetch_one(t.db.write_pool())
    .await
    .expect("db.migrate row");
    let changes: serde_json::Value = serde_json::from_str(&changes).expect("json");
    assert!(
        changes
            .as_array()
            .is_some_and(|c| c.iter().any(|v| v == "forge.events")),
        "{changes}"
    );
    // Its DDL is in the DDL log under the same transaction.
    let ddl: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge_audit.ddl_audit \
         WHERE txid::text = $1 AND object_identity = 'forge.events'",
    )
    .bind(&txid)
    .fetch_one(t.db.pool().executor())
    .await
    .expect("ddl rows");
    assert!(ddl >= 1);
    t.drop_now().await;
}

#[tokio::test]
async fn the_application_appends_events_but_cannot_change_them() {
    let t = migrated().await;
    let mut app = t.db.write_pool().acquire().await.expect("forge_app");
    sqlx::raw_sql(INSERT_EVENT)
        .execute(&mut *app)
        .await
        .map_err(|e| query_error("test.event", e))
        .expect("append");
    // txid and occurred_at are the guard's, whatever the caller meant.
    let forced: bool = sqlx::query_scalar(
        "SELECT txid <> '1'::xid8 AND occurred_at > now() - interval '1 minute' FROM forge.events",
    )
    .fetch_one(&mut *app)
    .await
    .expect("read back");
    assert!(forced);
    for sql in [
        "UPDATE forge.events SET payload = '{\"x\": 1}'",
        "DELETE FROM forge.events",
        "TRUNCATE forge.events",
        "INSERT INTO forge.events (seq, event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES \
         (1, gen_random_uuid(), 'a.b', 1, 'x', 1, 'system', 'r', '{}')",
        "INSERT INTO forge.audit_log (txid, actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('1', 'system', 'a', 'succeeded', 'x', 'r')",
    ] {
        assert_eq!(state_of(&mut app, sql).await.as_deref(), Some("42501"), "{sql}");
    }
    drop(app);
    t.drop_now().await;
}

#[tokio::test]
async fn the_guards_hold_even_for_the_owner() {
    let t = migrated().await;
    let mut su = t.superuser().await;
    sqlx::raw_sql(INSERT_EVENT)
        .execute(&mut su)
        .await
        .expect("append");
    for sql in [
        "UPDATE forge.events SET payload = '{\"x\": 1}'",
        "DELETE FROM forge.events",
        "TRUNCATE forge.events",
        "UPDATE forge.audit_log SET action = 'x'",
        "TRUNCATE forge.audit_log",
        "INSERT INTO forge.events (seq, event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES \
         (7, gen_random_uuid(), 'a.b', 1, 'x', 1, 'system', 'r', '{}')",
    ] {
        assert_eq!(
            state_of(&mut su, sql).await.as_deref(),
            Some("XF001"),
            "{sql}"
        );
    }
    let err = sqlx::query("DELETE FROM forge.audit_log")
        .execute(&mut su)
        .await
        .map_err(|e| query_error("test.audit_delete", e))
        .expect_err("append-only");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "rejected");
    t.drop_now().await;
}

#[tokio::test]
async fn only_the_sequencer_assigns_seq_and_only_within_the_window() {
    let t = migrated().await;
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!(
        "{INSERT_EVENT}; \
         INSERT INTO forge.event_feed_state (epoch, system_identifier, timeline_id, \
         sequencer_heartbeat) VALUES (gen_random_uuid(), 1, 1, now())"
    ))
    .execute(&mut su)
    .await
    .expect("event and feed state");
    sqlx::raw_sql("SET ROLE forge_seq")
        .execute(&mut su)
        .await
        .expect("forge_seq");
    for (sql, expected) in [
        // Beyond high_seq + 1000.
        (
            "UPDATE forge.events SET seq = 1001, sequenced_at = now()",
            Some("XF001"),
        ),
        // seq together with another column.
        (
            "UPDATE forge.events SET seq = 1, sequenced_at = now(), request_id = 'x'",
            Some("42501"),
        ),
        (
            "UPDATE forge.events SET seq = 1, sequenced_at = now()",
            None,
        ),
        // Once numbered, never again.
        (
            "UPDATE forge.events SET seq = 2, sequenced_at = now()",
            Some("XF001"),
        ),
    ] {
        assert_eq!(state_of(&mut su, sql).await.as_deref(), expected, "{sql}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn redaction_may_only_clear_the_personal_columns() {
    let t = migrated().await;
    let mut su = t.superuser().await;
    sqlx::raw_sql(
        "INSERT INTO forge.audit_log (actor_kind, actor_label, source_ip, user_agent, action, \
         outcome, resource_type, request_id) VALUES \
         ('anonymous', 'someone', '192.0.2.1', 'agent', 'login', 'denied', 'session', 'r-1'); \
         SET ROLE forge_purge",
    )
    .execute(&mut su)
    .await
    .expect("audit row");
    for (sql, expected) in [
        (
            "UPDATE forge.audit_log SET actor_label = 'other'",
            Some("XF001"),
        ),
        (
            "UPDATE forge.audit_log SET actor_label = NULL, source_ip = NULL, user_agent = NULL",
            None,
        ),
    ] {
        assert_eq!(state_of(&mut su, sql).await.as_deref(), expected, "{sql}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_sees_no_personal_audit_columns() {
    let t = migrated().await;
    let read = t.db.pool().executor();
    sqlx::query("SELECT action, request_id FROM forge.audit_log")
        .fetch_all(read)
        .await
        .expect("allowed columns");
    let err = sqlx::query("SELECT source_ip FROM forge.audit_log")
        .fetch_all(read)
        .await
        .map_err(|e| crate::read_query_error("test.pii", e))
        .expect_err("personal column");
    assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    t.drop_now().await;
}

#[tokio::test]
async fn weakened_schema_guards_are_reported() {
    let _lock = ClusterLock::shared().await;
    for (tamper, guard) in [
        (
            "ALTER TABLE forge.events ENABLE TRIGGER events_append_only",
            "guard=log_triggers",
        ),
        (
            "ALTER TABLE forge.audit_log DISABLE TRIGGER audit_log_no_truncate",
            "guard=log_triggers",
        ),
        (
            "CREATE TRIGGER extra AFTER INSERT ON forge.events \
             FOR EACH ROW EXECUTE FUNCTION forge.events_guard()",
            "guard=log_triggers",
        ),
        (
            "GRANT UPDATE ON forge.events TO forge_app",
            "guard=privileges",
        ),
        (
            "GRANT SELECT (source_ip) ON forge.audit_log TO forge_ro",
            "guard=privileges",
        ),
        (
            "GRANT TRUNCATE ON forge.jobs TO forge_app",
            "guard=privileges",
        ),
    ] {
        let t = migrated().await;
        t.exec_superuser(tamper).await;
        let text = bootstrap::check(&t.db, &[])
            .await
            .expect_err(tamper)
            .to_string();
        assert!(text.contains(guard), "{tamper}\n=> {text}");
        t.drop_now().await;
    }
}

#[tokio::test]
async fn migrations_leave_no_unexpected_ddl() {
    let t = migrated().await;
    let unexpected: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT command_tag, object_identity FROM forge.ddl_audit_status WHERE NOT expected",
    )
    .fetch_all(t.db.pool().executor())
    .await
    .expect("status");
    assert!(unexpected.is_empty(), "{unexpected:?}");
    t.drop_now().await;
}

#[tokio::test]
async fn ddl_outside_a_migration_is_unexpected_until_acknowledged() {
    let _lock = ClusterLock::shared().await;
    let t = migrated().await;
    // The migration login, but no migration (no db.migrate row in its
    // transaction).
    let mut mc = Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator");
    sqlx::raw_sql("CREATE TABLE forge.rogue (i int)")
        .execute(&mut mc.conn)
        .await
        .expect("ad-hoc DDL");
    mc.close().await;
    let err = bootstrap::check(&t.db, &[])
        .await
        .expect_err("unexpected DDL");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "unexpected_ddl");
    // forge_maint acknowledges each reviewed row.
    let maint_url = url_as("forge_maint", &t.name);
    let options: sqlx::postgres::PgConnectOptions = maint_url.parse().expect("maint url");
    let mut maint = sqlx::ConnectOptions::connect(&options.password(MAINT_PASSWORD))
        .await
        .expect("forge_maint");
    sqlx::raw_sql(
        "INSERT INTO forge_audit.ddl_audit_ack (ddl_audit_id, note) \
         SELECT id, 'reviewed in a test' FROM forge.ddl_audit_status \
         WHERE NOT expected AND NOT acknowledged",
    )
    .execute(&mut maint)
    .await
    .expect("acknowledge");
    bootstrap::check(&t.db, &[])
        .await
        .expect("acknowledged rows are fine");
    t.drop_now().await;
}
