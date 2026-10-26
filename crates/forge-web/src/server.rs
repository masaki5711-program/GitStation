//! Binding and serving (`forge serve`).

use std::future::Future;

use axum::Router;
use forge_errors::{ErrorCode, ForgeError};
use tokio::net::TcpListener;

/// Bind the listen address; failure is `FG-SYS-0004` with the OS error.
pub async fn bind(addr: &str) -> Result<TcpListener, ForgeError> {
    TcpListener::bind(addr).await.map_err(|e| {
        ForgeError::new(ErrorCode::ServerBindFailed)
            .with_context("bind", addr)
            .with_source(e)
    })
}

/// Serve until `shutdown` resolves, then finish in-flight requests.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ForgeError> {
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "serve")
                .with_source(e)
        })
}
