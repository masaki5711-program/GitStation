//! Bootstrap tests: the real files of ops/db/bootstrap/ in fresh databases.

use forge_errors::ErrorCode;

use super::*;
use crate::query_error;
use crate::test_support::*;

#[tokio::test]
async fn a_fresh_bootstrap_passes_every_guard() {
    let t = FreshDb::new().await;
    assert_eq!(check(&t.db, &[]).await.expect("guards"), BOOTSTRAP_VERSION);
    t.drop_now().await;
}

#[tokio::test]
async fn cluster_bootstrap_is_idempotent_and_records_each_run() {
    let t = FreshDb::new().await;
    let mut su = t.superuser().await;
    sqlx::raw_sql(FILES[0].1)
        .execute(&mut su)
        .await
        .expect("step 1 again");
    sqlx::raw_sql(FILES[2].1)
        .execute(&mut su)
        .await
        .expect("step 3 again");
    let runs: Vec<(i32, String)> =
        sqlx::query_as("SELECT bootstrap_version, txid::text FROM forge.bootstrap_log ORDER BY id")
            .fetch_all(&mut su)
            .await
            .expect("log");
    assert_eq!(runs.len(), 2, "{runs:?}");
    // The re-run's DDL is in the log under the re-run's txid: expected.
    let own: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.ddl_audit WHERE txid::text = $1")
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
        "UPDATE forge.ddl_audit SET event = 'x'",
        "DELETE FROM forge.ddl_audit",
        "TRUNCATE forge.bootstrap_log",
        "UPDATE forge.guarded_objects SET object_type = 'x'",
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
    let err = sqlx::query("UPDATE forge.bootstrap_log SET bootstrap_version = 99")
        .execute(&mut t.superuser().await)
        .await
        .map_err(|e| query_error("test.tamper", e))
        .expect_err("append-only");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "rejected");
    let text = err.to_string();
    assert!(text.contains("sqlstate=XF001"), "{text}");
    assert!(
        text.contains("forge.bootstrap_log"),
        "names the table: {text}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn ddl_is_recorded_with_who_and_how() {
    let t = FreshDb::new().await;
    let mut mc = crate::Db::connect_migrator(&t.migrate_config(&[]).db)
        .await
        .expect("migrator");
    sqlx::query("CREATE TABLE forge.ddl_probe (i int)")
        .execute(&mut mc.conn)
        .await
        .expect("ddl");
    mc.close().await;
    let row: (String, String, String) = sqlx::query_as(
        "SELECT session_user_name, application_name, command_tag FROM forge.ddl_audit \
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

#[tokio::test]
async fn a_disabled_log_trigger_is_guard_missing() {
    let t = FreshDb::new().await;
    // ORIGIN only instead of ALWAYS: skipped under replication role.
    sqlx::query("ALTER TABLE forge.ddl_audit ENABLE TRIGGER ddl_audit_append_only")
        .execute(&mut t.superuser().await)
        .await
        .expect("weaken");
    let err = check(&t.db, &[]).await.expect_err("weakened");
    assert_reason(&err, ErrorCode::AppendOnlyViolation, "guard_missing");
    assert!(err.to_string().contains("guard=log_triggers"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_replaced_function_body_is_guard_missing() {
    let t = FreshDb::new().await;
    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION forge.reject_log_mutation() RETURNS trigger \
         LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$ BEGIN RETURN NEW; END $$",
    )
    .execute(&mut t.superuser().await)
    .await
    .expect("replace");
    let err = check(&t.db, &[]).await.expect_err("replaced");
    assert!(err.to_string().contains("guard=function_body"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn an_extra_execute_grant_on_cluster_identity_is_guard_missing() {
    let t = FreshDb::new().await;
    sqlx::query("GRANT EXECUTE ON FUNCTION forge.cluster_identity() TO forge_maint")
        .execute(&mut t.superuser().await)
        .await
        .expect("grant");
    let err = check(&t.db, &[]).await.expect_err("over-granted");
    assert!(
        err.to_string().contains("guard=function_privilege"),
        "{err}"
    );
    assert!(err.to_string().contains("role=forge_maint"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_superuser_application_login_is_guard_missing() {
    let t = FreshDb::new().await;
    let cfg = test_config(&url_with_db(&t.name), "forge_dev", &[]);
    let su = crate::Db::connect(&cfg.db).await.expect("superuser as app");
    let err = check(&su, &[]).await.expect_err("superuser login");
    assert!(err.to_string().contains("guard=app_login"), "{err}");
    su.close().await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_database_without_the_bootstrap_is_guard_missing() {
    let (db, _cfg) = dev_db(&[]).await;
    let err = check(&db, &[]).await.expect_err("maintenance db");
    assert!(err.to_string().contains("guard=bootstrap_version"), "{err}");
}

#[test]
fn membership_drift_reports_unexpected_and_missing_members() {
    let row = |m: &str, r: &str| (m.to_owned(), r.to_owned());
    let fixed = vec![
        row("forge_migrator", "forge_owner"),
        row("forge_owner", "forge_seq"),
        row("forge_owner", "forge_purge"),
    ];
    let none: (Vec<String>, Vec<String>) = (vec![], vec![]);
    assert_eq!(membership_drift(&fixed, &[]), none);
    let mut extra = fixed.clone();
    extra.push(row("dba_alice", "forge_owner"));
    extra.push(row("forge_app", "forge_seq"));
    let (unexpected, missing) = membership_drift(&extra, &["dba_alice".to_owned()]);
    assert_eq!(unexpected, ["forge_app"]);
    assert!(missing.is_empty());
    let (_, missing) = membership_drift(&fixed[1..], &["dba_bob".to_owned()]);
    assert_eq!(missing, ["forge_migrator->forge_owner", "dba_bob"]);
}

#[test]
fn the_shipped_function_bodies_and_version_are_found() {
    for name in FUNCTIONS {
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
    let cfg = t.app_config(&[
        ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", "60"),
        ("FORGE_DATABASE_READ_MAX_CONNECTIONS", "40"),
    ]);
    let err = check_connections(&t.db, &cfg.db)
        .await
        .expect_err("203 > 80 %");
    assert_reason(&err, ErrorCode::ConfigInvalid, "connection_budget");
    assert!(err.to_string().contains("needed=203"), "{err}");
    let ok = check_connections(&t.db, &t.app_config(&[]).db)
        .await
        .expect("defaults fit");
    assert_eq!(ok.needed, 43);
    t.drop_now().await;
}
