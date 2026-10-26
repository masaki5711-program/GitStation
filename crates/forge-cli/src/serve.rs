//! `forge serve` — run the HTTP server (Web, REST API; Smart HTTP from M1
//! phase 9) until SIGINT / SIGTERM, then finish in-flight requests within the
//! shutdown grace period.
//!
//! Before binding, the database is connected and its schema checked
//! (ADR-0014): pending, newer, edited or dirty migrations stop the start
//! with `FG-DB-0004` — `serve` never migrates.

use std::sync::Arc;
use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_core::readiness::ReadinessProbe;
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use forge_web::{AppConfig, AppState, ServerLimits};

use crate::runtime::block_on;

pub fn serve(bind: &str, database: &DatabaseConfig) -> Result<String, ForgeError> {
    block_on(async {
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        let probe: Arc<dyn ReadinessProbe> = Arc::new(db.clone()); // clone: the pool handle is shared (Arc inside)
        let state = AppState::new(env!("CARGO_PKG_VERSION"), vec![probe])
            .with_probe_timeout(probe_timeout_for(database));
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
        db.pool().close().await;
        tracing::info!("forge.serve.stopped");
        Ok(String::new())
    })
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
}
