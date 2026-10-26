//! Logging setup and the single place failures are logged.
//!
//! Conventions: `observability` skill. Production uses JSON lines so a log
//! excerpt or diag bundle can be filtered by `request_id` / `error_code`.

use forge_errors::{ForgeError, Severity};
use tracing_subscriber::{fmt, EnvFilter};

use crate::RequestId;

/// Output format for log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line (production).
    Json,
    /// Human-readable (development).
    Pretty,
}

/// Install the global subscriber. `RUST_LOG` overrides `default_filter`.
/// Returns `false` if a subscriber was already installed (e.g. in tests).
///
/// Logs go to **stderr**: stdout carries only command output, so
/// `forge doctor --json | jq` and `forge errors list | head` stay parseable
/// (systemd / journald captures both streams).
pub fn init(format: LogFormat, default_filter: &str) -> bool {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let builder = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .try_init()
            .is_ok(),
        LogFormat::Pretty => builder.try_init().is_ok(),
    }
}

/// Log a handled failure **once**, at the boundary that handles it
/// (`forge-error-contract.md`). Level follows the code's severity.
pub fn log_failure(err: &ForgeError, request_id: &RequestId) {
    let code = err.code();
    let context = err
        .context()
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let source = std::error::Error::source(err)
        .map(ToString::to_string)
        .unwrap_or_default();
    match code.severity() {
        Severity::Alert => tracing::error!(
            error_code = code.id(), request_id = %request_id, context = %context, source = %source,
            "{}", code.entry().cause
        ),
        Severity::Warn => tracing::warn!(
            error_code = code.id(), request_id = %request_id, context = %context, source = %source,
            "{}", code.entry().cause
        ),
        Severity::Info => tracing::info!(
            error_code = code.id(), request_id = %request_id, context = %context,
            "{}", code.entry().cause
        ),
    }
}
