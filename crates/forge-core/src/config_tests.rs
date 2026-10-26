use std::io::Write as _;
use std::time::Duration;

use forge_errors::ErrorCode;

use super::*;

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().copied())
}

fn file(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("temp file");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn assert_invalid(err: &ForgeError, key: &str, reason: &str) {
    assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{err}");
    let text = err.to_string();
    assert!(text.contains(&format!("config_key={key}")), "{text}");
    assert!(text.contains(&format!("reason={reason}")), "{text}");
}

#[test]
fn defaults_apply_without_file_or_env() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.server.bind, "127.0.0.1:8080");
    assert!(cfg.storage.root.is_none());
    let db = &cfg.database;
    assert_eq!(db.max_connections, 10);
    assert_eq!(db.min_connections, 0);
    assert_eq!(db.connect_timeout, Duration::from_millis(5000));
    assert_eq!(db.acquire_timeout, Duration::from_millis(3000));
    assert_eq!(db.statement_timeout, Duration::from_millis(15000));
    assert_eq!(db.lock_timeout, Duration::from_millis(5000));
    assert_eq!(db.idle_in_transaction_timeout, Duration::from_millis(60000));
    assert_eq!(db.migrate_statement_timeout, None);
}

#[test]
fn config_file_values_are_overridden_by_env() {
    let f = file(
        r#"
[server]
bind = "0.0.0.0:9000"
[database]
url = "postgres://forge@db.internal:5432/forge"
max_connections = 20
"#,
    );
    let cfg = load(
        Some(f.path()),
        &env(&[
            ("FORGE_SERVER_BIND", "127.0.0.1:9001"),
            ("FORGE_DATABASE_MAX_CONNECTIONS", "30"),
        ]),
    )
    .expect("loads");
    assert_eq!(cfg.server.bind, "127.0.0.1:9001");
    assert_eq!(cfg.database.max_connections, 30);
    let url = cfg.database.url().expect("url");
    assert_eq!(url.host(), "db.internal");
    assert_eq!(url.port(), 5432);
    assert_eq!(url.dbname(), "forge");
}

#[test]
fn missing_database_url_is_config_invalid_with_config_key() {
    let cfg = load(None, &env(&[])).expect("loads");
    let err = cfg.database.url().expect_err("no url");
    assert_invalid(&err, "database.url", "missing");
}

#[test]
fn missing_storage_root_is_config_invalid_with_config_key() {
    let cfg = load(None, &env(&[])).expect("loads");
    let err = cfg.storage_root().expect_err("no root");
    assert_invalid(&err, "storage.root", "missing");
}

#[test]
fn database_url_must_be_postgres_with_host_and_dbname() {
    for bad in [
        "mysql://forge@db/forge",
        "postgres:///forge",
        "postgres://db.internal",
        "not a url",
    ] {
        let err = load(None, &env(&[("FORGE_DATABASE_URL", bad)])).expect_err(bad);
        assert_invalid(&err, "database.url", "invalid");
        assert!(
            !err.to_string().contains(bad),
            "the raw url is not echoed: {err}"
        );
    }
}

#[test]
fn database_url_password_is_never_in_debug_or_display() {
    let cfg = load(
        None,
        &env(&[(
            "FORGE_DATABASE_URL",
            "postgres://forge:hunter2@db:5432/forge",
        )]),
    )
    .expect("loads");
    let url = cfg.database.url().expect("url");
    assert!(!format!("{url}").contains("hunter2"));
    assert!(!format!("{url:?}").contains("hunter2"));
    assert!(!format!("{cfg:?}").contains("hunter2"));
    assert_eq!(
        cfg.database.password.as_ref().map(Secret::expose),
        Some("hunter2")
    );
    assert!(
        !url.without_password().contains("hunter2"),
        "the url handed to the driver carries no password"
    );
}

#[test]
fn password_file_is_read_and_trailing_newline_trimmed() {
    let secret = file("s3cret-value\r\n");
    let path = secret.path().to_string_lossy().into_owned();
    let cfg = load(
        None,
        &env(&[
            ("FORGE_DATABASE_URL", "postgres://forge@db/forge"),
            ("FORGE_DATABASE_PASSWORD_FILE", &path),
        ]),
    )
    .expect("loads");
    assert_eq!(
        cfg.database.password.as_ref().map(Secret::expose),
        Some("s3cret-value")
    );
    assert!(!format!("{cfg:?}").contains("s3cret-value"));
}

#[test]
fn password_in_url_and_password_file_is_config_invalid() {
    let secret = file("other");
    let path = secret.path().to_string_lossy().into_owned();
    let err = load(
        None,
        &env(&[
            ("FORGE_DATABASE_URL", "postgres://forge:inline@db/forge"),
            ("FORGE_DATABASE_PASSWORD_FILE", &path),
        ]),
    )
    .expect_err("two passwords");
    assert_invalid(&err, "database.password_file", "password_twice");
}

