//! `forge serve` — run the HTTP server (Web, REST API; Smart HTTP from M1
//! phase 9) until SIGINT / SIGTERM, then finish in-flight requests within the
//! shutdown grace period.
//!
//! Before binding, the database is connected and its schema checked
//! (ADR-0014): pending, newer, edited or dirty migrations stop the start
//! with `FG-DB-0004` — `serve` never migrates.
//!
//! The database is injected into forge-web twice through forge-core traits
//! (forge-web does not depend on forge-db): as the readiness probe and as
//! the token authenticator (`forge_db::tokens::authenticate`, ADR-0019 §7),
//! and as the dispatcher's lookups (`forge_db::resolve`: path → namespace
//! and project by id, ADR-0015 MR3 / MR4).
//!
//! Smart HTTP (M1 phase 9 MR3): `storage.root` is required; git runs from
//! `PATH` with HOME pinned to the storage root (as the worker does), its
//! stream pools and timeouts from `git.http.*`, the version checked
//! (≥ 2.38, `FG-GIT-0001`) before the server binds.

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::{Config, DatabaseConfig, GitHttpConfig};
use forge_core::readiness::ReadinessProbe;
use forge_core::Authenticator;
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use forge_git::{Git, GitConfig, StorageRoot};
use forge_web::{
    AppConfig, AppState, BoundedAuthenticator, GitHttpLimits, Lookups, ServerLimits, SmartHttp,
};

use crate::runtime::block_on;

pub fn serve(bind: &str, cfg: &Config) -> Result<String, ForgeError> {
    let database = &cfg.database;
    // The server process must not hold the migration or maintenance
    // credentials (ADR-0018): their password files belong to other configs.
    database.reject_privileged_logins()?;
    let storage = StorageRoot::open(cfg.storage_root()?)?;
    block_on(async {
        let smart_http = smart_http(storage, &cfg.git_http);
        forge_git::version::detect(&smart_http.git).await?;
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        // Pushes load their protection snapshot from, and are recorded in,
        // the database (forge-web reads both through forge-core traits).
        let smart_http = smart_http.with_push(Arc::new(db.clone()), Arc::new(db.clone()));
        let push_tasks = smart_http.push_tasks();
        let state = app_state(&db, database).with_smart_http(smart_http);
        let router = forge_web::app(
            state,
            app_config_for(database, &cfg.git_http, &cfg.server.limits),
        )?;
        let listener = forge_web::server::bind(bind).await?;
        tracing::info!(
            bind,
            version = env!("CARGO_PKG_VERSION"),
            db_target = %db.target(),
            "forge.serve.listening"
        );
        let served = forge_web::server::serve(
            listener,
            router,
            ServerLimits::from_config(&cfg.server.limits),
            forge_web::server::shutdown_signal(),
        )
        .await;
        // A receive-pack whose body was complete runs on in its own task
        // (and records the push afterwards): let it finish before the
        // runtime and the database pool go away.
        if !push_tasks.wait_idle(PUSH_SHUTDOWN_WAIT).await {
            forge_core::telemetry::log_failure(
                &forge_errors::ForgeError::new(forge_errors::ErrorCode::ServeFailed)
                    .with_context("operation", "push_tasks_pending")
                    .with_context("running", push_tasks.running())
                    .with_context("wait_ms", PUSH_SHUTDOWN_WAIT.as_millis()),
                &forge_core::RequestId::generate(),
            );
        }
        db.close().await;
        served?;
        tracing::info!("forge.serve.stopped");
        Ok(String::new())
    })
}

/// How long a shutdown waits for running pushes (their git, then their
/// record: 30 s of retries at most).
const PUSH_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// git for the Smart HTTP routes: `git` from `PATH`, HOME = the storage
/// root (only forge writes it; no user or shared config applies), stream
/// pools and timeouts from `git.http.*`.
fn smart_http(storage: StorageRoot, limits: &GitHttpConfig) -> SmartHttp {
    let mut git = GitConfig::new("git", storage.path());
    // u32 → usize: lossless on the supported (32/64-bit) targets.
    git.max_streams = usize::try_from(limits.max_streams).unwrap_or(usize::MAX);
    git.max_streams_per_principal =
        usize::try_from(limits.max_streams_per_principal).unwrap_or(usize::MAX);
    git.stream_idle_timeout = limits.idle_timeout;
    git.stream_timeout_total = limits.timeout;
    SmartHttp::new(Git::new(git), storage, GitHttpLimits::from_config(limits))
}

