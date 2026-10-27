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

// With its audit row: an event commits only together with one (C3).
const INSERT_EVENT: &str =
    "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
     VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-1'); \
     INSERT INTO forge.events (event_id, event_type, schema_version, \
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
        "SELECT command_tag, object_identity FROM forge_audit.ddl_audit_status() WHERE NOT expected",
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
         SELECT id, 'reviewed in a test' FROM forge_audit.ddl_audit_status() \
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

/// Run `sql` as superuser, then each (statement, expected SQLSTATE) as
/// `role` on the same connection.
async fn as_role(t: &FreshDb, setup: &str, role: &str, cases: &[(&str, Option<&str>)]) {
    let mut su = t.superuser().await;
    sqlx::raw_sql(setup).execute(&mut su).await.expect("setup");
    sqlx::raw_sql(&format!("SET ROLE {role}"))
        .execute(&mut su)
        .await
        .expect("set role");
    for (sql, expected) in cases {
        assert_eq!(
            state_of(&mut su, sql).await.as_deref(),
            *expected,
            "{role}: {sql}"
        );
    }
}

/// An event that is published and older than the retention floor (the
/// guard forces occurred_at, so the test moves it with the trigger off).
const OLD_PUBLISHED_EVENT: &str = "SET CONSTRAINTS ALL IMMEDIATE; \
     ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
     UPDATE forge.events SET seq = 1, sequenced_at = now(), \
         occurred_at = now() - interval '30 days' WHERE aggregate_id = 1; \
     ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only";

const FEED_STATE: &str = "INSERT INTO forge.event_feed_state (epoch, system_identifier, \
     timeline_id, high_seq, audit_high_seq, sequencer_heartbeat) \
     VALUES (gen_random_uuid(), 1, 1, 10, 5, now())";

#[tokio::test]
async fn purge_changes_only_payloads_and_deletes_only_old_published_events() {
    let t = migrated().await;
    as_role(
        &t,
        &format!("{INSERT_EVENT}; {INSERT_EVENT_2}"),
        "forge_purge",
        &[
            // Unsequenced: neither redacted nor deleted.
            (
                "UPDATE forge.events SET payload = '{\"redacted\": true}'",
                Some("XF001"),
            ),
            ("UPDATE forge.events SET aggregate_id = 9", Some("42501")),
            ("DELETE FROM forge.events", Some("XF001")),
        ],
    )
    .await;
    as_role(
        &t,
        &format!("RESET ROLE; {OLD_PUBLISHED_EVENT}"),
        "forge_purge",
        &[
            // A published event: redaction carries the marker.
            (
                "UPDATE forge.events SET payload = '{\"name\": \"other\"}' WHERE seq = 1",
                Some("XF001"),
            ),
            (
                "UPDATE forge.events SET payload = '{\"redacted\": true}' WHERE seq = 1",
                None,
            ),
            ("DELETE FROM forge.events WHERE seq = 1", None),
        ],
    )
    .await;
    t.drop_now().await;
}

// With its audit row: an event commits only together with one (C3).
const INSERT_EVENT_2: &str =
    "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
     VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-1'); \
     INSERT INTO forge.events (event_id, event_type, schema_version, \
     aggregate_type, aggregate_id, actor_kind, request_id, payload) VALUES \
     (gen_random_uuid(), 'repository.created', 1, 'repository', 2, 'system', 'r-2', '{}')";

#[tokio::test]
async fn the_sequencing_window_has_both_bounds_and_forces_sequenced_at() {
    let t = migrated().await;
    as_role(
        &t,
        &format!("{INSERT_EVENT}; {FEED_STATE}"),
        "forge_seq",
        &[
            (
                "UPDATE forge.events SET seq = 10, sequenced_at = now()",
                Some("XF001"),
            ),
            (
                "UPDATE forge.events SET seq = 1011, sequenced_at = now()",
                Some("XF001"),
            ),
            (
                "UPDATE forge.events SET seq = 11, sequenced_at = '1999-01-01'",
                None,
            ),
        ],
    )
    .await;
    let recent: bool =
        sqlx::query_scalar("SELECT sequenced_at > now() - interval '1 minute' FROM forge.events")
            .fetch_one(&mut t.superuser().await)
            .await
            .expect("sequenced_at");
    assert!(recent, "the guard sets sequenced_at itself");
    t.drop_now().await;
}

