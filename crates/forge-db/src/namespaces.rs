//! What every command that writes a namespace path shares (step D1 create;
//! D5 rename, move, delete and tombstone release).

use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::query_error;

/// Take the transaction-scoped advisory lock of a full path; it is held until
/// the transaction ends. Every command that creates a namespace at a path, or
/// writes or releases a tombstone for it, takes this lock before it looks at
/// the path, so a create never misses a tombstone written concurrently
/// (ADR-0015 Amendment D1). The key is `hashtextextended('namespace:' ||
/// path, 0)`: a hash collision only serializes two unrelated writers.
pub(crate) async fn lock_path(conn: &mut PgConnection, path: &str) -> Result<(), ForgeError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('namespace:' || $1, 0))")
        .bind(path)
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|e| query_error("namespaces.lock_path", e))
}

/// Whether a tombstone blocks the path (exact match, ADR-0015).
pub(crate) async fn is_tombstoned(conn: &mut PgConnection, path: &str) -> Result<bool, ForgeError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.namespace_tombstones WHERE old_path = $1)",
    )
    .bind(path)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("namespaces.tombstone", e))
}

/// `FG-REPO-0009`: the path is used by a sibling (`reason=sibling`) or
/// blocked by a tombstone (`reason=tombstoned`); never the path itself.
pub(crate) fn path_taken(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NamespacePathTaken).with_context("reason", reason)
}