/// The server's state: the database as readiness probe and as token
/// authenticator, bounded to [`auth_permits`] concurrent lookups.
fn app_state(db: &Db, database: &DatabaseConfig) -> AppState {
    // clone: the pool handle is shared (Arc inside)
    let probe: Arc<dyn ReadinessProbe> = Arc::new(db.clone());
    let authenticator: Arc<dyn Authenticator> = Arc::new(BoundedAuthenticator::new(
        Arc::new(db.clone()),
        auth_permits(database),
    ));
    let lookups = Lookups {
        namespaces: Arc::new(db.clone()),
        projects: Arc::new(db.clone()),
    };
    AppState::new(
        env!("CARGO_PKG_VERSION"),
        vec![probe],
        authenticator,
        lookups,
    )
    .with_probe_timeout(probe_timeout_for(database))
    .with_lookup_timeout(lookup_timeout_for(database))
    // The group / project writes (M1 phase 8b MR7a).
    .with_hosting(forge_hosting::Hosting::new(db.clone()))
}

/// One lookup is one statement on the read pool: acquire, then the
/// statement (bounded by the session's `statement_timeout`). The client
/// bound exceeds both, so the database reports its own cause first.
fn lookup_timeout_for(database: &DatabaseConfig) -> Duration {
    database
        .acquire_timeout
        .saturating_add(database.statement_timeout)
        .saturating_add(Duration::from_secs(1))
}

/// One token authentication is one `Db::within` operation on the write
/// pool, bounded on the client by acquire + statement timeout (the same
/// bound `Db` uses); it exceeds neither, so `FG-DB-0001` names the cause.
fn auth_timeout_for(database: &DatabaseConfig) -> Duration {
    database
        .acquire_timeout
        .saturating_add(database.statement_timeout)
}

/// A dispatched request runs one authentication and then one lookup, one
/// after the other, before its handler. The request timeout
/// (`FG-WEB-0002`) must not fire before either database bound does, or the
/// log loses the database's own cause (`FG-DB-0001` with its reason): it is
/// at least the default (30 s) and always exceeds both bounds together by
/// one second. Derived instead of refused, because the defaults (acquire
/// 3 s, statement 15 s → 18 s + 19 s) already exceed 30 s.
fn request_timeout_for(database: &DatabaseConfig) -> Duration {
    auth_timeout_for(database)
        .saturating_add(lookup_timeout_for(database))
        .saturating_add(Duration::from_secs(1))
        .max(AppConfig::default().request_timeout)
}

/// The application config `forge serve` runs with. git POSTs get
/// `git.http.timeout_ms` instead of the request timeout (they read the
/// whole request first, under their own idle and size limits).
fn app_config_for(
    database: &DatabaseConfig,
    git_http: &GitHttpConfig,
    server: &forge_core::config::ServerLimitsConfig,
) -> AppConfig {
    AppConfig {
        request_timeout: request_timeout_for(database),
        git_rpc_timeout: git_http.timeout,
        body_read_timeout: server.body_read_timeout,
        ..AppConfig::default()
    }
}

/// Token lookups run on the write pool (ADR-0018: only it can read the
/// hashes); at most half of its connections may serve them, so a flood of
/// random tokens cannot starve writers. At least one.
fn auth_permits(database: &DatabaseConfig) -> usize {
    // A u32 always fits a usize on the supported (32/64-bit) targets.
    usize::try_from(database.write_max_connections / 2)
        .unwrap_or(usize::MAX)
        .max(1)
}

/// The readiness bound must exceed what the database probe can take on its
/// own (acquire, then the ping round trip, each up to acquire_timeout), so
/// /readyz reports FG-DB-0001 with its reason instead of FG-WEB-0005.
fn probe_timeout_for(database: &DatabaseConfig) -> Duration {
    database
        .acquire_timeout
        .saturating_mul(2)
        .saturating_add(Duration::from_secs(1))
}

#[cfg(test)]
mod tests {
    use forge_core::config::{load, Env};

    use super::*;

