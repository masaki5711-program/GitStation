//! What every command that writes a namespace path shares (step D1 create;
//! D5 rename, move, delete and tombstone release).

use forge_core::ids::NamespaceId;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::query_error;

mod moves;

pub(crate) use moves::{lock_subtree, lock_target, move_subtree};
pub use moves::{
    Destination, MoveOptions, MoveOutcome, RateLimit, TargetAccess, MAX_MOVES_PER_DAY, MAX_SUBTREE,
};

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

/// [`lock_path`] for many paths in one statement, in the byte order of
/// the paths (every multi-path writer locks in that order, so two of them
/// never deadlock on each other's paths).
pub(crate) async fn lock_paths(
    conn: &mut PgConnection,
    paths: &[String],
) -> Result<(), ForgeError> {
    let mut sorted: Vec<&str> = paths.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sqlx::query(
        "SELECT pg_catalog.count(pg_advisory_xact_lock(hashtextextended('namespace:' || p, 0))) \
         FROM unnest($1::text[]) WITH ORDINALITY AS t(p, n)",
    )
    .bind(&sorted)
    .execute(conn)
    .await
    .map(|_| ())
    .map_err(|e| query_error("namespaces.lock_paths", e))
}

/// After a create took the path lock of `path` (`<parent path>/<segment>`):
/// the parent still has the path the create computed (M1 D5 MR2). The
/// parent row is held `FOR SHARE`, so a move cannot change it before the
/// create commits; this is the check that the path the create locked is the
/// path it inserts. A mismatch is `FG-REPO-0009 reason=parent_moved`
/// (retry: the parent moved before the lock was taken).
pub(crate) async fn check_parent_path(
    conn: &mut PgConnection,
    parent: NamespaceId,
    path: &str,
) -> Result<(), ForgeError> {
    let current: Option<String> =
        sqlx::query_scalar("SELECT full_path || '/' FROM forge.namespaces WHERE id = $1")
            .bind(parent.get())
            .fetch_optional(conn)
            .await
            .map_err(|e| query_error("namespaces.parent_path", e))?;
    match current {
        Some(prefix)
            if path
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/')) =>
        {
            Ok(())
        }
        _ => Err(path_taken("parent_moved")),
    }
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

#[cfg(test)]
mod tests;
