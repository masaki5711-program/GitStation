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

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_core::readiness::ReadinessProbe;
use forge_core::Authenticator;
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use forge_web::{AppConfig, AppState, BoundedAuthenticator, Lookups, ServerLimits};

use crate::runtime::block_on;

pub fn serve(bind: &str, database: &DatabaseConfig) -> Result<String, ForgeError> {
    // The server process must not hold the migration or maintenance
    // credentials (ADR-0018): their password files belong to other configs.
    database.reject_privileged_logins()?;
    block_on(async {
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        let state = app_state(&db, database);
        let router = forge_web::app(state, AppConfig::default())?;
        let listener = forge_web::server::bind(bind).await?;
        tracing::info!(
            bind,
            version = env!("CARGO_PKG_VERSION"),
            db_target = %db.target(),
            "forge.serve.listening"
        );
        forge_web::server::serve(
            listener,
            router,
            ServerLimits::default(),
            forge_web::server::shutdown_signal(),
        )
        .await?;
        db.close().await;
        tracing::info!("forge.serve.stopped");
        Ok(String::new())
    })
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
