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
    assert_eq!(db.write_max_connections, 10);
    assert_eq!(db.read_max_connections, 10);
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
write_max_connections = 20
"#,
    );
    let cfg = load(
        Some(f.path()),
        &env(&[
            ("FORGE_SERVER_BIND", "127.0.0.1:9001"),
            ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", "30"),
        ]),
    )
    .expect("loads");
    assert_eq!(cfg.server.bind, "127.0.0.1:9001");
    assert_eq!(cfg.database.write_max_connections, 30);
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
            "[database]\nwrite_max_connections = true\n",
            "database.write_max_connections",
        ),
        (
            "[database]\nwrite_max_connections = 1.5\n",
            "database.write_max_connections",
        ),
        (
            "[database]\nwrite_max_connections = 4294967296\n",
            "database.write_max_connections",
        ),
        (
            "[database]\nwrite_max_connections = [1]\n",
            "database.write_max_connections",
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
    let err = load(
        None,
        &env(&[("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", "ten")]),
    )
    .expect_err("nan");
    assert_invalid(&err, "database.write_max_connections", "invalid");
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
        (
            "FORGE_DATABASE_WRITE_MAX_CONNECTIONS",
            "database.write_max_connections",
        ),
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
            ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", "2"),
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

#[test]
fn tls_allow_unverified_is_a_strict_boolean() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert!(!cfg.database.tls_allow_unverified);
    let cfg = load(
        None,
        &env(&[("FORGE_DATABASE_TLS_ALLOW_UNVERIFIED", "true")]),
    )
    .expect("t");
    assert!(cfg.database.tls_allow_unverified);
    for bad in ["yes", "1", "TRUE"] {
        let err = load(None, &env(&[("FORGE_DATABASE_TLS_ALLOW_UNVERIFIED", bad)])).expect_err(bad);
        assert_invalid(&err, "database.tls_allow_unverified", "invalid");
    }
    let f = file("[database]\ntls_allow_unverified = true\n");
    assert!(
        load(Some(f.path()), &env(&[]))
            .expect("toml bool")
            .database
            .tls_allow_unverified
    );
    let f = file("[database]\nwrite_max_connections = true\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("bool for a number");
    assert_invalid(&err, "database.write_max_connections", "invalid");
}

#[test]
fn migrate_and_maint_logins_are_separate_and_optional() {
    let migrator = file("migrator-pw-1\n");
    let maint = file("maint-pw-2\n");
    let (migrator_path, maint_path) = (
        migrator.path().to_string_lossy().into_owned(),
        maint.path().to_string_lossy().into_owned(),
    );
    let cfg = load(
        None,
        &env(&[
            ("FORGE_DATABASE_URL", "postgres://forge_app@db/forge"),
            (
                "FORGE_DATABASE_MIGRATE_URL",
                "postgres://forge_migrator@db/forge",
            ),
            ("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", &migrator_path),
            (
                "FORGE_DATABASE_MAINT_URL",
                "postgres://forge_maint@db/forge",
            ),
            ("FORGE_DATABASE_MAINT_PASSWORD_FILE", &maint_path),
        ]),
    )
    .expect("loads");
    let db = &cfg.database;
    assert_eq!(db.migrate_url().expect("migrate").user(), "forge_migrator");
    assert_eq!(db.maint_url().expect("maint").user(), "forge_maint");
    // Read on use only (serve never reads them).
    assert_eq!(
        db.migrate_password()
            .expect("read")
            .as_ref()
            .map(Secret::expose),
        Some("migrator-pw-1")
    );
    assert_eq!(
        db.maint_password()
            .expect("read")
            .as_ref()
            .map(Secret::expose),
        Some("maint-pw-2")
    );
    assert_invalid(
        &db.reject_privileged_logins().expect_err("serve refuses"),
        "database.migrate_password_file",
        "not_for_this_command",
    );
    assert!(db.password.is_none());
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("migrator-pw-1") && !debug.contains("maint-pw-2"));

    let none = load(None, &env(&[])).expect("defaults");
    none.database
        .reject_privileged_logins()
        .expect("nothing privileged configured");
    assert_invalid(
        &none.database.migrate_url().expect_err("unset"),
        "database.migrate_url",
        "missing",
    );
    assert_invalid(
        &none.database.maint_url().expect_err("unset"),
        "database.maint_url",
        "missing",
    );
}

