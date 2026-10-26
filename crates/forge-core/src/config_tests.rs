use std::io::Write as _;
use std::path::Path;
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
    assert_eq!(db.migrate_statement_timeout, Duration::from_millis(600_000));
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
fn a_password_in_the_database_url_is_rejected() {
    for raw in [
        "postgres://forge:hunter2@db:5432/forge",
        "postgres://forge@db/forge?password=hunter2",
        "postgres://forge@db/forge?connect_timeout=5&PassWord=hunter2",
        "postgres://forge@db/forge?passfile=/tmp/pgpass",
        "postgres://forge@db/forge?sslpassword=hunter2",
    ] {
        let err = load(None, &env(&[("FORGE_DATABASE_URL", raw)])).expect_err(raw);
        assert_invalid(&err, "database.url", "password_in_url");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }
}

#[test]
fn an_oversized_or_non_file_password_path_is_config_invalid() {
    let big = file(&"x".repeat(5000));
    let path = big.path().to_string_lossy().into_owned();
    let err = load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &path)])).expect_err("big");
    assert_invalid(&err, "database.password_file", "invalid");
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().to_string_lossy().into_owned();
    let err = load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &path)])).expect_err("dir");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{err}");
}

#[test]
fn database_url_display_shows_host_port_and_dbname_only() {
    let cfg = load(
        None,
        &env(&[(
            "FORGE_DATABASE_URL",
            "postgres://forge%2Dapp@db:6543/forge%2Dprod",
        )]),
    )
    .expect("loads");
    let url = cfg.database.url().expect("url");
    assert_eq!(url.dbname(), "forge-prod", "percent-decoded");
    assert_eq!(url.user(), "forge-app", "percent-decoded");
    assert_eq!(url.host(), "db");
    assert_eq!(url.port(), 6543);
    assert_eq!(format!("{url}"), "db:6543/forge-prod");
    assert_eq!(format!("{url:?}"), "DatabaseUrl(db:6543/forge-prod)");
}