    #[test]
    fn the_readiness_bound_exceeds_the_database_probe_bounds() {
        let cfg = load(
            None,
            &Env::from_pairs([("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "4000")]),
        )
        .expect("config");
        let bound = probe_timeout_for(&cfg.database);
        assert!(bound > cfg.database.acquire_timeout * 2, "{bound:?}");
    }

    #[test]
    fn the_lookup_bound_exceeds_acquire_plus_statement() {
        let cfg = load(None, &Env::from_pairs([])).expect("config");
        let bound = lookup_timeout_for(&cfg.database);
        assert!(
            bound > cfg.database.acquire_timeout + cfg.database.statement_timeout,
            "{bound:?}"
        );
    }

    /// The request timeout never fires before the database bounds of the
    /// authentication and the lookup of one dispatched request, for the
    /// defaults and for large statement timeouts; it never drops below
    /// the 30 s default.
    #[test]
    fn the_request_bound_exceeds_authentication_plus_lookup() {
        let configs = [
            vec![],
            vec![("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "60000")],
            vec![
                ("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "10000"),
                ("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "120000"),
            ],
            vec![
                ("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "100"),
                ("FORGE_DATABASE_STATEMENT_TIMEOUT_MS", "200"),
            ],
        ];
        for pairs in configs {
            let cfg = load(None, &Env::from_pairs(pairs.clone())).expect("config");
            let db = &cfg.database;
            let request = app_config_for(db, &cfg.git_http, &cfg.server.limits).request_timeout;
            let needed = auth_timeout_for(db) + lookup_timeout_for(db);
            assert!(request > needed, "{pairs:?}: {request:?} <= {needed:?}");
            assert!(
                request >= AppConfig::default().request_timeout,
                "{pairs:?}: {request:?}"
            );
            assert!(
                auth_timeout_for(db) >= db.acquire_timeout + db.statement_timeout,
                "{pairs:?}"
            );
        }
        // The defaults alone would not fit the fixed 30 s.
        let defaults = load(None, &Env::from_pairs([])).expect("config");
        assert!(
            auth_timeout_for(&defaults.database) + lookup_timeout_for(&defaults.database)
                > AppConfig::default().request_timeout
        );
    }

    #[test]
    fn authentication_may_use_at_most_half_of_the_write_pool() {
        for (write_max, permits) in [("10", 5), ("3", 1), ("1", 1), ("30", 15)] {
            let cfg = load(
                None,
                &Env::from_pairs([
                    ("FORGE_DATABASE_WRITE_MAX_CONNECTIONS", write_max),
                    ("FORGE_DATABASE_MIN_CONNECTIONS", "1"),
                ]),
            )
            .expect("config");
            assert_eq!(
                auth_permits(&cfg.database),
                permits,
                "write_max={write_max}"
            );
        }
    }

    /// `forge serve` authenticates with the real token store: a token issued
    /// in the database resolves to its owner through the state's
    /// authenticator, and an unknown one is FG-AUTH-0001.
    #[tokio::test]
    async fn serve_wires_the_database_token_authenticator() {
        use forge_core::{RequestId, Scope};
        use forge_db::change::{Actor, ChangeContext};
        use forge_db::test_support::FreshDb;
        use forge_db::tokens::{self, NewToken};
        use forge_db::users::{self, NewUser};
        use forge_errors::ErrorCode;

        let t = FreshDb::migrated().await;
        let rid = |s: &str| RequestId::parse(s).expect("request id");
        let ctx = ChangeContext::new(Actor::System, rid("req-serve-user"));
        let alice = users::create(
            &t.db,
            &ctx,
            &NewUser::human("alice", "Alice", "alice@example.test"),
        )
        .await
        .expect("user")
        .user_id;
        let new = NewToken {
            user: alice,
            name: "laptop".to_owned(),
            scopes: vec![Scope::ReadRepository],
            expires_in: None,
            is_agent: false,
        };
        let issued = tokens::issue(
            &t.db,
            &ChangeContext::new(Actor::System, rid("req-serve-tok")),
            &new,
        )
        .await
        .expect("issue");
        let cfg = load(
            None,
            &Env::from_pairs([("FORGE_DATABASE_ACQUIRE_TIMEOUT_MS", "4000")]),
        )
        .expect("config");
        let state = app_state(&t.db, &cfg.database);
        let identity = state
            .authenticator
            .authenticate(&rid("req-serve-auth"), &issued.token.reveal())
            .await
            .expect("the issued token authenticates");
        assert_eq!(identity.user_id, alice);
        assert_eq!(identity.scopes, vec![Scope::ReadRepository]);
        let unknown =
            "fgp_00000000000000ff_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let err = state
            .authenticator
            .authenticate(&rid("req-serve-unknown"), unknown)
            .await
            .expect_err("unknown token");
        assert_eq!(err.code(), ErrorCode::Unauthenticated);
        let probes: Vec<&str> = state.probes.iter().map(|p| p.name()).collect();
        assert_eq!(probes, ["database"]);
        t.drop_now().await;
    }
}