#[test]
fn an_invalid_migrate_url_or_password_file_names_its_own_key() {
    let err = load(
        None,
        &env(&[("FORGE_DATABASE_MIGRATE_URL", "postgres://m:pw@db/forge")]),
    )
    .expect_err("password in url");
    assert_invalid(&err, "database.migrate_url", "password_in_url");
    let err = load(
        None,
        &env(&[("FORGE_DATABASE_MAINT_PASSWORD_FILE", "relative/pw")]),
    )
    .expect_err("relative");
    assert_invalid(&err, "database.maint_password_file", "not_absolute");
}

#[test]
fn the_old_max_connections_variable_names_the_write_pool_key() {
    let err = load(None, &env(&[("FORGE_DATABASE_MAX_CONNECTIONS", "5")])).expect_err("renamed");
    assert_invalid(&err, "FORGE_DATABASE_MAX_CONNECTIONS", "renamed");
    assert!(
        err.to_string()
            .contains("renamed_to=FORGE_DATABASE_WRITE_MAX_CONNECTIONS"),
        "{err}"
    );
}

#[test]
fn min_connections_must_fit_the_smaller_pool() {
    let err = load(
        None,
        &env(&[
            ("FORGE_DATABASE_MIN_CONNECTIONS", "4"),
            ("FORGE_DATABASE_READ_MAX_CONNECTIONS", "3"),
        ]),
    )
    .expect_err("min > read max");
    assert_invalid(&err, "database.min_connections", "above_max");
    let err = load(None, &env(&[("FORGE_DATABASE_READ_MAX_CONNECTIONS", "0")])).expect_err("zero");
    assert_invalid(&err, "database.read_max_connections", "zero");
}

#[test]
fn privileged_members_are_role_member_pairs() {
    let cfg = load(
        None,
        &env(&[(
            "FORGE_DATABASE_PRIVILEGED_MEMBERS",
            "forge_owner:dba_alice, forge_seq:ops_2",
        )]),
    )
    .expect("loads");
    let pairs: Vec<(&str, &str)> = cfg
        .database
        .privileged_members
        .iter()
        .map(|m| (m.role.as_str(), m.member.as_str()))
        .collect();
    assert_eq!(
        pairs,
        [("forge_owner", "dba_alice"), ("forge_seq", "ops_2")]
    );
    for (value, reason) in [
        ("dba_alice", "invalid"),
        ("forge_app:dba", "invalid"),
        ("forge_owner:Dba", "invalid"),
        ("forge_owner:a;drop", "invalid"),
        ("forge_owner:ok,", "invalid"),
        ("forge_owner:x,forge_owner:x", "duplicate"),
        ("forge_purge:forge_audit", "invalid"),
    ] {
        let err =
            load(None, &env(&[("FORGE_DATABASE_PRIVILEGED_MEMBERS", value)])).expect_err(value);
        assert_invalid(&err, "database.privileged_members", reason);
    }
}

// ── C8: audit.seal_keys_file ──
#[test]
fn the_seal_keys_file_is_checked_at_load_and_read_on_use() {
    let keys = file(&format!("1 {}\n2 {}\n", "11".repeat(32), "22".repeat(32)));
    let path = keys.path().to_string_lossy().into_owned();
    let cfg = load(None, &env(&[("FORGE_AUDIT_SEAL_KEYS_FILE", path.as_str())])).expect("loads");
    assert!(cfg.audit.has_seal_keys());
    let read = cfg.audit.seal_keys().expect("read").expect("configured");
    assert_eq!(read.signing().id(), 2);
    assert!(!format!("{cfg:?}").contains("2222"));

    let toml = file(&format!("[audit]\nseal_keys_file = {path:?}\n"));
    let from_file = load(Some(toml.path()), &env(&[])).expect("file");
    assert!(from_file.audit.has_seal_keys());

    let none = load(None, &env(&[])).expect("defaults");
    assert!(none.audit.seal_keys().expect("unset").is_none());
    // The worker needs them: not configured is a named error.
    assert_invalid(
        &none.audit.require_seal_keys().expect_err("required"),
        "audit.seal_keys_file",
        "missing",
    );
    assert_eq!(
        cfg.audit
            .require_seal_keys()
            .expect("required")
            .signing()
            .id(),
        2
    );
    let err = load(
        None,
        &env(&[("FORGE_AUDIT_SEAL_KEYS_FILE", "relative/keys")]),
    )
    .expect_err("relative");
    assert_invalid(&err, "audit.seal_keys_file", "not_absolute");
    let err = load(None, &env(&[("FORGE_AUDIT_SEAL_KEY", "x")])).expect_err("typo");
    assert_invalid(&err, "FORGE_AUDIT_SEAL_KEY", "unknown_env");
    let bad = file("1 abc\n");
    let bad_path = bad.path().to_string_lossy().into_owned();
    let cfg = load(
        None,
        &env(&[("FORGE_AUDIT_SEAL_KEYS_FILE", bad_path.as_str())]),
    )
    .expect("the path is fine");
    assert_invalid(
        &cfg.audit.seal_keys().expect_err("content is not"),
        "audit.seal_keys_file",
        "invalid_key",
    );
}
// ── end C8 ──

