//! The async runtime for commands that need one (serve, db).

use std::future::Future;

use forge_errors::{ErrorCode, ForgeError};

/// Run `fut` on a fresh multi-threaded runtime; failing to build it is
/// `FG-SYS-0005 operation=build_runtime`.
pub fn block_on<T>(fut: impl Future<Output = Result<T, ForgeError>>) -> Result<T, ForgeError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ForgeError::new(ErrorCode::ServeFailed)
                .with_context("operation", "build_runtime")
                .with_source(e)
        })?
        .block_on(fut)
}
