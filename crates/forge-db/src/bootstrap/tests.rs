//! Bootstrap tests: the real files of ops/db/bootstrap/ in fresh databases.
//! Every test that runs the role guards holds the shared cluster lock; a
//! test that changes a cluster role holds the exclusive one and restores it.

use forge_core::config::PrivilegedMember;
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::objects::{shipped_body, FUNCTIONS};
use super::*;
use crate::query_error;
use crate::test_support::*;

async fn exec(conn: &mut PgConnection, sql: &str) {
    sqlx::raw_sql(sql)
        .execute(conn)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn guard_error(t: &FreshDb, members: &[PrivilegedMember]) -> String {
    check(&t.db, members)
        .await
        .expect_err("a weakened guard")
        .to_string()
}

#[tokio::test]
async fn a_fresh_bootstrap_passes_every_guard() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    assert_eq!(check(&t.db, &[]).await.expect("guards"), BOOTSTRAP_VERSION);
    t.drop_now().await;
}

#[tokio::test]
async fn cluster_bootstrap_is_idempotent_and_records_each_run() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    let mut su = t.superuser().await;
    exec(&mut su, &bootstrap_sql(0)).await;
    exec(&mut su, &bootstrap_sql(2)).await;
    let runs: Vec<(i32, String)> = sqlx::query_as(
        "SELECT bootstrap_version, txid::text FROM forge_audit.bootstrap_log ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("log");
    assert_eq!(runs.len(), 2, "{runs:?}");
    // The re-run's DDL is in the log under the re-run's txid: expected.
    let own: i64 =
        sqlx::query_scalar("SELECT count(*) FROM forge_audit.ddl_audit WHERE txid::text = $1")
            .bind(&runs[1].1)
            .fetch_one(&mut su)
            .await
            .expect("ddl of the re-run");
    assert!(own > 0);
    check(&t.db, &[]).await.expect("still intact");
    t.drop_now().await;
}

#[tokio::test]
async fn ddl_audit_rejects_update_and_delete() {
    let t = FreshDb::new().await;
    let mut su = t.superuser().await;
    for sql in [
        "UPDATE forge_audit.ddl_audit SET event = 'x'",
        "DELETE FROM forge_audit.ddl_audit",
        "TRUNCATE forge_audit.bootstrap_log",
        "UPDATE forge_audit.guarded_objects SET object_type = 'x'",
    ] {
        let err = sqlx::query(sql).execute(&mut su).await.expect_err(sql);
        let state = err.as_database_error().and_then(|d| d.code());
        assert_eq!(state.as_deref(), Some("XF001"), "{sql}: {err}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn an_update_of_an_append_only_table_is_append_only_violation() {
    let t = FreshDb::new().await;
    let err = sqlx::query("UPDATE forge_audit.bootstrap_log SET bootstrap_version = 99")
        .execute(&mut t.superuser().await)
        .await
        .map_err(|e| query_error("test.tamper", e))
        .expect_err("append-only");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "rejected");
    let text = err.to_string();
    assert!(text.contains("sqlstate=XF001"), "{text}");
    assert!(
        text.contains("forge_audit.bootstrap_log"),
        "names the table: {text}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_owner_role_cannot_attach_code_to_the_audit_objects() {
    // Regression for the review's CRITICAL: a trigger planted on a table the
    // superuser-owned event trigger writes to would run as superuser.
    let t = FreshDb::new().await;
    let mut mc = crate::Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator (forge_owner)");
    exec(
        &mut mc.conn,
        "CREATE FUNCTION forge.planted() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RETURN NEW; END $$",
    )
    .await;
    for sql in [
        "CREATE TRIGGER planted BEFORE INSERT ON forge_audit.ddl_audit \
         FOR EACH ROW EXECUTE FUNCTION forge.planted()",
        "ALTER TABLE forge_audit.ddl_audit DISABLE TRIGGER ALL",
        "DROP FUNCTION forge_audit.ddl_audit_record() CASCADE",
        "DROP TABLE forge_audit.bootstrap_log",
        "CREATE FUNCTION forge_audit.x() RETURNS int LANGUAGE sql AS 'select 1'",
    ] {
        let err = sqlx::raw_sql(sql)
            .execute(&mut mc.conn)
            .await
            .expect_err(sql);
        let state = err.as_database_error().and_then(|d| d.code());
        assert_eq!(state.as_deref(), Some("42501"), "{sql}: {err}");
    }
    mc.close().await;
    t.drop_now().await;
}

#[tokio::test]
async fn ddl_is_recorded_with_who_and_how() {
    let t = FreshDb::new().await;
    let mut mc = crate::Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator");
    exec(&mut mc.conn, "CREATE TABLE forge.ddl_probe (i int)").await;
    mc.close().await;
    let row: (String, String, String) = sqlx::query_as(
        "SELECT session_user_name, application_name, command_tag FROM forge_audit.ddl_audit \
         WHERE object_identity = 'forge.ddl_probe'",
    )
    .fetch_one(t.db.pool().executor())
    .await
    .expect("recorded");
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2.as_str()),
        ("forge_migrator", "forge-migrate", "CREATE TABLE")
    );
    t.drop_now().await;
}