/// M1 phase 9 MR3: `git.http.*` defaults, the `[git.http]` table in the
/// file, environment overrides and the cross-key rules (FG-SYS-0002).
#[test]
fn git_http_limits_have_defaults_file_env_and_rules() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.git_http, GitHttpConfig::DEFAULT);
    assert_eq!(cfg.git_http.fetch_request_max_bytes, 16 * 1024 * 1024);
    assert_eq!(cfg.git_http.idle_timeout, Duration::from_secs(60));
    assert_eq!(cfg.git_http.timeout, Duration::from_secs(3600));
    assert_eq!(cfg.git_http.max_streams, 16);
    assert_eq!(cfg.git_http.max_streams_per_principal, 4);
    assert_eq!(cfg.git_http.body_timeout, Duration::from_secs(300));
    assert_eq!(cfg.git_http.max_object_wants, 10_000);
    assert_eq!(cfg.git_http.push_max_bytes, 2 * 1024 * 1024 * 1024);

    let f = file("[git.http]\nmax_streams = 8\nidle_timeout_ms = 1000\n");
    let cfg = load(
        Some(f.path()),
        &env(&[
            ("FORGE_GIT_HTTP_MAX_STREAMS_PER_PRINCIPAL", "2"),
            ("FORGE_GIT_HTTP_PUSH_MAX_BYTES", "65536"),
        ]),
    )
    .expect("file + env");
    assert_eq!(cfg.git_http.push_max_bytes, 65536);
    assert_eq!(cfg.git_http.max_streams, 8);
    assert_eq!(cfg.git_http.max_streams_per_principal, 2);
    assert_eq!(cfg.git_http.idle_timeout, Duration::from_millis(1000));

    for (pairs, key, reason) in [
        (
            vec![("FORGE_GIT_HTTP_MAX_STREAMS", "0")],
            "git.http.max_streams",
            "zero",
        ),
        (
            vec![("FORGE_GIT_HTTP_FETCH_REQUEST_MAX_BYTES", "4294967296")],
            "git.http.fetch_request_max_bytes",
            "invalid",
        ),
        (
            vec![("FORGE_GIT_HTTP_FETCH_REQUEST_MAX_BYTES", "1073741825")],
            "git.http.fetch_request_max_bytes",
            "above_max",
        ),
        (
            vec![
                ("FORGE_GIT_HTTP_TIMEOUT_MS", "10000"),
                ("FORGE_GIT_HTTP_WANT_CHECK_TIMEOUT_MS", "20000"),
            ],
            "git.http.want_check_timeout_ms",
            "above_timeout",
        ),
        (
            vec![
                ("FORGE_GIT_HTTP_TIMEOUT_MS", "10000"),
                ("FORGE_GIT_HTTP_WANT_CHECK_TIMEOUT_MS", "5000"),
                ("FORGE_GIT_HTTP_BODY_TIMEOUT_MS", "5000"),
                ("FORGE_GIT_HTTP_IDLE_TIMEOUT_MS", "20000"),
            ],
            "git.http.idle_timeout_ms",
            "above_timeout",
        ),
        (
            vec![
                ("FORGE_GIT_HTTP_TIMEOUT_MS", "10000"),
                ("FORGE_GIT_HTTP_WANT_CHECK_TIMEOUT_MS", "5000"),
                ("FORGE_GIT_HTTP_IDLE_TIMEOUT_MS", "5000"),
            ],
            "git.http.body_timeout_ms",
            "above_timeout",
        ),
        (
            vec![("FORGE_GIT_HTTP_MAX_OBJECT_WANTS", "0")],
            "git.http.max_object_wants",
            "zero",
        ),
        (
            vec![
                ("FORGE_GIT_HTTP_MAX_STREAMS", "2"),
                ("FORGE_GIT_HTTP_MAX_STREAMS_PER_PRINCIPAL", "3"),
            ],
            "git.http.max_streams_per_principal",
            "above_max",
        ),
        (
            vec![("FORGE_GIT_HTTP_NOPE", "1")],
            "FORGE_GIT_HTTP_NOPE",
            "unknown_env",
        ),
    ] {
        let err = load(None, &env(&pairs)).expect_err(key);
        assert_invalid(&err, key, reason);
    }
    let f = file("[git.http]\nnope = 1\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("unknown key");
    assert_invalid(&err, "git.http.nope", "unknown_key");
    let f = file("[git.ssh]\nport = 1\n");
    let err = load(Some(f.path()), &env(&[])).expect_err("unknown table");
    assert_invalid(&err, "git.ssh", "unknown_key");
}

