//! Binding and serving (`forge serve`).
//!
//! The connection loop is built on hyper directly (not `axum::serve`) so
//! that every connection is bounded (security review, slowloris):
//! - a header-read timeout (also bounds idle keep-alive connections),
//! - a maximum number of concurrent connections,
//! - a graceful-shutdown deadline.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use forge_core::{telemetry, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// Limits of the HTTP server.
#[derive(Debug, Clone, Copy)]
pub struct ServerLimits {
    /// Time a client has to send the request headers (and the idle time
    /// allowed between keep-alive requests).
    pub header_read_timeout: Duration,
    /// Concurrent connections; further clients wait in the OS backlog.
    pub max_connections: usize,
    /// How long in-flight requests may run after shutdown is requested.
    pub shutdown_grace: Duration,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(30),
            max_connections: 1024,
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

/// Bind the listen address; failure is `FG-SYS-0004` with the OS error.
pub async fn bind(addr: &str) -> Result<TcpListener, ForgeError> {
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        ForgeError::new(ErrorCode::ServerBindFailed)
            .with_context("bind", addr)
            .with_source(e)
    })?;
    if let Ok(local) = listener.local_addr() {
        if !local.ip().is_loopback() {
            // Not a failure: a reminder that TLS is the reverse proxy's job.
            tracing::warn!(
                target: "forge_web::server",
                bind = %local,
                "listening on a non-loopback address: put the TLS reverse proxy in front"
            );
        }
    }
    Ok(listener)
}

fn serve_failed(operation: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ServeFailed).with_context("operation", operation)
}

/// Serve until `shutdown` resolves; then stop accepting, let in-flight
/// requests finish within `limits.shutdown_grace`, and return.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    limits: ServerLimits,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ForgeError> {
    let slots = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let graceful = GracefulShutdown::new();
    let service = TowerToHyperService::new(router);
    let mut shutdown = std::pin::pin!(shutdown);

    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            permit = Arc::clone(&slots).acquire_owned() => match permit {
                Ok(p) => p,
                // The semaphore is never closed; treat it as a fatal bug.
                Err(e) => return Err(serve_failed("connection_limit").with_source(e)),
            },
        };
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok((stream, _peer)) => stream,
            Err(e) => {
                // Usually out of file descriptors: log with a code, pause, retry.
                let err = ForgeError::new(ErrorCode::AcceptFailed)
                    .with_context("operation", "accept")
                    .with_source(e);
                telemetry::log_failure(&err, &RequestId::generate());
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let conn = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_read_timeout)
            // No HTTP upgrades: git Smart HTTP does not use them, and plain
            // connections take part in graceful shutdown.
            .serve_connection(TokioIo::new(stream), service.clone());
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            // Client-side errors (reset, timeout while reading headers) are
            // not server failures; they are visible at debug level.
            if let Err(e) = conn.await {
                tracing::debug!(target: "forge_web::server", error = %e, "http.connection_closed");
            }
            drop(permit);
        });
    }

    drop(listener);
    tokio::select! {
        () = graceful.shutdown() => Ok(()),
        () = tokio::time::sleep(limits.shutdown_grace) => Err(serve_failed("shutdown_deadline")
            .with_context("grace_ms", limits.shutdown_grace.as_millis())),
    }
}

/// Resolves on Ctrl-C (SIGINT) or, on Unix, SIGTERM (`systemctl stop`).
/// If no handler can be installed the failure is logged with
/// `FG-SYS-0006` and the server keeps running (it never stops on its own).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            report_signal_failure("SIGINT", e);
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                report_signal_failure("SIGTERM", e);
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

/// The error logged when a signal handler cannot be installed.
pub fn signal_failure(signal: &'static str, e: std::io::Error) -> ForgeError {
    ForgeError::new(ErrorCode::SignalHandlerUnavailable)
        .with_context("signal", signal)
        .with_source(e)
}

fn report_signal_failure(signal: &'static str, e: std::io::Error) {
    telemetry::log_failure(&signal_failure(signal, e), &RequestId::generate());
}