/// Each tampering, made as superuser in a fresh database, and the guard
/// doctor reports for it.
#[tokio::test]
async fn every_weakened_object_guard_is_reported() {
    let _lock = ClusterLock::shared().await;
    for (tamper, guard) in [
        // ORIGIN only instead of ALWAYS: skipped under replication role.
        (
            "ALTER TABLE forge_audit.ddl_audit ENABLE TRIGGER ddl_audit_append_only",
            "guard=log_triggers",
        ),
        (
            "CREATE FUNCTION forge.noop() RETURNS trigger LANGUAGE plpgsql \
             AS $$ BEGIN RETURN NEW; END $$; \
             DROP TRIGGER ddl_audit_append_only ON forge_audit.ddl_audit; \
             CREATE TRIGGER ddl_audit_append_only BEFORE UPDATE OR DELETE ON forge_audit.ddl_audit \
             FOR EACH ROW EXECUTE FUNCTION forge.noop(); \
             ALTER TABLE forge_audit.ddl_audit ENABLE ALWAYS TRIGGER ddl_audit_append_only",
            "guard=log_triggers",
        ),
        (
            "CREATE TRIGGER extra AFTER UPDATE ON forge_audit.ddl_audit \
             FOR EACH ROW EXECUTE FUNCTION forge_audit.reject_log_mutation()",
            "guard=log_triggers",
        ),
        (
            "ALTER EVENT TRIGGER forge_ddl_audit_end DISABLE",
            "guard=event_triggers",
        ),
        (
            "CREATE OR REPLACE FUNCTION forge_audit.reject_log_mutation() RETURNS trigger \
             LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp \
             AS $$ BEGIN RETURN NEW; END $$",
            "guard=function_body",
        ),
        (
            "ALTER FUNCTION forge_audit.reject_log_mutation() OWNER TO forge_owner",
            "guard=function_owner",
        ),
        (
            "GRANT EXECUTE ON FUNCTION forge_audit.cluster_identity() TO forge_maint",
            "guard=function_privilege",
        ),
        (
            "ALTER TABLE forge_audit.guarded_objects OWNER TO forge_owner",
            "guard=object_owner",
        ),
        (
            "CREATE RULE r AS ON INSERT TO forge_audit.guarded_objects DO ALSO NOTIFY forge_t",
            "guard=object_owner",
        ),
        (
            "ALTER TABLE forge_audit.guarded_objects ENABLE ROW LEVEL SECURITY",
            "guard=object_owner",
        ),
        (
            "DROP TRIGGER ddl_audit_no_truncate ON forge_audit.ddl_audit",
            "guard=log_triggers",
        ),
        (
            "ALTER EVENT TRIGGER forge_ddl_audit_drop DISABLE",
            "guard=event_triggers",
        ),
        (
            "DROP EVENT TRIGGER forge_ddl_audit_drop; CREATE EVENT TRIGGER forge_ddl_audit_drop ON sql_drop WHEN TAG IN ('DROP TABLE') EXECUTE FUNCTION forge_audit.ddl_audit_record()",
            "guard=event_triggers",
        ),
        (
            "CREATE EVENT TRIGGER forge_t_extra ON ddl_command_start EXECUTE FUNCTION forge_audit.ddl_audit_record()",
            "guard=event_triggers",
        ),
        (
            "ALTER FUNCTION forge_audit.cluster_identity() OWNER TO forge_audit",
            "guard=function_owner",
        ),
        (
            "CREATE FUNCTION forge_audit.cluster_identity(i int) RETURNS int LANGUAGE sql AS 'select 1'",
            "guard=function_owner",
        ),
        (
            "REVOKE EXECUTE ON FUNCTION forge_audit.cluster_identity() FROM forge_app",
            "guard=function_privilege",
        ),
        (
            "GRANT EXECUTE ON FUNCTION forge_audit.ddl_audit_record() TO forge_app",
            "guard=function_privilege",
        ),
    ] {
        let t = FreshDb::new().await;
        exec(&mut t.superuser().await, tamper).await;
        let text = guard_error(&t, &[]).await;
        assert!(text.contains(guard), "{tamper}\n=> {text}");
        assert!(text.contains("FG-DB-0009"), "{text}");
        // Re-running step 3 repairs it (every trigger and function is
        // recreated) — except the owner of a table, which 03 also resets.
        exec(&mut t.superuser().await, &bootstrap_sql(2)).await;
        check(&t.db, &[])
            .await
            .unwrap_or_else(|e| panic!("{tamper}: not repaired by 03: {e}"));
        t.drop_now().await;
    }
}