/// M1 phase 9 MR0: `server.*` connection limits have defaults, a `[server]`
/// table, environment overrides and cross-key rules (FG-SYS-0002).
#[test]
fn server_limits_have_defaults_file_env_and_rules() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.server.limits, ServerLimitsConfig::DEFAULT);
    assert_eq!(cfg.server.limits.max_connections, 1024);
    assert_eq!(cfg.server.limits.max_connections_per_peer, 64);

    let f = file("[server]\nmax_connections = 100\nwrite_idle_timeout_ms = 2000\n");
    let cfg = load(
        Some(f.path()),
        &env(&[("FORGE_SERVER_MAX_CONNECTIONS_PER_PEER", "7")]),
    )
    .expect("file + env");
    assert_eq!(cfg.server.limits.max_connections, 100);
    assert_eq!(cfg.server.limits.max_connections_per_peer, 7);
    assert_eq!(
        cfg.server.limits.write_idle_timeout,
        Duration::from_millis(2000)
    );

    for (pairs, key, reason) in [
        (
            vec![("FORGE_SERVER_MAX_CONNECTIONS", "0")],
            "server.max_connections",
            "zero",
        ),
        (
            vec![("FORGE_SERVER_MAX_CONNECTIONS_PER_PEER", "0")],
            "server.max_connections_per_peer",
            "zero",
        ),
        (
            vec![("FORGE_SERVER_WRITE_IDLE_TIMEOUT_MS", "0")],
            "server.write_idle_timeout_ms",
            "zero",
        ),
        (
            vec![("FORGE_SERVER_KEEP_ALIVE_MAX_REQUESTS", "abc")],
            "server.keep_alive_max_requests",
            "invalid",
        ),
        (
            vec![
                ("FORGE_SERVER_MAX_CONNECTIONS", "10"),
                ("FORGE_SERVER_MAX_CONNECTIONS_PER_PEER", "11"),
            ],
            "server.max_connections_per_peer",
            "above_max",
        ),
        (
            vec![
                ("FORGE_SERVER_READ_IDLE_TIMEOUT_MS", "5000"),
                ("FORGE_GIT_HTTP_IDLE_TIMEOUT_MS", "6000"),
            ],
            "server.read_idle_timeout_ms",
            "below_git_idle_timeout",
        ),
        (
            vec![("FORGE_SERVER_READ_IDLE_TIMEOUT_MS", "0")],
            "server.read_idle_timeout_ms",
            "zero",
        ),
        (
            vec![("FORGE_SERVER_BODY_READ_TIMEOUT_MS", "0")],
            "server.body_read_timeout_ms",
            "zero",
        ),
        (
            vec![
                ("FORGE_SERVER_HEADER_READ_TIMEOUT_MS", "1000"),
                ("FORGE_SERVER_BODY_READ_TIMEOUT_MS", "5000"),
                ("FORGE_SERVER_KEEP_ALIVE_MAX_MS", "4000"),
            ],
            "server.body_read_timeout_ms",
            "above_keep_alive_max",
        ),
        (
            vec![
                ("FORGE_SERVER_HEADER_READ_TIMEOUT_MS", "5000"),
                ("FORGE_SERVER_KEEP_ALIVE_MAX_MS", "4000"),
            ],
            "server.header_read_timeout_ms",
            "above_keep_alive_max",
        ),
    ] {
        let err = load(None, &env(&pairs)).expect_err(key);
        assert_invalid(&err, key, reason);
    }
}

