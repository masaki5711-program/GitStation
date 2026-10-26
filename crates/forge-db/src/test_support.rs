//! Shared helpers for the DB tests. They need the development database
//! (ADR-0011) and fail — never skip — with the compose command when it is
//! not running.

use std::io::Write as _;

use forge_core::config::{self, DatabaseConfig, Env};
use forge_errors::{ErrorCode, ForgeError};

use crate::Db;

pub(crate) const HINT: &str = "start the development database: \
    docker compose -f ops/dev/compose.yml up -d --wait (in Git/)";

pub(crate) fn dev_url() -> String {
    std::env::var("FORGE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://forge_dev@127.0.0.1:55432/forge_dev".to_owned())
}

/// `dev_url()` with another database name.
pub(crate) fn url_with_db(name: &str) -> String {
    let base = dev_url();
    let (server, _db) = base.rsplit_once('/').expect("url has a dbname");
    format!("{server}/{name}")
}

/// A database config for tests; the password file lives as long as `_pw`.
pub(crate) struct TestConfig {
    pub(crate) db: DatabaseConfig,
    _pw: tempfile::NamedTempFile,
}

pub(crate) fn test_config(url: &str, password: &str, extra: &[(&str, &str)]) -> TestConfig {
    let mut pw = tempfile::NamedTempFile::new().expect("password file");
    pw.write_all(password.as_bytes()).expect("write");
    let pw_path = pw.path().to_string_lossy().into_owned();
    // Generous waits by default: the suite runs many tests (and CREATE
    // DATABASE) in parallel against one development server, often while
    // cargo compiles; a test that is about a timeout sets its own (later
    // pairs win).
    let mut pairs = vec![
        ("FORGE_DATABASE_URL", url),
        ("FORGE_DATABASE_PASSWORD_FILE", pw_path.as_str()),
        ("FORGE_DATABASE_TLS_MODE", "disable"),
        ("FORGE_DATABASE_CONNECT_TIMEOUT_MS", "15000"),
        ("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "10000"),
    ];
    pairs.extend_from_slice(extra);
    let cfg = config::load(None, &Env::from_pairs(pairs)).expect("test config");
    TestConfig {
        db: cfg.database,
        _pw: pw,
    }
}

pub(crate) async fn dev_db(extra: &[(&str, &str)]) -> (Db, TestConfig) {
    let cfg = test_config(&dev_url(), "forge_dev", extra);
    match Db::connect(&cfg.db).await {
        Ok(db) => (db, cfg),
        Err(e) => panic!("{HINT}\n{e}"),
    }
}

/// A database created for one test (`forge_t_<uuid>`), dropped by
/// [`FreshDb::drop_now`]. A panicking test leaks it; the prefix makes such
/// leftovers easy to find (`SELECT datname FROM pg_database WHERE datname
/// LIKE 'forge_t_%'`).
static CREATE_TURN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(crate) struct FreshDb {
    pub(crate) db: Db,
    /// Keeps the password file alive while the database is used.
    _cfg: TestConfig,
    name: String,
    admin: Db,
}

impl FreshDb {
    pub(crate) async fn new() -> Self {
        // One CREATE DATABASE at a time per test process: concurrent ones
        // contend on the template database and slow every other test.
        let _turn = CREATE_TURN
            .acquire()
            .await
            .expect("semaphore is never closed");
        let (admin, _admin_cfg) = dev_db(&[("FORGE_DATABASE_MAX_CONNECTIONS", "1")]).await;
        let name = format!("forge_t_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(admin.pool())
            .await
            .expect("create test database");
        let cfg = test_config(&url_with_db(&name), "forge_dev", &[]);
        let db = Db::connect(&cfg.db).await.expect("connect test database");
        Self {
            db,
            _cfg: cfg,
            name,
            admin,
        }
    }

    pub(crate) async fn drop_now(self) {
        self.db.pool().close().await;
        sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name))
            .execute(self.admin.pool())
            .await
            .expect("drop test database");
    }
}

pub(crate) fn assert_reason(err: &ForgeError, code: ErrorCode, reason: &str) {
    assert_eq!(err.code(), code, "{err}");
    assert!(
        err.to_string().contains(&format!("reason={reason}")),
        "{err}"
    );
}

/// The whole error chain as the log would show it.
pub(crate) fn chain(err: &ForgeError) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        text.push_str(&format!(" | {s}"));
        source = s.source();
    }
    text
}
