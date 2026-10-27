//! Pool tests. Most need the development database (ADR-0011): they fail —
//! never skip — with the compose command when it is not running.

use std::time::{Duration, Instant};

use forge_core::config::TlsMode;
use forge_errors::{ErrorCode, Lang};

use super::*;
use crate::options::*;
use crate::test_support::*;
use crate::{query_error, read_query_error};

#[tokio::test]
async fn test_database_is_reachable_or_fails_with_compose_hint() {
    let (db, _cfg) = dev_db(&[]).await;
    db.ping().await.expect("ping");
}

#[tokio::test]
async fn connect_to_a_closed_port_is_db_unavailable_within_the_connect_timeout() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }; // closed again
    let url = format!("postgres://forge@127.0.0.1:{port}/forge");
    let cfg = test_config(&url, "x", &[("FORGE_DATABASE_CONNECT_TIMEOUT_MS", "3000")]);
    let started = Instant::now();
    let err = Db::connect(&cfg.db).await.expect_err("closed port");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
    let text = err.to_string();
    assert!(text.contains("reason=connect_refused"), "{text}");
    assert!(
        text.contains(&format!("target=127.0.0.1:{port}/forge")),
        "{text}"
    );
}

#[tokio::test]
async fn an_unresponsive_server_is_db_unavailable_connect_timeout() {
    // Accepts TCP (backlog) but never speaks the protocol.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = silent.local_addr().expect("addr").port();
    let url = format!("postgres://forge@127.0.0.1:{port}/forge");
    let cfg = test_config(&url, "x", &[("FORGE_DATABASE_CONNECT_TIMEOUT_MS", "500")]);
    let started = Instant::now();
    let err = Db::connect(&cfg.db).await.expect_err("silent server");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_reason(&err, ErrorCode::DbUnavailable, "connect_timeout");
    drop(silent);
}

#[tokio::test]
async fn a_wrong_password_is_auth_failed_and_never_logged() {
    let _ = dev_db(&[]).await; // precondition: the database is up
    let cfg = test_config(&dev_url(), "wrong-pw-7d1f", &[]);
    let err = Db::connect(&cfg.db).await.expect_err("bad password");
    assert_reason(&err, ErrorCode::DbUnavailable, "auth_failed");
    assert!(!chain(&err).contains("wrong-pw-7d1f"), "{}", chain(&err));
}

#[tokio::test]
async fn verify_full_against_a_server_without_tls_is_refused() {
    let _ = dev_db(&[]).await;
    let cfg = test_config(
        &dev_url(),
        "forge_dev",
        &[("FORGE_DATABASE_TLS_MODE", "verify-full")],
    );
    let err = Db::connect(&cfg.db).await.expect_err("no downgrade");
    assert_reason(&err, ErrorCode::DbUnavailable, "tls");
}

