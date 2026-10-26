//! `forge serve` — run the HTTP server (Web, REST API; Smart HTTP from M1
//! phase 9) until SIGINT / SIGTERM, then finish in-flight requests within the
//! shutdown grace period.

use forge_errors::{ErrorCode, ForgeError};
use forge_web::{AppConfig, AppState, ServerLimits};

pub fn serve(bind: &str) -> Result<String, ForgeError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ForgeError::new(ErrorCode::ServeFailed)
                .with_context("operation", "build_runtime")
                .with_source(e)
        })?;
    runtime.block_on(async {
        let state = AppState {
            version: env!("CARGO_PKG_VERSION"),
        };
        let router = forge_web::app(state, AppConfig::default())?;
        let listener = forge_web::server::bind(bind).await?;
        tracing::info!(
            bind,
            version = env!("CARGO_PKG_VERSION"),
            "forge.serve.listening"
        );
        forge_web::server::serve(
            listener,
            router,
            ServerLimits::default(),
            forge_web::server::shutdown_signal(),
        )
        .await?;
        tracing::info!("forge.serve.stopped");
        Ok(String::new())
    })
}