// ── M1 phase 8b MR6: api.cursor_keys_file ──
#[test]
fn the_cursor_keys_file_is_checked_at_load_and_read_on_use() {
    let keys = file(&format!("3 {}\n9 {}\n", "33".repeat(32), "99".repeat(32)));
    let path = keys.path().to_string_lossy().into_owned();
    let cfg = load(None, &env(&[("FORGE_API_CURSOR_KEYS_FILE", path.as_str())])).expect("loads");
    assert!(cfg.api.has_cursor_keys());
    let read = cfg.api.cursor_keys().expect("read").expect("configured");
    assert_eq!(read.encrypting().kid(), 9);
    assert!(!format!("{cfg:?} {read:?}").contains("9999"));

    let toml = file(&format!("[api]\ncursor_keys_file = {path:?}\n"));
    let from_file = load(Some(toml.path()), &env(&[])).expect("file");
    assert!(from_file.api.has_cursor_keys());

    let none = load(None, &env(&[])).expect("defaults");
    assert!(none.api.cursor_keys().expect("unset").is_none());
    assert_invalid(
        &none.api.require_cursor_keys().expect_err("required"),
        "api.cursor_keys_file",
        "missing",
    );
    let err = load(
        None,
        &env(&[("FORGE_API_CURSOR_KEYS_FILE", "relative/keys")]),
    )
    .expect_err("relative");
    assert_invalid(&err, "api.cursor_keys_file", "not_absolute");
    // A typo under the new namespace fails like every other one.
    let err = load(None, &env(&[("FORGE_API_CURSOR_KEY_FILE", path.as_str())])).expect_err("typo");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
}

/// `api.list_max_concurrent*` and `api.cursor_max_age_s`: defaults, env and
/// the rules (FG-SYS-0002).
#[test]
fn api_list_limits_and_cursor_age_have_defaults_env_and_rules() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.api.list_max_concurrent, 16);
    assert_eq!(cfg.api.list_max_concurrent_per_principal, 2);
    assert_eq!(cfg.api.cursor_max_age, Duration::from_secs(86_400));

    let cfg = load(
        None,
        &env(&[
            ("FORGE_API_LIST_MAX_CONCURRENT", "4"),
            ("FORGE_API_LIST_MAX_CONCURRENT_PER_PRINCIPAL", "4"),
            ("FORGE_API_CURSOR_MAX_AGE_S", "60"),
        ]),
    )
    .expect("env");
    assert_eq!(cfg.api.list_max_concurrent, 4);
    assert_eq!(cfg.api.cursor_max_age, Duration::from_secs(60));

    for (pairs, key, reason) in [
        (
            vec![("FORGE_API_LIST_MAX_CONCURRENT", "0")],
            "api.list_max_concurrent",
            "zero",
        ),
        (
            vec![("FORGE_API_CURSOR_MAX_AGE_S", "0")],
            "api.cursor_max_age_s",
            "zero",
        ),
        (
            vec![("FORGE_API_LIST_MAX_CONCURRENT_PER_PRINCIPAL", "17")],
            "api.list_max_concurrent_per_principal",
            "above_max",
        ),
    ] {
        let err = load(None, &env(&pairs)).expect_err(key);
        let text = err.to_string();
        assert!(text.contains(key) && text.contains(reason), "{key}: {text}");
    }
}

/// `hosting.deletion_grace_days` (D5 MR3): default 7 days, env, and the
/// 1..=365 range (FG-SYS-0002 naming the key).
#[test]
fn hosting_deletion_grace_has_a_default_env_and_a_range() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.hosting.deletion_grace, Duration::from_secs(7 * 86_400));
    assert_eq!(
        cfg.hosting.deletion_grace,
        HostingConfig::DEFAULT_DELETION_GRACE
    );
    let cfg = load(None, &env(&[("FORGE_HOSTING_DELETION_GRACE_DAYS", "1")])).expect("min");
    assert_eq!(cfg.hosting.deletion_grace, Duration::from_secs(86_400));
    let cfg = load(None, &env(&[("FORGE_HOSTING_DELETION_GRACE_DAYS", "365")])).expect("max");
    assert_eq!(
        cfg.hosting.deletion_grace,
        Duration::from_secs(365 * 86_400)
    );
    for (value, reason) in [
        ("0", "below_min"),
        ("366", "above_max"),
        ("-1", "invalid"),
        ("a week", "invalid"),
    ] {
        let err =
            load(None, &env(&[("FORGE_HOSTING_DELETION_GRACE_DAYS", value)])).expect_err(value);
        assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{value}");
        let text = err.to_string();
        assert!(
            text.contains("hosting.deletion_grace_days") && text.contains(reason),
            "{value}: {text}"
        );
    }
    // A typo under the new namespace fails like every other one.
    let err = load(None, &env(&[("FORGE_HOSTING_DELETION_GRACE", "7")])).expect_err("typo");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
}