#[tokio::test]
async fn the_audit_sequencing_window_uses_audit_high_seq() {
    let t = migrated().await;
    as_role(
        &t,
        &format!(
            "{FEED_STATE}; INSERT INTO forge.audit_log (actor_kind, action, outcome, \
             resource_type, request_id) VALUES ('system', 'x.y', 'succeeded', 'x', 'r')"
        ),
        "forge_seq",
        &[
            (
                "UPDATE forge.audit_log SET seq = 5, sequenced_at = now() WHERE action = 'x.y'",
                Some("XF001"),
            ),
            (
                "UPDATE forge.audit_log SET seq = 1006, sequenced_at = now() WHERE action = 'x.y'",
                Some("XF001"),
            ),
            (
                "UPDATE forge.audit_log SET seq = 6, sequenced_at = now() WHERE action = 'x.y'",
                None,
            ),
            (
                "UPDATE forge.audit_log SET seq = 7, sequenced_at = now() WHERE action = 'x.y'",
                Some("XF001"),
            ),
        ],
    )
    .await;
    t.drop_now().await;
}

#[tokio::test]
async fn the_application_cannot_write_reserved_audit_actions() {
    let t = migrated().await;
    let mut app = t.db.write_pool().acquire().await.expect("forge_app");
    let insert = |action: &str| {
        format!(
            "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
             request_id) VALUES ('system', '{action}', 'succeeded', 'x', 'r')"
        )
    };
    for (action, expected) in [
        ("db.migrate", Some("XF001")),
        ("audit.purged", Some("XF001")),
        ("feed.epoch_rotated", Some("XF001")),
        ("events.retention", Some("XF001")),
        ("repository.created", None),
    ] {
        assert_eq!(
            state_of(&mut app, &insert(action)).await.as_deref(),
            expected,
            "{action}"
        );
    }
    drop(app);
    t.drop_now().await;
}