#[tokio::test]
async fn pool_exhaustion_is_db_unavailable_acquire_timeout() {
    // The pool is lazy: the first acquire also opens the connection (TCP +
    // SCRAM), so acquire_timeout must leave room for that under a loaded
    // parallel run; 300 ms was flaky. 1.5 s still proves exhaustion.
    let (db, _cfg) = dev_db(&[
        ("FORGE_DATABASE_READ_MAX_CONNECTIONS", "1"),
        ("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "1500"),
    ])
    .await;
    let held = db.acquire().await.expect("first connection");
    let err = db.acquire().await.expect_err("pool exhausted");
    assert_reason(&err, ErrorCode::DbUnavailable, "acquire_timeout");
    assert!(
        err.to_string()
            .contains("timeout_key=database.acquire_timeout_ms"),
        "{err}"
    );
    drop(held);
    db.acquire().await.expect("released connection is reusable");
}

#[tokio::test]
async fn a_slow_statement_is_db_query_failed_with_sqlstate_57014() {
    let (db, _cfg) = dev_db(&[("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "200")]).await;
    let err = sqlx::query("SELECT pg_sleep(5)")
        .execute(db.pool().executor())
        .await
        .map_err(|e| query_error("test.sleep", e))
        .expect_err("statement timeout");
    assert_reason(&err, ErrorCode::DbQueryFailed, "statement_timeout");
    let text = err.to_string();
    assert!(text.contains("sqlstate=57014"), "{text}");
    assert!(text.contains("query=test.sleep"), "{text}");
}

#[tokio::test]
async fn a_lock_wait_beyond_lock_timeout_is_db_query_failed() {
    let (db, _cfg) = dev_db(&[("FORGE_DATABASE_LOCK_TIMEOUT_MS", "200")]).await;
    let key = i64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().expect("8"));
    let mut holder = db.pool().executor().begin().await.expect("tx");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("first lock");
    let mut waiter = db.pool().executor().begin().await.expect("tx");
    let err = sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *waiter)
        .await
        .map_err(|e| query_error("test.lock", e))
        .expect_err("lock timeout");
    assert_reason(&err, ErrorCode::DbQueryFailed, "lock_timeout");
    assert!(err.to_string().contains("sqlstate=55P03"), "{err}");
}

#[tokio::test]
async fn query_errors_never_expose_sql_to_the_user_or_the_context() {
    let (db, _cfg) = dev_db(&[("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "100")]).await;
    let err = sqlx::query("SELECT pg_sleep(5) AS secret_column_name")
        .execute(db.pool().executor())
        .await
        .map_err(|e| query_error("test.sleep", e))
        .expect_err("timeout");
    for lang in [Lang::Ja, Lang::En, Lang::Id] {
        assert!(!err.user_message(lang).contains("pg_sleep"));
    }
    assert!(!err.to_string().contains("secret_column_name"), "{err}");
}

#[tokio::test]
async fn sessions_carry_the_configured_timeouts() {
    let (db, _cfg) = dev_db(&[
        ("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "15000"),
        ("FORGE_DATABASE_LOCK_TIMEOUT_MS", "5000"),
        ("FORGE_DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_MS", "60000"),
    ])
    .await;
    let mut conn = db.acquire().await.expect("conn");
    for (setting, expected) in [
        ("statement_timeout", "15s"),
        ("lock_timeout", "5s"),
        ("idle_in_transaction_session_timeout", "1min"),
        ("application_name", "forge"),
    ] {
        let value: String = sqlx::query_scalar(&format!("SHOW {setting}"))
            .fetch_one(&mut *conn)
            .await
            .expect("show");
        assert_eq!(value, expected, "{setting}");
    }
}

#[test]
fn server_versions_below_16_are_rejected() {
    assert_eq!(
        check_version("160004", MIN_SERVER_VERSION_NUM).expect("16"),
        160_004
    );
    assert_eq!(
        check_version("170000", MIN_SERVER_VERSION_NUM).expect("17"),
        170_000
    );
    let err = check_version("150008", MIN_SERVER_VERSION_NUM).expect_err("15");
    assert_reason(&err, ErrorCode::DbUnavailable, "version");
    assert!(
        err.to_string().contains("server_version_num=150008"),
        "{err}"
    );
    let err = check_version("banana", MIN_SERVER_VERSION_NUM).expect_err("garbage");
    assert_reason(&err, ErrorCode::DbUnavailable, "version");
}

#[tokio::test]
async fn connect_checks_the_server_version() {
    let _ = dev_db(&[]).await;
    let cfg = test_config(&dev_url(), "forge_dev", &[]);
    let err = Db::connect_requiring(&cfg.db, 999_999)
        .await
        .expect_err("too old for this minimum");
    assert_reason(&err, ErrorCode::DbUnavailable, "version");
    assert!(
        err.to_string().contains("target=127.0.0.1:55432/forge_dev"),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_database_is_server_rejected_3d000() {
    let _ = dev_db(&[]).await;
    let url = url_with_db("forge_does_not_exist");
    let cfg = test_config(&url, "forge_dev", &[]);
    let err = Db::connect(&cfg.db).await.expect_err("no such database");
    assert_reason(&err, ErrorCode::DbUnavailable, "server_rejected");
    assert!(err.to_string().contains("sqlstate=3D000"), "{err}");
}

#[tokio::test]
async fn a_closed_pool_is_unavailable_for_ping_and_acquire() {
    let (db, _cfg) = dev_db(&[]).await;
    db.close().await;
    for err in [
        db.ping().await.expect_err("closed"),
        db.acquire().await.map(|_| ()).expect_err("closed"),
    ] {
        assert_reason(&err, ErrorCode::DbUnavailable, "pool_closed");
        assert!(err.to_string().contains("target="), "{err}");
    }
}

#[tokio::test]
async fn a_terminated_backend_is_unavailable_not_a_query_failure() {
    let (db, _cfg) = dev_db(&[]).await;
    // The write pool: the read pool runs as forge_ro, read-only.
    let mut conn = db.write_pool().acquire().await.expect("conn");
    let err = sqlx::query("SELECT pg_terminate_backend(pg_backend_pid())")
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("test.terminate", e))
        .expect_err("terminated");
    assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
    // The next statement on the dead connection is a lost link.
    let err = sqlx::query("SELECT 1")
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("test.after", e))
        .expect_err("dead connection");
    assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
}

#[tokio::test]
async fn a_constraint_violation_never_logs_the_row_values() {
    // forge_app has no TEMP: the table is created by the superuser.
    let t = FreshDb::new().await;
    sqlx::raw_sql(
        "CREATE TABLE forge.t_unique (email text PRIMARY KEY); \
         GRANT INSERT ON forge.t_unique TO forge_app",
    )
    .execute(&mut t.superuser().await)
    .await
    .expect("table");
    // The write pool: the read pool runs as forge_ro, read-only.
    let mut conn = t.db.write_pool().acquire().await.expect("conn");
    let mut failure = None;
    for _ in 0..2 {
        let r = sqlx::query("INSERT INTO forge.t_unique VALUES ($1)")
            .bind("pii-a1b2@example.invalid")
            .execute(&mut *conn)
            .await;
        if let Err(e) = r {
            failure = Some(query_error("test.insert", e));
            break;
        }
    }
    drop(conn);
    t.drop_now().await;
    let err = failure.expect("the duplicate insert failed");
    assert_reason(&err, ErrorCode::DbQueryFailed, "constraint");
    let text = chain(&err);
    assert!(text.contains("sqlstate=23505"), "{text}");
    assert!(text.contains("constraint=t_unique_pkey"), "{text}");
    assert!(!text.contains("pii-a1b2"), "row value leaked: {text}");
}

#[test]
fn a_libpq_environment_variable_is_a_configuration_error() {
    let err =
        reject_libpq_env(["PATH".to_owned(), "PGSSLROOTCERT".to_owned()]).expect_err("libpq env");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    let text = err.to_string();
    assert!(text.contains("config_key=PGSSLROOTCERT"), "{text}");
    assert!(text.contains("reason=libpq_env"), "{text}");
    let err = reject_libpq_env(["pgpassword".to_owned()]).expect_err("case-insensitive");
    assert!(err.to_string().contains("config_key=pgpassword"), "{err}");
    assert!(reject_libpq_env(["PATH".to_owned(), "PGP_HOME".to_owned()]).is_ok());
}

#[test]
fn loopback_hosts_are_recognised() {
    for host in ["localhost", "LOCALHOST", "127.0.0.1", "::1", "[::1]"] {
        assert!(is_loopback(host), "{host}");
    }
    for host in ["10.0.0.1", "db.example.invalid", "0.0.0.0"] {
        assert!(!is_loopback(host), "{host}");
    }
}

#[test]
fn an_unverified_remote_database_needs_an_explicit_allowance() {
    for mode in [TlsMode::Require, TlsMode::Disable] {
        let err = check_unverified(mode, false, "db.internal", "db.internal:5432/forge")
            .expect_err("remote, not allowed");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(
            err.to_string().contains("reason=unverified_remote"),
            "{err}"
        );
        assert!(
            check_unverified(mode, true, "db.internal", "t").is_ok(),
            "allowed"
        );
        assert!(
            check_unverified(mode, false, "127.0.0.1", "t").is_ok(),
            "loopback"
        );
    }
    assert!(check_unverified(TlsMode::VerifyFull, false, "db.internal", "t").is_ok());
}

#[tokio::test]
async fn read_pool_sessions_run_as_forge_ro_and_read_only() {
    let t = FreshDb::new().await;
    let (user, read_only): (String, String) =
        sqlx::query_as("SELECT current_user::text, current_setting('transaction_read_only')")
            .fetch_one(t.db.pool().executor())
            .await
            .expect("read session");
    assert_eq!((user.as_str(), read_only.as_str()), ("forge_ro", "on"));
    let user: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(t.db.write_pool())
        .await
        .expect("write session");
    assert_eq!(user, "forge_app");
    t.drop_now().await;
}

#[tokio::test]
async fn read_pool_write_is_write_on_read_pool_not_unavailable() {
    let t = FreshDb::new().await;
    let err = sqlx::query("CREATE TABLE forge.read_pool_write (i int)")
        .execute(t.db.pool().executor())
        .await
        .map_err(|e| read_query_error("test.read_write", e))
        .expect_err("read-only session");
    assert_reason(&err, ErrorCode::DbQueryFailed, "write_on_read_pool");
    assert!(err.to_string().contains("sqlstate=25006"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_missing_privilege_is_permission_denied() {
    let t = FreshDb::new().await;
    // forge_app has no INSERT on the DDL acknowledgements (forge_maint only).
    let err =
        sqlx::query("INSERT INTO forge_audit.ddl_audit_ack (ddl_audit_id, note) VALUES (1, 'x')")
            .execute(t.db.write_pool())
            .await
            .map_err(|e| query_error("test.ack", e))
            .expect_err("not granted");
    assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    assert!(err.to_string().contains("sqlstate=42501"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_login_outside_forge_ro_is_config_invalid_wrong_role() {
    let t = FreshDb::new().await;
    let role = format!("forge_t_login_{}", uuid::Uuid::new_v4().simple());
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!(
        "CREATE ROLE \"{role}\" LOGIN PASSWORD 'x-login-pw'; \
         GRANT CONNECT ON DATABASE \"{}\" TO \"{role}\"",
        t.name
    ))
    .execute(&mut su)
    .await
    .expect("temporary login");
    let cfg = test_config(&url_as(&role, &t.name), "x-login-pw", &[]);
    let err = Db::connect(&cfg.db)
        .await
        .expect_err("not a member of forge_ro");
    assert_reason(&err, ErrorCode::ConfigInvalid, "wrong_role");
    assert!(err.to_string().contains("role_check=not_member"), "{err}");
    t.drop_now().await;
    sqlx::query(&format!("DROP ROLE \"{role}\""))
        .execute(&mut superuser(dev_url().rsplit_once('/').expect("db").1).await)
        .await
        .expect("drop temporary login");
}

#[tokio::test]
async fn a_superuser_application_login_is_refused() {
    let t = FreshDb::new().await;
    let cfg = test_config(&url_with_db(&t.name), "forge_dev", &[]);
    let err = Db::connect(&cfg.db).await.expect_err("superuser as app");
    assert_reason(&err, ErrorCode::ConfigInvalid, "wrong_role");
    let text = err.to_string();
    assert!(text.contains("config_key=database.url"), "{text}");
    assert!(text.contains("role_check=superuser"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn an_application_login_that_can_reach_forge_owner_is_refused() {
    // A temporary member of forge_owner is a cluster-wide change.
    let _lock = ClusterLock::exclusive().await;
    let t = FreshDb::new().await;
    let role = format!("forge_t_login_{}", uuid::Uuid::new_v4().simple());
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!(
        "CREATE ROLE \"{role}\" LOGIN PASSWORD 'x-login-pw'; \
         GRANT CONNECT ON DATABASE \"{}\" TO \"{role}\"; \
         GRANT forge_ro TO \"{role}\"; GRANT forge_owner TO \"{role}\"",
        t.name
    ))
    .execute(&mut su)
    .await
    .expect("temporary privileged login");
    let cfg = test_config(&url_as(&role, &t.name), "x-login-pw", &[]);
    let result = Db::connect(&cfg.db).await;
    t.drop_now().await;
    sqlx::query(&format!("DROP ROLE \"{role}\""))
        .execute(&mut superuser(&maintenance_db()).await)
        .await
        .expect("drop temporary login");
    let err = result.expect_err("privileged login");
    assert_reason(&err, ErrorCode::ConfigInvalid, "wrong_role");
    assert!(err.to_string().contains("role_check=privileged"), "{err}");
}
