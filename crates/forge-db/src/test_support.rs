//! Shared helpers for the DB tests (this crate's, and other crates' with the
//! `test-support` feature). They need the development database (ADR-0011)
//! and fail — never skip — with the compose command when it is not running.
//!
//! The cluster is bootstrapped once per test process with the real files
//! of `ops/db/bootstrap/` (roles; synthetic passwords for the logins), and
//! every [`FreshDb`] is a new database set up by steps 2 and 3. Because the
//! logins get passwords that are in this source, the target must be a
//! loopback server (or `FORGE_TEST_ALLOW_ROLE_RESET=1` must be set).
//!
//! Roles are cluster-wide: a test that changes one holds
//! [`ClusterLock::exclusive`]; every test that runs the role guards holds
//! [`ClusterLock::shared`] (also across test processes).
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

/// A bootstrap file as SQL the driver can run: psql meta-commands
/// (`\set ON_ERROR_STOP on`) are dropped — the driver stops at the first
/// error anyway.
pub fn bootstrap_sql(index: usize) -> String {
    FILES[index]
        .1
        .lines()
        .filter(|l| !l.starts_with('\\'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn refuse_remote_cluster() {
    let url = dev_url();
    let host = url
        .split_once('@')
        .and_then(|(_, rest)| rest.split(['/', ':']).next())
        .unwrap_or_default()
        .to_owned();
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1";
    let allowed = std::env::var("FORGE_TEST_ALLOW_ROLE_RESET").as_deref() == Ok("1");
    assert!(
        loopback || allowed,
        "FORGE_TEST_DATABASE_URL points at {host}: the DB tests reset the forge logins' \
         passwords to values in the source; use a loopback development server or set \
         FORGE_TEST_ALLOW_ROLE_RESET=1 for a disposable one"
    );
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
/// the advisory lock of step 1 (the same maintenance database). It holds
/// the cluster lock shared, so it never runs while a test changes a role.
pub async fn ensure_cluster() {
    CLUSTER
        .get_or_init(|| async {
            refuse_remote_cluster();
            let mut admin = superuser(&maintenance_db()).await;
            sqlx::query("SELECT pg_advisory_lock_shared(hashtext($1))")
                .bind(ClusterLock::KEY)
                .execute(&mut admin)
                .await
                .expect("cluster lock (shared)");
            sqlx::raw_sql(&bootstrap_sql(0))
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
            // Closing the connection releases the lock.
            drop(admin);
        })
        .await;
}

/// The database named by `FORGE_TEST_DATABASE_URL` (cluster-wide changes run here).
pub fn maintenance_db() -> String {
    dev_url()
        .rsplit_once('/')
        .map(|(_, db)| db.to_owned())
        .expect("url has a dbname")
}

/// The database shared by the tests that need a working connection but
/// no private schema (connecting, timeouts, error mapping).
pub const SHARED_DB: &str = "forge_test_shared";

static SHARED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Create (once per cluster) and re-bootstrap (once per process) the shared
/// database; concurrent test processes serialize on an advisory lock.
async fn ensure_shared() {
    ensure_cluster().await;
    SHARED
        .get_or_init(|| async {
            let mut admin = superuser(&maintenance_db()).await;
            sqlx::query("SELECT pg_advisory_lock(hashtext('forge.test.shared_db'))")
                .execute(&mut admin)
                .await
                .expect("shared db lock");
            // None, or the owner is (not) a superuser: one created by an
            // older bootstrap is recreated.
            let owner_is_superuser: Option<bool> = sqlx::query_scalar(
                "SELECT r.rolsuper FROM pg_database d JOIN pg_roles r ON r.oid = d.datdba \
                 WHERE d.datname = $1",
            )
            .bind(SHARED_DB)
            .fetch_optional(&mut admin)
            .await
            .expect("shared db owner");
            if owner_is_superuser == Some(false) {
                sqlx::query(&format!("DROP DATABASE \"{SHARED_DB}\" WITH (FORCE)"))
                    .execute(&mut admin)
                    .await
                    .expect("drop outdated shared db");
            }
            if owner_is_superuser != Some(true) {
                let create = bootstrap_sql(1).replace(":\"dbname\"", &format!("\"{SHARED_DB}\""));
                sqlx::raw_sql(&create)
                    .execute(&mut admin)
                    .await
                    .expect("bootstrap step 2 (shared)");
            }
            let cluster = ClusterLock::shared().await;
            let mut owner = superuser(SHARED_DB).await;
            sqlx::raw_sql(&bootstrap_sql(2))
                .execute(&mut owner)
                .await
                .expect("bootstrap step 3 (shared)");
            drop(owner);
            drop(cluster);
            sqlx::query("SELECT pg_advisory_unlock(hashtext('forge.test.shared_db'))")
                .execute(&mut admin)
                .await
                .expect("shared db unlock");
        })
        .await;
}

/// forge_app on the shared bootstrapped database (for tests about
/// connecting, timeouts and error mapping).
pub async fn dev_db(extra: &[(&str, &str)]) -> (Db, TestConfig) {
    ensure_shared().await;
    let cfg = test_config(&url_as("forge_app", SHARED_DB), APP_PASSWORD, extra);
    match Db::connect(&cfg.db).await {
        Ok(db) => (db, cfg),
        Err(e) => panic!("{HINT}\n{e}"),
    }
}

/// SQL run on the maintenance database when this value is dropped — also
/// when the test panics — to undo a cluster-wide change. Runs on its own
/// thread and runtime (Drop cannot await).
pub struct Restore(pub Vec<String>);

impl Drop for Restore {
    fn drop(&mut self) {
        let statements = std::mem::take(&mut self.0);
        let worker = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("restore runtime");
            rt.block_on(async {
                let mut conn = superuser(&maintenance_db()).await;
                for sql in &statements {
                    if let Err(e) = sqlx::raw_sql(sql).execute(&mut conn).await {
                        // Reported, not panicked: a panic in Drop during an
                        // unwind would abort the test binary.
                        eprintln!("restore failed: {sql}: {e}");
                    }
                }
            });
        });
        if worker.join().is_err() {
            eprintln!("restore thread panicked");
        }
    }
}

/// A cluster-wide advisory lock on the maintenance database (held while
/// this value lives).
///
/// The shared lock is reentrant per test process: every holder in the
/// process shares one database session. PostgreSQL queues a new shared
/// request behind a waiting exclusive one, so a test that holds the shared
/// lock and asks for it again (FreshDb::new inside a guarded test) would
/// otherwise deadlock with an exclusive waiter. An exclusive holder must not
/// ask for the shared lock (create its FreshDb before taking it).
pub enum ClusterLock {
    Shared,
    Exclusive { _held: PgConnection },
}

/// The process's shared-lock session and its number of holders.
static SHARED_HOLD: std::sync::Mutex<(usize, Option<PgConnection>)> =
    std::sync::Mutex::new((0, None));

impl ClusterLock {
    const KEY: &'static str = "forge.test.cluster_roles";

    /// For tests that run the role guards (and step 3).
    pub async fn shared() -> Self {
        if Self::join_shared() {
            return Self::Shared;
        }
        let conn = Self::take("pg_advisory_lock_shared").await;
        let mut hold = SHARED_HOLD.lock().expect("shared lock state");
        hold.0 += 1;
        if hold.1.is_none() {
            hold.1 = Some(conn);
        }
        // Otherwise another task got there first: `conn` is dropped (its
        // session, and its share of the lock, end with it).
        Self::Shared
    }

    fn join_shared() -> bool {
        let mut hold = SHARED_HOLD.lock().expect("shared lock state");
        if hold.0 > 0 {
            hold.0 += 1;
            return true;
        }
        false
    }

    /// For tests that change a cluster role.
    pub async fn exclusive() -> Self {
        Self::Exclusive {
            _held: Self::take("pg_advisory_lock").await,
        }
    }

    async fn take(function: &str) -> PgConnection {
        ensure_cluster().await;
        let mut conn = superuser(&maintenance_db()).await;
        sqlx::query(&format!("SELECT {function}(hashtext($1))"))
            .bind(Self::KEY)
            .execute(&mut conn)
            .await
            .expect("cluster lock");
        conn
    }
}

impl Drop for ClusterLock {
    fn drop(&mut self) {
        if let Self::Shared = self {
            let mut hold = SHARED_HOLD.lock().expect("shared lock state");
            hold.0 = hold.0.saturating_sub(1);
            if hold.0 == 0 {
                // Dropping the session releases the lock on the server.
                hold.1 = None;
            }
        }
    }
}

/// One CREATE DATABASE at a time per test process: concurrent ones contend
/// on the template database and slow every other test.
static CREATE_TURN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// A bootstrapped database created for one test (`forge_t_<uuid>`), dropped
/// when the value goes away — after [`FreshDb::drop_now`], at the end of the
/// test, or while a failing test unwinds. Only a killed test process leaks
/// one; the prefix makes such leftovers easy to find (`SELECT datname FROM
/// pg_database WHERE datname LIKE 'forge_t_%'`).
pub struct FreshDb {
    /// Logged in as forge_app.
    pub db: Db,
    pub name: String,
    /// Keeps the forge_app password file alive as long as the pool.
    _app: TestConfig,
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
            let create = bootstrap_sql(1).replace(":\"dbname\"", &format!("\"{name}\""));
            sqlx::raw_sql(&create)
                .execute(&mut admin)
                .await
                .expect("bootstrap step 2 (02-create-database.sql)");
            drop(admin);
        }
        // Step 3 resets role-wide settings of the bootstrap roles: never while
        // a test changes a role.
        let cluster = ClusterLock::shared().await;
        let mut owner = superuser(&name).await;
        sqlx::raw_sql(&bootstrap_sql(2))
            .execute(&mut owner)
            .await
            .expect("bootstrap step 3 (03-database-objects.sql)");
        owner.close().await.expect("close");
        drop(cluster);
        let app = test_config(&url_as("forge_app", &name), APP_PASSWORD, &[]);
        let db = Db::connect(&app.db).await.expect("connect as forge_app");
        Self {
            db,
            name,
            _app: app,
        }
    }

    /// A fresh database with the product migrations applied (as
    /// forge_migrator, like `forge db migrate`).
    pub async fn migrated() -> Self {
        let t = Self::new().await;
        let mut mc = Db::connect_migrator(&t.migrate_config(&[]).db)
            .await
            .expect("migrator");
        crate::migrate(
            &mut mc,
            &crate::MIGRATOR,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("migrate");
        mc.close().await;
        t
    }

    /// One transaction as forge_app: the audit row of `cause`
    /// (`(resource_type, resource_id)`), then `statements`, then COMMIT
    /// (where the txn-audited triggers fire). For fixtures no command
    /// writes yet — D5's moves and tombstones (forge-web's
    /// `namespace_tombstone_routes`, M1 phase 8 MR4).
    pub async fn commit_audited(&self, cause: (&str, i64), statements: &[String]) {
        let mut tx = self.db.write_pool().begin().await.expect("begin");
        let audit = format!(
            "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
             resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', '{}', {}, \
             'r-fixture')",
            cause.0, cause.1
        );
        for sql in std::iter::once(&audit).chain(statements) {
            sqlx::raw_sql(sql)
                .execute(&mut *tx)
                .await
                .unwrap_or_else(|e| panic!("fixture statement failed: {e}"));
        }
        tx.commit().await.expect("commit");
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
    /// Run SQL as the superuser on this database (for crates without sqlx).
    pub async fn exec_superuser(&self, sql: &str) {
        sqlx::raw_sql(sql)
            .execute(&mut self.superuser().await)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    pub async fn superuser(&self) -> PgConnection {
        superuser(&self.name).await
    }

    /// The rows of `sql` as JSON objects, read as the superuser (to inspect
    /// from crates without sqlx).
    pub async fn json_rows(&self, sql: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar(&format!("SELECT pg_catalog.to_jsonb(q) FROM ({sql}) AS q"))
            .fetch_all(&mut self.superuser().await)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// Close the pool cleanly, then drop the database (`Drop`).
    pub async fn drop_now(self) {
        self.db.close().await;
    }
}

/// How long dropping a test database may take (connect + DROP).
const DROP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Drop for FreshDb {
    /// `Drop` cannot await, so a helper thread drops the database on its own
    /// runtime (`WITH (FORCE)` ends connections the pool still holds), within
    /// [`DROP_TIMEOUT`]. The calling thread blocks meanwhile (on a
    /// multi-thread test runtime, one worker stalls for the drop). A failure
    /// panics, except while the test already unwinds (a second panic would
    /// abort): then it is printed.
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        // clone: the helper thread needs its own copy; `name` stays for the message.
        let target = name.clone();
        let dropped = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(async {
                tokio::time::timeout(DROP_TIMEOUT, async {
                    let mut admin = superuser(&maintenance_db()).await;
                    sqlx::query(&format!(
                        "DROP DATABASE IF EXISTS \"{target}\" WITH (FORCE)"
                    ))
                    .execute(&mut admin)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
                })
                .await
                .unwrap_or_else(|_| Err(format!("timed out after {DROP_TIMEOUT:?}")))
            })
        })
        .join()
        .unwrap_or_else(|_| Err("the drop thread panicked".to_owned()));
        if let Err(e) = dropped {
            if std::thread::panicking() {
                eprintln!("FreshDb: could not drop test database {name}: {e}");
            } else {
                panic!("FreshDb: could not drop test database {name}: {e}");
            }
        }
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