#[tokio::test]
async fn the_feed_state_only_moves_forward() {
    let t = migrated().await;
    as_role(
        &t,
        FEED_STATE,
        "forge_seq",
        &[
            (
                "UPDATE forge.event_feed_state SET high_seq = 9",
                Some("XF001"),
            ),
            (
                "UPDATE forge.event_feed_state SET audit_high_seq = 4",
                Some("XF001"),
            ),
            (
                "UPDATE forge.event_feed_state SET high_seq = 12, epoch = gen_random_uuid()",
                None,
            ),
        ],
    )
    .await;
    as_role(
        &t,
        "RESET ROLE",
        "forge_purge",
        &[
            (
                "UPDATE forge.event_feed_state SET retained_from = 0",
                Some("XF001"),
            ),
            (
                "UPDATE forge.event_feed_state SET retained_from = 14",
                Some("XF001"),
            ),
            ("UPDATE forge.event_feed_state SET retained_from = 13", None),
        ],
    )
    .await;
    let mut su = t.superuser().await;
    assert_eq!(
        state_of(&mut su, "DELETE FROM forge.event_feed_state")
            .await
            .as_deref(),
        Some("XF001")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_sees_none_of_the_personal_or_act_as_columns() {
    let t = migrated().await;
    for column in [
        "actor_label",
        "user_agent",
        "act_as_claimed",
        "act_as_key_id",
    ] {
        let err = sqlx::query(&format!("SELECT {column} FROM forge.audit_log"))
            .fetch_all(t.db.pool().executor())
            .await
            .map_err(|e| crate::read_query_error("test.pii", e))
            .expect_err(column);
        assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn more_weakened_schema_guards_are_reported() {
    let _lock = ClusterLock::shared().await;
    for (tamper, guard, context) in [
        (
            "CREATE RULE swallow AS ON INSERT TO forge.audit_log DO INSTEAD NOTHING",
            "guard=object_owner",
            "object=forge.audit_log",
        ),
        (
            "ALTER TABLE forge.events ENABLE ROW LEVEL SECURITY",
            "guard=object_owner",
            "object=forge.events",
        ),
        (
            "DROP TABLE forge.jobs",
            "guard=object_owner",
            "object=forge.jobs",
        ),
        (
            "CREATE OR REPLACE FUNCTION forge.events_guard() RETURNS trigger LANGUAGE plpgsql \
             SET search_path = pg_catalog, pg_temp AS $$ BEGIN RETURN NEW; END $$",
            "guard=function_body",
            "function=events_guard",
        ),
        (
            "ALTER FUNCTION forge.audit_log_guard() SET search_path = forge, pg_catalog",
            "guard=function_body",
            "function=audit_log_guard",
        ),
        (
            "DROP TRIGGER events_no_truncate ON forge.events",
            "guard=log_triggers",
            "trigger=events_no_truncate",
        ),
        (
            "DROP TRIGGER audit_log_append_only ON forge.audit_log; \
             CREATE TRIGGER audit_log_append_only BEFORE INSERT OR UPDATE OR DELETE \
             ON forge.audit_log FOR EACH ROW EXECUTE FUNCTION forge.events_guard(); \
             ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_append_only",
            "guard=log_triggers",
            "trigger=audit_log_append_only",
        ),
        (
            "DROP TRIGGER events_append_only ON forge.events; \
             CREATE TRIGGER events_append_only BEFORE INSERT OR UPDATE OR DELETE \
             ON forge.events FOR EACH ROW WHEN (false) EXECUTE FUNCTION forge.events_guard(); \
             ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only",
            "guard=log_triggers",
            "trigger=events_append_only",
        ),
        (
            "ALTER TABLE forge.event_feed_state DISABLE TRIGGER event_feed_state_append_only",
            "guard=log_triggers",
            "trigger=event_feed_state_append_only",
        ),
        (
            "GRANT UPDATE ON forge.event_feed_state TO forge_app",
            "guard=privileges",
            "object=forge.event_feed_state",
        ),
        (
            "GRANT DELETE ON forge.event_subscribers TO forge_ro",
            "guard=privileges",
            "object=forge.event_subscribers",
        ),
        (
            "GRANT UPDATE (enabled) ON forge.event_subscribers TO forge_app",
            "guard=privileges",
            "object=forge.event_subscribers",
        ),
    ] {
        let t = migrated().await;
        t.exec_superuser(tamper).await;
        let text = bootstrap::check(&t.db, &[])
            .await
            .expect_err(tamper)
            .to_string();
        assert!(
            text.contains(guard) && text.contains(context),
            "{tamper}\n=> {text}"
        );
        t.drop_now().await;
    }
}

#[tokio::test]
async fn a_weakened_audit_presence_check_is_reported() {
    let _lock = ClusterLock::shared().await;
    let recreate = |options: &str| {
        format!(
            "DROP TRIGGER events_audit_present ON forge.events; \
             CREATE CONSTRAINT TRIGGER events_audit_present AFTER INSERT ON forge.events \
             {options} EXECUTE FUNCTION forge.events_audit_present(); \
             ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_audit_present"
        )
    };
    for (tamper, guard, context) in [
        (
            "DROP TRIGGER events_audit_present ON forge.events".to_owned(),
            "guard=log_triggers",
            "trigger=events_audit_present",
        ),
        (
            recreate("NOT DEFERRABLE FOR EACH ROW"),
            "guard=log_triggers",
            "trigger=events_audit_present",
        ),
        (
            recreate("DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.caused_by_seq IS NULL)"),
            "guard=log_triggers",
            "trigger=events_audit_present",
        ),
        (
            "ALTER TABLE forge.events DISABLE TRIGGER events_audit_present".to_owned(),
            "guard=log_triggers",
            "trigger=events_audit_present",
        ),
        (
            "CREATE OR REPLACE FUNCTION forge.events_audit_present() RETURNS trigger \
             LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NULL; END $$"
                .to_owned(),
            "guard=function_body",
            "function=events_audit_present",
        ),
    ] {
        let t = migrated().await;
        t.exec_superuser(&tamper).await;
        let text = bootstrap::check(&t.db, &[])
            .await
            .expect_err(&tamper)
            .to_string();
        assert!(
            text.contains(guard) && text.contains(context),
            "{tamper}\n=> {text}"
        );
        t.drop_now().await;
    }
}

/// DDL in the migration login's transaction together with a db.migrate row
/// listing `changes` — what a later migration file would do.
async fn migration_like(t: &FreshDb, ddl: &str, changes: &str) {
    let mut mc = Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator");
    sqlx::raw_sql(&format!(
        "BEGIN; {ddl}; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, \
         details) VALUES ('system', 'db.migrate', 'succeeded', 'migration', 'db.migrate', \
         '{{\"guarded_changes\": {changes}}}'); COMMIT"
    ))
    .execute(&mut mc.conn)
    .await
    .expect("migration-like transaction");
    mc.close().await;
}

async fn unexpected_rows(t: &FreshDb) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT command_tag, object_identity FROM forge_audit.ddl_audit_status() \
         WHERE NOT expected ORDER BY id",
    )
    .fetch_all(t.db.pool().executor())
    .await
    .expect("status")
}

#[tokio::test]
async fn a_guarded_change_must_be_listed_by_its_own_name() {
    // Not listed at all.
    let t = migrated().await;
    migration_like(
        &t,
        "ALTER TABLE forge.events ALTER COLUMN request_id SET DEFAULT 'x'",
        "[]",
    )
    .await;
    assert_eq!(unexpected_rows(&t).await.len(), 1);
    t.drop_now().await;
    // Listed: expected.
    let t = migrated().await;
    migration_like(
        &t,
        "ALTER TABLE forge.events ALTER COLUMN request_id SET DEFAULT 'x'",
        "[\"forge.events\"]",
    )
    .await;
    assert!(unexpected_rows(&t).await.is_empty());
    t.drop_now().await;
    // A trigger on a guarded table is not covered by listing the table.
    let t = migrated().await;
    migration_like(
        &t,
        "CREATE TRIGGER sneak BEFORE INSERT ON forge.events \
         FOR EACH ROW EXECUTE FUNCTION forge.events_guard()",
        "[\"forge.events\"]",
    )
    .await;
    let rows = unexpected_rows(&t).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    t.drop_now().await;
    // Nor is a rule.
    let t = migrated().await;
    migration_like(
        &t,
        "CREATE RULE swallow AS ON INSERT TO forge.audit_log DO INSTEAD NOTHING",
        "[\"forge.audit_log\"]",
    )
    .await;
    assert_eq!(unexpected_rows(&t).await.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_partial_acknowledgement_still_reports_the_rest() {
    let _lock = ClusterLock::shared().await;
    let t = migrated().await;
    t.exec_superuser("CREATE TABLE forge.rogue_a (i int); CREATE TABLE forge.rogue_b (i int)")
        .await;
    let maint_url = url_as("forge_maint", &t.name);
    let options: sqlx::postgres::PgConnectOptions = maint_url.parse().expect("maint url");
    let mut maint = sqlx::ConnectOptions::connect(&options.password(MAINT_PASSWORD))
        .await
        .expect("forge_maint");
    sqlx::raw_sql(
        "INSERT INTO forge_audit.ddl_audit_ack (ddl_audit_id, note) \
         SELECT min(id), 'one of two' FROM forge_audit.ddl_audit_status() \
         WHERE NOT expected AND NOT acknowledged",
    )
    .execute(&mut maint)
    .await
    .expect("acknowledge one");
    let text = bootstrap::check(&t.db, &[])
        .await
        .expect_err("one left")
        .to_string();
    assert!(text.contains("reason=unexpected_ddl"), "{text}");
    assert!(text.contains("count=1"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn the_feed_state_moves_at_most_one_batch_per_update() {
    let t = migrated().await;
    as_role(
        &t,
        FEED_STATE,
        "forge_seq",
        &[
            (
                "UPDATE forge.event_feed_state SET high_seq = 1011",
                Some("XF001"),
            ),
            (
                "UPDATE forge.event_feed_state SET audit_high_seq = 1006",
                Some("XF001"),
            ),
            ("UPDATE forge.event_feed_state SET high_seq = 1010", None),
        ],
    )
    .await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_quoted_name_does_not_hide_a_guarded_change() {
    let t = migrated().await;
    migration_like(
        &t,
        "CREATE RULE \"r on forge.jobs\" AS ON INSERT TO forge.events DO INSTEAD NOTHING",
        "[]",
    )
    .await;
    assert_eq!(unexpected_rows(&t).await.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_guarded_table_never_inherits_or_becomes_a_partition() {
    let t = migrated().await;
    let mut su = t.superuser().await;
    for sql in [
        "CREATE TABLE forge.audit_archive () INHERITS (forge.audit_log)",
        "CREATE TABLE forge.kid (LIKE forge.events); ALTER TABLE forge.events INHERIT forge.kid",
        "CREATE TABLE forge.pp (LIKE forge.audit_log) PARTITION BY LIST (action); \
         ALTER TABLE forge.pp ATTACH PARTITION forge.audit_log DEFAULT",
    ] {
        assert_eq!(
            state_of(&mut su, sql).await.as_deref(),
            Some("XF001"),
            "{sql}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_rule_on_the_bookkeeping_is_reported() {
    let _lock = ClusterLock::shared().await;
    let t = migrated().await;
    t.exec_superuser("CREATE RULE keep AS ON DELETE TO forge._sqlx_migrations DO INSTEAD NOTHING")
        .await;
    let text = bootstrap::check(&t.db, &[])
        .await
        .expect_err("rule")
        .to_string();
    assert!(
        text.contains("guard=object_owner") && text.contains("object=forge._sqlx_migrations"),
        "{text}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_db_migrate_row_outside_the_shipped_migrations_is_reported() {
    let _lock = ClusterLock::shared().await;
    for forged in [
        // A version this binary does not ship.
        "{\"version\": 1, \"guarded_changes\": [\"forge.audit_log\"]}",
        // A second row for a shipped version.
        "{\"version\": 20261027100000, \"guarded_changes\": []}",
    ] {
        let t = migrated().await;
        // As forge_owner: only a migration's role may write db.migrate rows.
        t.exec_superuser(&format!(
            "SET ROLE forge_owner; INSERT INTO forge.audit_log (actor_kind, action, outcome, \
             resource_type, request_id, details) VALUES ('system', 'db.migrate', 'succeeded', \
             'migration', 'db.migrate', '{forged}')"
        ))
        .await;
        let text = bootstrap::check(&t.db, &[])
            .await
            .expect_err(forged)
            .to_string();
        assert!(
            text.contains("guard=migration_records"),
            "{forged}\n=> {text}"
        );
        t.drop_now().await;
    }
}

#[tokio::test]
async fn redaction_only_blanks_values_of_a_published_event() {
    let t = migrated().await;
    as_role(
        &t,
        &format!(
            "{INSERT_EVENT}; SET CONSTRAINTS ALL IMMEDIATE; \
             ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
             UPDATE forge.events SET seq = 1, sequenced_at = now(), \
                 payload = '{{\"name\": \"alice\", \"repo\": 7}}'; \
             ALTER TABLE forge.events ENABLE ALWAYS TRIGGER events_append_only"
        ),
        "forge_purge",
        &[
            // A new key.
            (
                "UPDATE forge.events SET payload = '{\"name\": \"[redacted]\", \"repo\": 7, \
                 \"redacted\": true, \"x\": 1}'",
                Some("XF001"),
            ),
            // A different value instead of the marker.
            (
                "UPDATE forge.events SET payload = '{\"name\": \"mallory\", \"repo\": 7, \
                 \"redacted\": true}'",
                Some("XF001"),
            ),
            // A dropped key.
            (
                "UPDATE forge.events SET payload = '{\"name\": \"[redacted]\", \"redacted\": true}'",
                Some("XF001"),
            ),
            (
                "UPDATE forge.events SET payload = '{\"name\": \"[redacted]\", \"repo\": 7, \
                 \"redacted\": true}'",
                None,
            ),
        ],
    )
    .await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_change_to_an_identity_sequence_touches_its_table() {
    let t = migrated().await;
    migration_like(&t, "ALTER SEQUENCE forge.audit_log_id_seq CACHE 2", "[]").await;
    assert_eq!(unexpected_rows(&t).await.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn only_migrations_write_db_migrate_rows_and_guarded_tables_keep_their_names() {
    let t = migrated().await;
    let mut su = t.superuser().await;
    for (sql, expected) in [
        (
            "SET ROLE forge_seq; INSERT INTO forge.audit_log (actor_kind, action, outcome, \
             resource_type, request_id) VALUES ('system', 'db.migrate', 'succeeded', 'x', 'r')",
            Some("XF001"),
        ),
        ("RESET ROLE", None),
        (
            "ALTER TABLE forge.audit_log RENAME TO audit_log_old",
            Some("XF001"),
        ),
        ("ALTER TABLE forge.events SET SCHEMA public", Some("XF001")),
    ] {
        assert_eq!(state_of(&mut su, sql).await.as_deref(), expected, "{sql}");
    }
    t.drop_now().await;
}