#[test]
fn the_database_user_is_required_and_ipv6_hosts_lose_their_brackets() {
    let err = load(None, &env(&[("FORGE_DATABASE_URL", "postgres://db/forge")])).expect_err("user");
    assert_invalid(&err, "database.url", "invalid");
    let cfg = load(
        None,
        &env(&[("FORGE_DATABASE_URL", "postgres://forge@[::1]:5433/forge")]),
    )
    .expect("ipv6");
    assert_eq!(cfg.database.url().expect("url").host(), "::1");
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
fn a_relative_password_file_is_config_invalid() {
    let err = load(
        None,
        &env(&[("FORGE_DATABASE_PASSWORD_FILE", "secrets/db")]),
    )
    .expect_err("relative");
    assert_invalid(&err, "database.password_file", "not_absolute");
}

#[cfg(unix)]
#[test]
fn a_password_file_readable_by_others_is_config_invalid() {
    use std::os::unix::fs::PermissionsExt as _;
    let secret = file("s3cret-value\n");
    let path = secret.path().to_string_lossy().into_owned();
    for mode in [0o644, 0o640, 0o604] {
        std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(mode))
            .expect("chmod");
        let err = load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &path)])).expect_err("mode");
        assert_invalid(&err, "database.password_file", "insecure_permissions");
    }
    std::fs::set_permissions(secret.path(), std::fs::Permissions::from_mode(0o400)).expect("chmod");
    assert!(load(None, &env(&[("FORGE_DATABASE_PASSWORD_FILE", &path)])).is_ok());
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
fn a_syntax_error_reports_its_position_but_never_the_line() {
    let f = file("[database]\nurl = \"postgres://forge:Pa55word@db/forge\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("unclosed string");
    assert_invalid(&err, "config_file", "parse");
    let text = err.to_string();
    assert!(text.contains("line=2"), "{text}");
    assert!(!text.contains("Pa55word"), "{text}");
    assert!(
        std::error::Error::source(&err).is_none(),
        "the TOML error (which quotes the line) is not attached"
    );
}

#[test]
fn wrong_value_type_in_file_is_config_invalid() {
    for (body, key) in [
        (
            "[database]\nmax_connections = true\n",
            "database.max_connections",
        ),
        (
            "[database]\nmax_connections = 1.5\n",
            "database.max_connections",
        ),
        (
            "[database]\nmax_connections = 4294967296\n",
            "database.max_connections",
        ),
        (
            "[database]\nmax_connections = [1]\n",
            "database.max_connections",
        ),
        (
            "[database]\npassword_file = 1234\n",
            "database.password_file",
        ),
        ("[server]\nbind = 8080\n", "server.bind"),
    ] {
        let f = file(body);
        let err = load(Some(f.path()), &env(&[])).expect_err(body);
        assert_invalid(&err, key, "invalid");
    }
}

#[test]
fn a_top_level_value_in_file_is_an_unknown_key() {
    let f = file("bind = \"127.0.0.1:1\"\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("top level");
    assert_invalid(&err, "bind", "unknown_key");
}

#[test]
fn the_renamed_forge_bind_is_rejected_with_its_new_name() {
    let err = load(None, &env(&[("FORGE_BIND", "0.0.0.0:80")])).expect_err("renamed");
    assert_invalid(&err, "FORGE_BIND", "renamed");
    assert!(
        err.to_string().contains("renamed_to=FORGE_SERVER_BIND"),
        "{err}"
    );
}

#[test]
fn empty_values_are_config_invalid_not_unset() {
    for (var, key) in [
        ("FORGE_SERVER_BIND", "server.bind"),
        ("FORGE_STORAGE_ROOT", "storage.root"),
        ("FORGE_DATABASE_URL", "database.url"),
        ("FORGE_DATABASE_PASSWORD_FILE", "database.password_file"),
    ] {
        let err = load(None, &env(&[(var, " ")])).expect_err(var);
        assert_invalid(&err, key, "empty");
    }
}

#[test]
fn bind_must_be_host_and_port() {
    for bad in [
        "8080",
        "localhost",
        ":8080",
        "127.0.0.1:http",
        "127.0.0.1:70000",
    ] {
        let err = load(None, &env(&[("FORGE_SERVER_BIND", bad)])).expect_err(bad);
        assert_invalid(&err, "server.bind", "invalid");
    }
    for good in ["0.0.0.0:80", "[::1]:8080", "forge.internal:8443"] {
        assert!(
            load(None, &env(&[("FORGE_SERVER_BIND", good)])).is_ok(),
            "{good}"
        );
    }
}

#[test]
fn env_debug_shows_names_only() {
    let e = env(&[("FORGE_DATABASE_URL", "postgres://forge@secret-host/forge")]);
    let shown = format!("{e:?}");
    assert!(shown.contains("FORGE_DATABASE_URL"), "{shown}");
    assert!(!shown.contains("secret-host"), "{shown}");
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
        (
            "FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS",
            "database.migrate_statement_timeout_ms",
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
fn migrate_statement_timeout_is_long_but_bounded() {
    let cfg = load(
        None,
        &env(&[("FORGE_DATABASE_MIGRATE_STATEMENT_TIMEOUT_MS", "3600000")]),
    )
    .expect("loads");
    assert_eq!(
        cfg.database.migrate_statement_timeout,
        Duration::from_millis(3_600_000)
    );
}

#[test]
fn every_key_has_a_derived_env_name() {
    for key in KEYS {
        assert_eq!(key.env, env_name(key.key), "{}", key.key);
    }
}

#[test]
fn tls_defaults_to_verify_full_and_accepts_explicit_modes() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.database.tls_mode, TlsMode::VerifyFull);
    assert!(cfg.database.tls_root_cert.is_none());
    for (text, mode) in [
        ("verify-full", TlsMode::VerifyFull),
        ("require", TlsMode::Require),
        ("disable", TlsMode::Disable),
    ] {
        let cfg = load(None, &env(&[("FORGE_DATABASE_TLS_MODE", text)])).expect(text);
        assert_eq!(cfg.database.tls_mode, mode);
    }
    // verify-ca would accept any public certificate (public roots, no name).
    for bad in ["verify-ca", "prefer", "allow", "VERIFY_FULL", "on"] {
        let err = load(None, &env(&[("FORGE_DATABASE_TLS_MODE", bad)])).expect_err(bad);
        assert_invalid(&err, "database.tls_mode", "invalid");
    }
}

#[test]
fn tls_root_cert_must_be_absolute() {
    let err = load(None, &env(&[("FORGE_DATABASE_TLS_ROOT_CERT", "ca.pem")])).expect_err("rel");
    assert_invalid(&err, "database.tls_root_cert", "not_absolute");
    let dir = tempfile::tempdir().expect("dir");
    let ca = dir.path().join("ca.pem").to_string_lossy().into_owned();
    let cfg = load(None, &env(&[("FORGE_DATABASE_TLS_ROOT_CERT", &ca)])).expect("abs");
    assert_eq!(cfg.database.tls_root_cert.as_deref(), Some(Path::new(&ca)));
}

#[test]
fn any_query_or_fragment_in_the_url_is_rejected() {
    // TLS settings, target overrides, typos of "password" (which the driver
    // would log with their value), and anything else.
    for raw in [
        "postgres://forge@db/forge?sslmode=disable",
        "postgres://forge@db/forge?SSLROOTCERT=/x",
        "postgres://forge@db-a/forge?host=db-b",
        "postgres://forge@db-a/forge?port=6000",
        "postgres://forge@db-a/forge?options=-csearch_path%3Devil",
        "postgres://forge@db/forge?pasword=hunter2",
        "postgres://forge@db/forge?application_name=x",
        "postgres://forge@db/forge?%E3%81%82=1",
        "postgres://forge@db/forge?",
        "postgres://forge@db/forge#frag",
    ] {
        let err = load(None, &env(&[("FORGE_DATABASE_URL", raw)])).expect_err(raw);
        assert_invalid(&err, "database.url", "query_in_url");
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }
}
