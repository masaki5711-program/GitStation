//! `forge serve` — run the HTTP server (Web, REST API; Smart HTTP from M1
//! phase 9) until SIGINT / SIGTERM, then finish in-flight requests within the
//! shutdown grace period.
//!
//! Before binding, the database is connected and its schema checked
//! (ADR-0014): pending, newer, edited or dirty migrations stop the start
//! with `FG-DB-0004` — `serve` never migrates.

use forge_core::config::DatabaseConfig;
use forge_db::{Db, MIGRATOR};
use forge_errors::ForgeError;
use forge_web::{AppConfig, AppState, ServerLimits};

use crate::runtime::block_on;

pub fn serve(bind: &str, database: &DatabaseConfig) -> Result<String, ForgeError> {
    block_on(async {
        let db = Db::connect(database).await?;
        forge_db::check_schema(&db, &MIGRATOR).await?;
        let state = AppState {
            version: env!("CARGO_PKG_VERSION"),
        };
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