#[test]
fn unreadable_password_file_is_config_invalid() {
    let dir = tempfile::tempdir().expect("dir");
    let missing = dir.path().join("nope").to_string_lossy().into_owned();
    let err = load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &missing)])).expect_err("missing");
    assert_invalid(&err, "database.password_file", "unreadable");
}

#[test]
fn empty_password_file_is_config_invalid() {
    let secret = file("\n");
    let path = secret.path().to_string_lossy().into_owned();
    let err = load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &path)])).expect_err("empty");
    assert_invalid(&err, "database.password_file", "empty");
}

#[test]
fn missing_config_file_is_config_invalid() {
    let dir = tempfile::tempdir().expect("dir");
    let err = load(Some(&dir.path().join("forge.toml")), &env(&[])).expect_err("missing");
    assert_invalid(&err, "config_file", "unreadable");
}

#[test]
fn unknown_key_in_file_is_config_invalid() {
    let f = file("[database]\nmax_conections = 5\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("typo");
    assert_invalid(&err, "database.max_conections", "unknown_key");
}

#[test]
fn malformed_config_file_is_config_invalid() {
    let f = file("[database\nurl = ");
    let err = load(Some(f.path()), &env(&[])).expect_err("syntax");
    assert_invalid(&err, "config_file", "parse");
}

#[test]
fn wrong_value_type_in_file_is_config_invalid() {
    let f = file("[database]\nmax_connections = true\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("bool");
    assert_invalid(&err, "database.max_connections", "invalid");
}

#[test]
fn unknown_env_in_a_config_namespace_is_config_invalid() {
    let err = load(
        None,
        &env(&[("FORGE_DATABSE_URL", "x"), ("FORGE_DATABASE_URLL", "x")]),
    )
    .expect_err("typo");
    assert_invalid(&err, "FORGE_DATABASE_URLL", "unknown_env");
}

#[test]
fn foreign_forge_env_vars_are_ignored() {
    // Hooks run with FORGE_REQUEST_ID / FORGE_USER_ID; they are not config.
    let cfg = load(
        None,
        &env(&[("FORGE_REQUEST_ID", "r"), ("FORGE_USER_ID", "1")]),
    );
    assert!(cfg.is_ok());
}

#[test]
fn non_numeric_env_value_is_config_invalid() {
    let err = load(None, &env(&[("FORGE_DATABASE_MAX_CONNECTIONS", "ten")])).expect_err("nan");
    assert_invalid(&err, "database.max_connections", "invalid");
}

#[test]
fn zero_timeouts_and_connections_are_rejected() {
    for (var, key) in [
        (
            "FORGE_DATABASE_CONNECT_TIMEOUT_MS",
            "database.connect_timeout_ms",
        ),
        (
            "FORGE_DATABASE_ACQUIRE_TIMEOUT_MS",
            "database.acquire_timeout_ms",
        ),
        (
            "FORGE_DATABASE_STATEMENT_TIMEOUT_MS",
            "database.statement_timeout_ms",
        ),
        ("FORGE_DATABASE_LOCK_TIMEOUT_MS", "database.lock_timeout_ms"),
        (
            "FORGE_DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_MS",
            "database.idle_in_transaction_timeout_ms",
        ),
        ("FORGE_DATABASE_MAX_CONNECTIONS", "database.max_connections"),
    ] {
        let err = load(None, &env(&[(var, "0")])).expect_err(var);
        assert_invalid(&err, key, "zero");
    }
    let err = load(None, &env(&[("FORGE_DATABASE_LOCK_TIMEOUT_MS", "-5")])).expect_err("negative");
    assert_invalid(&err, "database.lock_timeout_ms", "invalid");
}

#[test]
fn min_connections_above_max_is_config_invalid() {
    let err = load(
        None,
        &env(&[
            ("FORGE_DATABASE_MIN_CONNECTIONS", "5"),
            ("FORGE_DATABASE_MAX_CONNECTIONS", "2"),
        ]),
    )
    .expect_err("min > max");
    assert_invalid(&err, "database.min_connections", "above_max");
}

#[test]
fn migrate_statement_timeout_zero_means_unbounded() {
    let cfg = load(
        None,
        &env(&[("FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS", "0")]),
    )
    .expect("loads");
    assert_eq!(cfg.database.migrate_statement_timeout, None);
    let cfg = load(
        None,
        &env(&[("FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS", "600000")]),
    )
    .expect("loads");
    assert_eq!(
        cfg.database.migrate_statement_timeout,
        Some(Duration::from_millis(600_000))
    );
}

#[test]
fn every_key_has_a_derived_env_name() {
    for key in KEYS {
        assert_eq!(key.env, env_name(key.key), "{}", key.key);
    }
}
