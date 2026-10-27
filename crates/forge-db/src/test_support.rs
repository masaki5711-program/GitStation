//! Shared helpers for the DB tests (this crate's, and other crates' with the
//! `test-support` feature). They need the development database (ADR-0011)
//! and fail — never skip — with the compose command when it is not running.
//!
//! The cluster is bootstrapped once per test process with the real files
//! of `ops/db/bootstrap/` (roles; synthetic passwords for the logins), and
//! every [`FreshDb`] is a new database set up by steps 2 and 3.
#![allow(clippy::expect_used, clippy::panic)] // test helpers: failing loudly is the point

use std::io::Write as _;

use forge_core::config::{self, DatabaseConfig, Env};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;
use sqlx::{ConnectOptions as _, Connection as _};

use crate::bootstrap::FILES;
use crate::Db;

pub const HINT: &str = "start the development database: \
    docker compose -f ops/dev/compose.yml up -d --wait (in Git/)";

/// Synthetic passwords of the bootstrap logins (development cluster only).
pub const APP_PASSWORD: &str = "forge_app_dev";
pub const MIGRATOR_PASSWORD: &str = "forge_migrator_dev";
pub const MAINT_PASSWORD: &str = "forge_maint_dev";

/// The superuser URL of the development cluster.
pub fn dev_url() -> String {
    std::env::var("FORGE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://forge_dev@127.0.0.1:55432/forge_dev".to_owned())
}

/// `dev_url()` with another database name.
pub fn url_with_db(name: &str) -> String {
    let base = dev_url();
    let (server, _db) = base.rsplit_once('/').expect("url has a dbname");
    format!("{server}/{name}")
}

/// `dev_url()` with another user and database.
pub fn url_as(user: &str, dbname: &str) -> String {
    let base = url_with_db(dbname);
    let (scheme, rest) = base.split_once("://").expect("url has a scheme");
    let (_user, host) = rest.split_once('@').expect("url has a user");
    format!("{scheme}://{user}@{host}")
}

/// A password file that lives as long as this value.
pub struct PasswordFile(tempfile::NamedTempFile);

impl PasswordFile {
    pub fn new(password: &str) -> Self {
        let mut f = tempfile::NamedTempFile::new().expect("password file");
        f.write_all(password.as_bytes()).expect("write");
        Self(f)
    }

    pub fn path(&self) -> String {
        self.0.path().to_string_lossy().into_owned()
    }
}

/// A database config for tests; the password files live as long as it.
pub struct TestConfig {
    pub db: DatabaseConfig,
    _pw: Vec<PasswordFile>,
}

pub fn test_config(url: &str, password: &str, extra: &[(&str, &str)]) -> TestConfig {
    let pw = PasswordFile::new(password);
    let pw_path = pw.path();
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
        _pw: vec![pw],
    }
}

/// One superuser connection to `dbname`.
pub async fn superuser(dbname: &str) -> PgConnection {
    let url = url_with_db(dbname);
    let options: sqlx::postgres::PgConnectOptions = url.parse().expect("dev url");
    match options.password("forge_dev").connect().await {
        Ok(conn) => conn,
        Err(e) => panic!("{HINT}\n{e}"),
    }
}

static CLUSTER: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Step 1 and the synthetic passwords, once per process. Other test
/// processes may run it at the same time: both statements serialize on
/// the advisory lock of step 1 (the same maintenance database).
pub async fn ensure_cluster() {
    CLUSTER
        .get_or_init(|| async {
            let mut admin = superuser(&maintenance_db()).await;
            sqlx::raw_sql(FILES[0].1)
                .execute(&mut admin)
                .await
                .expect("bootstrap step 1 (01-cluster-roles.sql)");
            sqlx::raw_sql(&format!(
                "BEGIN; \
                 SELECT pg_advisory_xact_lock(hashtext('forge.bootstrap.cluster_roles')); \
                 ALTER ROLE forge_app PASSWORD '{APP_PASSWORD}'; \
                 ALTER ROLE forge_migrator PASSWORD '{MIGRATOR_PASSWORD}'; \
                 ALTER ROLE forge_maint PASSWORD '{MAINT_PASSWORD}'; \
                 COMMIT;"
            ))
            .execute(&mut admin)
            .await
            .expect("set synthetic passwords");
            // Dropping the connection closes it; nothing else to wait for.
            drop(admin);
        })
        .await;
}

fn maintenance_db() -> String {
    dev_url()
        .rsplit_once('/')
        .map(|(_, db)| db.to_owned())
        .expect("url has a dbname")
}