/// `hosting.purge_interval_secs` (D5 MR4): default 300 s, env, and the
/// 10..=86400 range (FG-SYS-0002 naming the key).
#[test]
fn hosting_purge_interval_has_a_default_env_and_a_range() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.hosting.purge_interval, Duration::from_secs(300));
    assert_eq!(
        cfg.hosting.purge_interval,
        HostingConfig::DEFAULT_PURGE_INTERVAL
    );
    for (value, secs) in [("10", 10), ("86400", 86_400), (" 60 ", 60)] {
        let cfg = load(None, &env(&[("FORGE_HOSTING_PURGE_INTERVAL_SECS", value)])).expect(value);
        assert_eq!(cfg.hosting.purge_interval, Duration::from_secs(secs));
    }
    for (value, reason) in [
        ("9", "below_min"),
        ("0", "below_min"),
        ("86401", "above_max"),
        ("-1", "invalid"),
        ("5m", "invalid"),
    ] {
        let err =
            load(None, &env(&[("FORGE_HOSTING_PURGE_INTERVAL_SECS", value)])).expect_err(value);
        assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{value}");
        let text = err.to_string();
        assert!(
            text.contains("hosting.purge_interval_secs") && text.contains(reason),
            "{value}: {text}"
        );
    }
}

/// `web.base_url` (M2 MR3, ADR-0021 §12): https, or http on loopback; no
/// path, query or user info; the origin as a browser sends it. Optional at
/// load (`forge serve` requires it).
#[test]
fn the_web_base_url_is_an_origin_and_is_validated() {
    let none = load(None, &env(&[])).expect("defaults");
    assert!(none.web.base_url().is_none());
    assert_invalid(
        &none.web.require_base_url().expect_err("required"),
        "web.base_url",
        "missing",
    );
    for (value, origin) in [
        ("https://git.example.test", "https://git.example.test"),
        ("https://git.example.test/", "https://git.example.test"),
        ("https://GIT.example.test:443/", "https://git.example.test"),
        (
            "https://git.example.test:8443",
            "https://git.example.test:8443",
        ),
        ("http://localhost:8080", "http://localhost:8080"),
        ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
    ] {
        let cfg = load(None, &env(&[("FORGE_WEB_BASE_URL", value)])).expect(value);
        assert_eq!(
            cfg.web.require_base_url().expect("set").origin(),
            origin,
            "{value}"
        );
    }
    for (value, reason) in [
        ("not a url", "invalid"),
        ("http://git.example.test", "insecure_http"),
        ("ftp://git.example.test", "scheme"),
        ("https://git.example.test/gitstation", "has_path"),
        ("https://git.example.test/?a=1", "has_query"),
        ("https://git.example.test/#x", "has_query"),
        ("https://user@git.example.test", "has_user_info"),
    ] {
        let err = load(None, &env(&[("FORGE_WEB_BASE_URL", value)])).expect_err(value);
        assert_invalid(&err, "web.base_url", reason);
    }
    let toml = file("[web]\nbase_url = \"https://git.example.test\"\n");
    let from_file = load(Some(toml.path()), &env(&[])).expect("file");
    assert!(from_file.web.base_url().is_some());
}

