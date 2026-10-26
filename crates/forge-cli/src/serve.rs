//! `forge serve` — run the HTTP server (Web, REST API; Smart HTTP from M1
//! phase 9) until Ctrl-C / SIGINT, then finish in-flight requests.

use forge_errors::{ErrorCode, ForgeError};
use forge_web::{AppConfig, AppState};

pub fn serve(bind: &str) -> Result<String, ForgeError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
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
        forge_web::server::serve(listener, router, shutdown_signal()).await?;
        tracing::info!("forge.serve.stopped");
        Ok(String::new())
    })
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        // Without a signal handler the server keeps running; it can still be
        // stopped by the service manager. Never shut down on this error.
        tracing::warn!(error = %e, "forge.serve.no_signal_handler");
        std::future::pending::<()>().await;
    }
}