/// The superuser on the maintenance database, as an app-shaped `Db` (for
/// tests about connecting, timeouts and error mapping).
pub async fn dev_db(extra: &[(&str, &str)]) -> (Db, TestConfig) {
    ensure_cluster().await;
    let cfg = test_config(&dev_url(), "forge_dev", extra);
    match Db::connect(&cfg.db).await {
        Ok(db) => (db, cfg),
        Err(e) => panic!("{HINT}\n{e}"),
    }
}

/// One CREATE DATABASE at a time per test process: concurrent ones contend
/// on the template database and slow every other test.
static CREATE_TURN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// A bootstrapped database created for one test (`forge_t_<uuid>`), dropped
/// by [`FreshDb::drop_now`]. A panicking test leaks it; the prefix makes
/// such leftovers easy to find (`SELECT datname FROM pg_database WHERE
/// datname LIKE 'forge_t_%'`).
pub struct FreshDb {
    /// Logged in as forge_app.
    pub db: Db,
    pub name: String,
    app: TestConfig,
}

impl FreshDb {
    pub async fn new() -> Self {
        ensure_cluster().await;
        let name = format!("forge_t_{}", uuid::Uuid::new_v4().simple());
        {
            let _turn = CREATE_TURN
                .acquire()
                .await
                .expect("semaphore is never closed");
            let mut admin = superuser(&maintenance_db()).await;
            let create = FILES[1].1.replace(":\"dbname\"", &format!("\"{name}\""));
            sqlx::raw_sql(&create)
                .execute(&mut admin)
                .await
                .expect("bootstrap step 2 (02-create-database.sql)");
            drop(admin);
        }
        let mut owner = superuser(&name).await;
        sqlx::raw_sql(FILES[2].1)
            .execute(&mut owner)
            .await
            .expect("bootstrap step 3 (03-database-objects.sql)");
        owner.close().await.expect("close");
        let app = test_config(&url_as("forge_app", &name), APP_PASSWORD, &[]);
        let db = Db::connect(&app.db).await.expect("connect as forge_app");
        Self { db, name, app }
    }

    /// The application config (forge_app) with extra settings.
    pub fn app_config(&self, extra: &[(&str, &str)]) -> TestConfig {
        test_config(&url_as("forge_app", &self.name), APP_PASSWORD, extra)
    }

    /// A config whose migrate login is forge_migrator.
    pub fn migrate_config(&self, extra: &[(&str, &str)]) -> TestConfig {
        let pw = PasswordFile::new(MIGRATOR_PASSWORD);
        let (url, path) = (url_as("forge_migrator", &self.name), pw.path());
        let mut pairs = vec![
            ("FORGE_DATABASE_MIGRATE_URL", url.as_str()),
            ("FORGE_DATABASE_MIGRATE_PASSWORD_FILE", path.as_str()),
        ];
        pairs.extend_from_slice(extra);
        let mut cfg = self.app_config(&pairs);
        cfg._pw.push(pw);
        cfg
    }

    /// Environment for a `forge` subprocess logged in as forge_app (and
    /// forge_migrator); the password files live as long as the returned
    /// guard.
    pub fn env(&self) -> (Vec<(String, String)>, Vec<PasswordFile>) {
        let app = PasswordFile::new(APP_PASSWORD);
        let migrator = PasswordFile::new(MIGRATOR_PASSWORD);
        let vars = vec![
            (
                "FORGE_DATABASE_URL".to_owned(),
                url_as("forge_app", &self.name),
            ),
            ("FORGE_DATABASE_PASSWORD_FILE".to_owned(), app.path()),
            (
                "FORGE_DATABASE_MIGRATE_URL".to_owned(),
                url_as("forge_migrator", &self.name),
            ),
            (
                "FORGE_DATABASE_MIGRATE_PASSWORD_FILE".to_owned(),
                migrator.path(),
            ),
            ("FORGE_DATABASE_TLS_MODE".to_owned(), "disable".to_owned()),
        ];
        (vars, vec![app, migrator])
    }

    /// A superuser connection to this database (to inspect or tamper).
    pub async fn superuser(&self) -> PgConnection {
        superuser(&self.name).await
    }

    pub async fn drop_now(self) {
        self.db.close().await;
        drop(self.app);
        let mut admin = superuser(&maintenance_db()).await;
        sqlx::query(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name))
            .execute(&mut admin)
            .await
            .expect("drop test database");
    }
}

pub fn assert_reason(err: &ForgeError, code: ErrorCode, reason: &str) {
    assert_eq!(err.code(), code, "{err}");
    assert!(
        err.to_string().contains(&format!("reason={reason}")),
        "{err}"
    );
}

/// The whole error chain as the log would show it.
pub fn chain(err: &ForgeError) -> String {
    let mut text = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        text.push_str(&format!(" | {s}"));
        source = s.source();
    }
    text
}