/// `web.session_idle_secs` / `web.session_absolute_secs`: 4 h and 12 h by
/// default, at least 60 s, idle within absolute, absolute at most 24 h.
#[test]
fn web_session_lifetimes_have_defaults_and_rules() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.web.session_idle, Duration::from_secs(4 * 3600));
    assert_eq!(cfg.web.session_absolute, Duration::from_secs(12 * 3600));
    let cfg = load(
        None,
        &env(&[
            ("FORGE_WEB_SESSION_IDLE_SECS", "600"),
            ("FORGE_WEB_SESSION_ABSOLUTE_SECS", "86400"),
        ]),
    )
    .expect("set");
    assert_eq!(cfg.web.session_idle, Duration::from_secs(600));
    assert_eq!(cfg.web.session_absolute, Duration::from_secs(86_400));
    for (pairs, key, reason) in [
        (
            vec![("FORGE_WEB_SESSION_IDLE_SECS", "59")],
            "web.session_idle_secs",
            "below_min",
        ),
        (
            vec![("FORGE_WEB_SESSION_ABSOLUTE_SECS", "86401")],
            "web.session_absolute_secs",
            "above_max",
        ),
        (
            vec![
                ("FORGE_WEB_SESSION_IDLE_SECS", "7200"),
                ("FORGE_WEB_SESSION_ABSOLUTE_SECS", "3600"),
            ],
            "web.session_idle_secs",
            "above_absolute",
        ),
        (
            vec![("FORGE_WEB_SESSION_IDLE_SECS", "4h")],
            "web.session_idle_secs",
            "invalid",
        ),
    ] {
        let err = load(None, &env(&pairs)).expect_err(key);
        assert_invalid(&err, key, reason);
    }
}

/// `web.session_key_file` is checked at load (absolute path) and read on
/// use (secret file rules, the `<kid> <hex>` format); `FORGE_WEB_` and
/// `FORGE_AUTH_` are configuration namespaces (a typo fails).
#[test]
fn the_web_session_key_file_is_checked_at_load_and_read_on_use() {
    let keys = file(&format!("2 {}\n", "22".repeat(32)));
    let path = keys.path().to_string_lossy().into_owned();
    let cfg = load(None, &env(&[("FORGE_WEB_SESSION_KEY_FILE", path.as_str())])).expect("loads");
    assert!(cfg.web.has_session_keys());
    let read = cfg.web.session_keys().expect("read").expect("configured");
    assert_eq!(read.kids(), vec![2]);
    assert!(!format!("{cfg:?} {read:?}").contains("2222"));
    let none = load(None, &env(&[])).expect("defaults");
    assert!(none.web.session_keys().expect("unset").is_none());
    let err = load(
        None,
        &env(&[("FORGE_WEB_SESSION_KEY_FILE", "relative/keys")]),
    )
    .expect_err("relative");
    assert_invalid(&err, "web.session_key_file", "not_absolute");
    let broken = file("1 abcd\n");
    let broken_path = broken.path().to_string_lossy().into_owned();
    let cfg = load(
        None,
        &env(&[("FORGE_WEB_SESSION_KEY_FILE", broken_path.as_str())]),
    )
    .expect("checked on use");
    assert_invalid(
        &cfg.web.session_keys().expect_err("invalid key"),
        "web.session_key_file",
        "invalid_key",
    );
    for typo in ["FORGE_WEB_BASEURL", "FORGE_AUTH_OIDC_ISSUR"] {
        let err = load(None, &env(&[(typo, "x")])).expect_err(typo);
        assert_invalid(&err, typo, "unknown_env");
    }
}

