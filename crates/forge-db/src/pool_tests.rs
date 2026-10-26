//! Pool tests. Most need the development database (ADR-0011): they fail —
//! never skip — with the compose command when it is not running.

use std::time::{Duration, Instant};

use forge_core::config::TlsMode;
use forge_errors::{ErrorCode, Lang};

use super::*;
use crate::query_error;
use crate::test_support::*;

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
    let (db, _cfg) = dev_db(&[
        ("FORGE_DATABASE_MAX_CONNECTIONS", "1"),
        ("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "300"),
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
        .execute(db.pool())
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
    let mut holder = db.pool().begin().await.expect("tx");
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *holder)
        .await
        .expect("first lock");
    let mut waiter = db.pool().begin().await.expect("tx");
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
        .execute(db.pool())
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
    db.pool().close().await;
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
    let mut conn = db.acquire().await.expect("conn");
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
    let (db, _cfg) = dev_db(&[]).await;
    let mut conn = db.acquire().await.expect("conn");
    sqlx::query("CREATE TEMP TABLE t_unique (email text PRIMARY KEY)")
        .execute(&mut *conn)
        .await
        .expect("temp table");
    for _ in 0..2 {
        let r = sqlx::query("INSERT INTO t_unique VALUES ($1)")
            .bind("pii-a1b2@example.invalid")
            .execute(&mut *conn)
            .await;
        if let Err(e) = r {
            let err = query_error("test.insert", e);
            assert_reason(&err, ErrorCode::DbQueryFailed, "constraint");
            let text = chain(&err);
            assert!(text.contains("sqlstate=23505"), "{text}");
            assert!(text.contains("constraint=t_unique_pkey"), "{text}");
            assert!(!text.contains("pii-a1b2"), "row value leaked: {text}");
            return;
        }
    }
    panic!("the duplicate insert succeeded");
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