/// Each cluster-wide tampering, its undo (run even when the test panics)
/// and the guard doctor reports.
#[tokio::test]
async fn an_elevated_role_or_an_extra_role_of_a_login_is_reported() {
    let _lock = ClusterLock::exclusive().await;
    let t = FreshDb::new().await;
    let hop = format!("forge_t_hop_{}", uuid::Uuid::new_v4().simple());
    let cases: Vec<(String, String, &str)> = vec![
        (
            "ALTER ROLE forge_maint CREATEDB".into(),
            "ALTER ROLE forge_maint NOCREATEDB".into(),
            "guard=role_attributes",
        ),
        (
            "ALTER ROLE forge_ro BYPASSRLS".into(),
            "ALTER ROLE forge_ro NOBYPASSRLS".into(),
            "guard=role_attributes",
        ),
        (
            "ALTER ROLE forge_owner LOGIN".into(),
            "ALTER ROLE forge_owner NOLOGIN".into(),
            "guard=role_attributes",
        ),
        (
            "GRANT pg_write_all_data TO forge_maint".into(),
            "REVOKE pg_write_all_data FROM forge_maint".into(),
            "guard=role_separation",
        ),
        (
            "GRANT forge_seq TO forge_maint".into(),
            "REVOKE forge_seq FROM forge_maint".into(),
            "guard=role_separation",
        ),
        // Through an intermediate role: forge_maint -> hop -> forge_purge (forge_app is left alone:
        // the other tests keep connecting as it).
        (
            format!(
                "CREATE ROLE \"{hop}\" NOLOGIN; GRANT forge_purge TO \"{hop}\"; \
                 GRANT \"{hop}\" TO forge_maint"
            ),
            format!("DROP ROLE IF EXISTS \"{hop}\""),
            "guard=role_separation",
        ),
        (
            "GRANT forge_owner TO forge_migrator WITH ADMIN OPTION".into(),
            "REVOKE ADMIN OPTION FOR forge_owner FROM forge_migrator".into(),
            "reason=role_membership_changed",
        ),
    ];
    for (tamper, undo, guard) in &cases {
        let _undo = Restore(vec![undo.clone()]); // clone: the guard owns its SQL
        exec(&mut superuser(&maintenance_db()).await, tamper).await;
        let text = check(&t.db, &[]).await.expect_err(tamper).to_string();
        assert!(text.contains(guard), "{tamper}\n=> {text}");
    }
    check(&t.db, &[]).await.expect("every change undone");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unlisted_privileged_member_is_role_membership_changed() {
    let _lock = ClusterLock::exclusive().await;
    let t = FreshDb::new().await;
    let role = format!("forge_t_dba_{}", uuid::Uuid::new_v4().simple());
    let _undo = Restore(vec![format!("DROP ROLE IF EXISTS \"{role}\"")]);
    let mut su = superuser(&maintenance_db()).await;
    exec(
        &mut su,
        &format!("CREATE ROLE \"{role}\" NOLOGIN; GRANT forge_owner TO \"{role}\""),
    )
    .await;
    let member = |r: &str| PrivilegedMember {
        role: r.to_owned(),
        member: role.clone(),
    };
    let text = check(&t.db, &[]).await.expect_err("unlisted").to_string();
    assert!(text.contains("reason=role_membership_changed"), "{text}");
    check(&t.db, &[member("forge_owner")])
        .await
        .expect("listed member is allowed");
    let text = check(&t.db, &[member("forge_seq")])
        .await
        .expect_err("listed for another role")
        .to_string();
    assert!(text.contains("role_membership_changed"), "per role: {text}");
    exec(&mut su, &format!("DROP ROLE \"{role}\"")).await;
    let text = check(&t.db, &[member("forge_owner")])
        .await
        .expect_err("configured but absent")
        .to_string();
    assert!(text.contains("role_membership_changed"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_role_not_created_by_the_bootstrap_stops_step_1() {
    let _lock = ClusterLock::exclusive().await;
    let _undo = Restore(vec![
        "COMMENT ON ROLE forge_ro IS 'forge bootstrap role'".to_owned()
    ]);
    let mut su = superuser(&maintenance_db()).await;
    exec(&mut su, "COMMENT ON ROLE forge_ro IS NULL").await;
    let result = sqlx::raw_sql(&bootstrap_sql(0)).execute(&mut su).await;
    let err = result.expect_err("a foreign role is not adopted");
    assert!(
        err.to_string()
            .contains("was not created by the forge bootstrap"),
        "{err}"
    );
}

#[tokio::test]
async fn a_stale_bootstrap_version_is_guard_missing() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    // A newer run records a higher version (rows are only ever appended).
    t.exec_superuser("INSERT INTO forge_audit.bootstrap_log (bootstrap_version) VALUES (999)")
        .await;
    let text = guard_error(&t, &[]).await;
    assert!(text.contains("guard=bootstrap_version"), "{text}");
    assert!(text.contains("found=999"), "{text}");
    assert!(
        text.contains(&format!("expected={BOOTSTRAP_VERSION}")),
        "{text}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_database_without_the_bootstrap_is_guard_missing() {
    let _lock = ClusterLock::shared().await;
    ensure_cluster().await;
    // forge_app on the maintenance database: roles exist, objects do not.
    let maintenance = dev_url();
    let maintenance = maintenance.rsplit_once('/').expect("db").1;
    let cfg = test_config(&url_as("forge_app", maintenance), APP_PASSWORD, &[]);
    let db = crate::Db::connect(&cfg.db).await.expect("forge_app");
    let err = check(&db, &[]).await.expect_err("not bootstrapped");
    assert!(err.to_string().contains("guard=bootstrap_version"), "{err}");
    db.close().await;
}

#[test]
fn membership_drift_reports_unexpected_admin_and_missing_members() {
    let row = |m: &str, r: &str| (m.to_owned(), r.to_owned(), false);
    let fixed = vec![
        row("forge_migrator", "forge_owner"),
        row("forge_owner", "forge_seq"),
        row("forge_owner", "forge_purge"),
        row("forge_app", "forge_ro"),
    ];
    let none: (Vec<String>, Vec<String>) = (vec![], vec![]);
    assert_eq!(membership_drift(&fixed, &[]), none);
    let alice = PrivilegedMember {
        role: "forge_owner".to_owned(),
        member: "dba_alice".to_owned(),
    };
    let mut extra = fixed.clone();
    extra.push(row("dba_alice", "forge_owner"));
    extra.push(row("dba_alice", "forge_purge"));
    extra.push(("forge_migrator".to_owned(), "forge_owner".to_owned(), true));
    let (unexpected, missing) = membership_drift(&extra, std::slice::from_ref(&alice));
    assert_eq!(
        unexpected,
        [
            "dba_alice->forge_purge",
            "forge_migrator->forge_owner(admin)"
        ]
    );
    assert!(missing.is_empty());
    let (_, missing) = membership_drift(&fixed[1..], &[alice]);
    assert_eq!(
        missing,
        ["forge_migrator->forge_owner", "dba_alice->forge_owner"]
    );
}

#[test]
fn the_shipped_function_bodies_and_version_are_found() {
    for (name, _) in FUNCTIONS {
        let body = shipped_body(name).expect(name);
        assert!(!body.trim().is_empty(), "{name}");
    }
    assert!(
        FILES[2]
            .1
            .contains(&format!("VALUES ({BOOTSTRAP_VERSION});")),
        "BOOTSTRAP_VERSION matches 03-database-objects.sql"
    );
    assert!(
        FILES[1].1.contains(":\"dbname\""),
        "02 takes the psql variable"
    );
    for (file, text) in FILES {
        assert!(
            text.contains("\\set ON_ERROR_STOP on"),
            "{file} stops psql at the first error"
        );
    }
}

#[test]
fn the_default_pools_fit_the_development_server() {
    let cfg =
        forge_core::config::load(None, &forge_core::config::Env::default()).expect("defaults");
    let budget = ConnectionBudget::new(&cfg.database, 197);
    assert_eq!(budget.needed, 43);
    assert!(budget.fits());
    assert!(
        !ConnectionBudget::new(&cfg.database, 53).fits(),
        "43 > 80 % of 53"
    );
    assert!(ConnectionBudget::new(&cfg.database, 54).fits());
}

#[tokio::test]
async fn too_many_connections_for_the_server_is_connection_budget() {
    let t = FreshDb::new().await;
    let usable: i32 = sqlx::query_scalar(
        "SELECT current_setting('max_connections')::int \
         - current_setting('superuser_reserved_connections')::int \
         - coalesce(current_setting('reserved_connections', true)::int, 0)",
    )
    .fetch_one(t.db.pool().executor())
    .await
    .expect("usable");
    // Half of the usable slots per pool: twice the server in total.
    let half = (usable / 2).to_string();
    let cfg = t.app_config(&[
        ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", half.as_str()),
        ("FORGE_DATABASE_READ_MAX_CONNECTIONS", half.as_str()),
    ]);
    let err = check_connections(&t.db, &cfg.db)
        .await
        .expect_err("over budget");
    assert_reason(&err, ErrorCode::ConfigInvalid, "connection_budget");
    assert!(err.to_string().contains("needed="), "{err}");
    let ok = check_connections(&t.db, &t.app_config(&[]).db)
        .await
        .expect("defaults fit");
    assert_eq!(ok.needed, 43);
    t.drop_now().await;
}

#[tokio::test]
async fn audit_schema_changes_fail_or_are_reported() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    // Moving the schema away from forge_audit breaks the DDL log's own
    // write, so the change itself fails.
    let err = sqlx::raw_sql("ALTER SCHEMA forge_audit OWNER TO forge_owner")
        .execute(&mut t.superuser().await)
        .await
        .expect_err("the DDL log cannot write any more");
    assert!(err.to_string().contains("permission denied"), "{err}");
    // forge_owner does not own the database: no extension, no schema, no
    // database defaults.
    let mut mc = crate::Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator (forge_owner)");
    for sql in [
        "CREATE EXTENSION citext SCHEMA forge_audit".to_owned(),
        "CREATE SCHEMA forge_dev".to_owned(),
        "CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'select 1'".to_owned(),
        format!("ALTER DATABASE \"{}\" SET role = forge_owner", t.name),
    ] {
        let err = sqlx::raw_sql(&sql)
            .execute(&mut mc.conn)
            .await
            .expect_err(&sql);
        let state = err.as_database_error().and_then(|d| d.code());
        assert_eq!(state.as_deref(), Some("42501"), "{sql}: {err}");
    }
    mc.close().await;
    // An extension a superuser installs there is reported.
    t.exec_superuser("CREATE EXTENSION citext SCHEMA forge_audit")
        .await;
    let text = guard_error(&t, &[]).await;
    assert!(text.contains("guard=function_owner"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn database_session_defaults_are_reported_and_reset() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    t.exec_superuser(&format!(
        "ALTER DATABASE \"{}\" SET search_path = public",
        t.name
    ))
    .await;
    let text = guard_error(&t, &[]).await;
    assert!(text.contains("guard=db_settings"), "{text}");
    exec(&mut t.superuser().await, &bootstrap_sql(2)).await;
    check(&t.db, &[])
        .await
        .expect("03 resets the database defaults");
    t.drop_now().await;
}

#[tokio::test]
async fn overloads_planted_in_forge_do_not_fool_the_checks() {
    let _lock = ClusterLock::shared().await;
    let t = FreshDb::new().await;
    // forge_owner (forge is on the read pool's search_path) plants an
    // always-true equality and an empty unnest, then a superuser re-points
    // a guard trigger.
    let mut mc = crate::Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator (forge_owner)");
    exec(
        &mut mc.conn,
        "CREATE FUNCTION forge.eq(oid, regprocedure) RETURNS boolean LANGUAGE sql \
         AS 'select true'; \
         CREATE OPERATOR forge.= (LEFTARG = oid, RIGHTARG = regprocedure, FUNCTION = forge.eq); \
         CREATE FUNCTION forge.unnest(text[]) RETURNS SETOF text LANGUAGE sql \
         AS 'select null::text where false'; \
         GRANT EXECUTE ON FUNCTION forge.eq(oid, regprocedure), forge.unnest(text[]) TO PUBLIC",
    )
    .await;
    mc.close().await;
    t.exec_superuser(
        "CREATE FUNCTION forge.noop() RETURNS trigger LANGUAGE plpgsql \
         AS $$ BEGIN RETURN NEW; END $$; \
         DROP TRIGGER ddl_audit_append_only ON forge_audit.ddl_audit; \
         CREATE TRIGGER ddl_audit_append_only BEFORE UPDATE OR DELETE ON forge_audit.ddl_audit \
         FOR EACH ROW EXECUTE FUNCTION forge.noop(); \
         ALTER TABLE forge_audit.ddl_audit ENABLE ALWAYS TRIGGER ddl_audit_append_only",
    )
    .await;
    let text = guard_error(&t, &[]).await;
    assert!(text.contains("guard=log_triggers"), "{text}");
    t.drop_now().await;
}