/// `auth.oidc.*` (M2 MR4, ADR-0021 §12): all or nothing, the issuer's
/// scheme rules, the CA file for https, the algorithm allow-list, the
/// defaults; the client secret never shows in `Debug`.
#[test]
fn auth_oidc_is_all_or_nothing_and_validated() {
    let none = load(None, &env(&[])).expect("defaults");
    assert!(none.auth.oidc.is_none());
    let secret = file("synthetic-client-secret\n");
    let secret_path = secret.path().to_string_lossy().into_owned();
    let ca = file("-----BEGIN CERTIFICATE-----\n");
    let ca_path = ca.path().to_string_lossy().into_owned();
    let base = [
        (
            "FORGE_AUTH_OIDC_ISSUER",
            "http://127.0.0.1:58080/realms/gitstation-dev",
        ),
        ("FORGE_AUTH_OIDC_CLIENT_ID", "gitstation"),
        ("FORGE_AUTH_OIDC_CLIENT_SECRET_FILE", secret_path.as_str()),
    ];
    let cfg = load(None, &env(&base)).expect("loads");
    let oidc = cfg.auth.oidc.as_ref().expect("configured");
    assert_eq!(oidc.client_secret.expose(), "synthetic-client-secret");
    assert_eq!(oidc.employee_number_claim, "employee_number");
    assert_eq!(oidc.access_role, "access");
    assert_eq!(oidc.signing_algs, vec![OidcSigningAlg::Rs256]);
    assert!(oidc.auto_link);
    assert!(!format!("{cfg:?}").contains("synthetic-client-secret"));

    // Partial: the required keys are named.
    let err =
        load(None, &env(&[("FORGE_AUTH_OIDC_CLIENT_ID", "gitstation")])).expect_err("partial");
    assert_invalid(&err, "auth.oidc.issuer", "missing");
    let err = load(None, &env(&base[..2])).expect_err("no secret");
    assert_invalid(&err, "auth.oidc.client_secret_file", "missing");

    let with = |extra: &[(&'static str, &str)]| {
        let mut pairs: Vec<(&str, &str)> = base.to_vec();
        pairs.extend_from_slice(extra);
        load(None, &env(&pairs))
    };
    for (issuer, reason) in [
        ("http://keycloak.example.invalid/realms/x", "insecure_http"),
        ("ftp://127.0.0.1/realms/x", "scheme"),
        (
            "https://u:p@keycloak.example.invalid/realms/x",
            "has_user_info",
        ),
        ("https://keycloak.example.invalid/realms/x?a=b", "has_query"),
        ("not a url", "invalid"),
    ] {
        let err = with(&[("FORGE_AUTH_OIDC_ISSUER", issuer)]).expect_err(issuer);
        assert_invalid(&err, "auth.oidc.issuer", reason);
    }
    let https = (
        "FORGE_AUTH_OIDC_ISSUER",
        "https://keycloak.example.invalid/realms/x",
    );
    assert_invalid(
        &with(&[https]).expect_err("https needs a CA"),
        "auth.oidc.ca_file",
        "ca_required",
    );
    let ok = with(&[https, ("FORGE_AUTH_OIDC_CA_FILE", ca_path.as_str())]).expect("https + ca");
    assert!(ok.auth.oidc.expect("configured").ca_file.is_some());
    assert_invalid(
        &with(&[("FORGE_AUTH_OIDC_CA_FILE", ca_path.as_str())]).expect_err("http + ca"),
        "auth.oidc.ca_file",
        "ca_not_allowed",
    );
    for algs in ["none", "HS256", "PS256", "RS256,EdDSA"] {
        let err = with(&[("FORGE_AUTH_OIDC_SIGNING_ALGS", algs)]).expect_err(algs);
        assert_invalid(&err, "auth.oidc.signing_algs", "alg_not_allowed");
    }
    let both = with(&[("FORGE_AUTH_OIDC_SIGNING_ALGS", "ES256, RS256")]).expect("both");
    assert_eq!(
        both.auth.oidc.expect("configured").signing_algs,
        vec![OidcSigningAlg::Es256, OidcSigningAlg::Rs256]
    );
    let off = with(&[("FORGE_AUTH_OIDC_AUTO_LINK", "false")]).expect("auto_link");
    assert!(!off.auth.oidc.expect("configured").auto_link);
    assert_invalid(
        &with(&[("FORGE_AUTH_OIDC_AUTO_LINK", "yes")]).expect_err("bool"),
        "auth.oidc.auto_link",
        "invalid",
    );
    assert_invalid(
        &with(&[("FORGE_AUTH_OIDC_EMPLOYEE_NUMBER_CLAIM", "a b")]).expect_err("claim"),
        "auth.oidc.employee_number_claim",
        "invalid",
    );
}

/// `web.session_purge_interval_secs` (M2 MR6): default 1 h, env, and the
/// 60..=86400 range (FG-SYS-0002 naming the key).
#[test]
fn web_session_purge_interval_has_a_default_env_and_a_range() {
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.web.session_purge_interval, Duration::from_secs(3600));
    for (value, secs) in [("60", 60), ("86400", 86_400)] {
        let cfg = load(
            None,
            &env(&[("FORGE_WEB_SESSION_PURGE_INTERVAL_SECS", value)]),
        )
        .expect(value);
        assert_eq!(cfg.web.session_purge_interval, Duration::from_secs(secs));
    }
    for (value, reason) in [
        ("59", "below_min"),
        ("86401", "above_max"),
        ("x", "invalid"),
    ] {
        let err = load(
            None,
            &env(&[("FORGE_WEB_SESSION_PURGE_INTERVAL_SECS", value)]),
        )
        .expect_err(value);
        assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{value}");
        let text = err.to_string();
        assert!(
            text.contains("web.session_purge_interval_secs") && text.contains(reason),
            "{value}: {text}"
        );
    }
}
