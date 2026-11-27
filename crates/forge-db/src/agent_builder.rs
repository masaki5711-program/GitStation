//! The agent builder's reads and records (plan 16 A7, ADR-0032 Amendment
//! A7): the published versions a build selects with their files' contents,
//! the stable catalog the recommendation picks from, and
//! `forge.agent_builds` (derived: a builder run of a project — its
//! selection, the lock's digest, a dry run or the merge request it opened;
//! deleted with the project by the purge).
//!
//! The parts' rows are only read here (they are frozen at publication,
//! A3 / A5). What a build reads is verified again by forge-agent's
//! `compile` (digest and signature), so a row changed in the database is
//! refused there, not trusted here.

use forge_core::{MergeRequestId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use crate::agent_marketplace::{version_sql, StoredVersion};
use crate::{query_error, read_query_error, Db};

/// The most bytes of file contents one build reads (all parts together).
pub const MAX_BUILD_BYTES: i64 = 8 * 1024 * 1024;
/// The most versions one read names.
pub const MAX_SELECTED: usize = 200;
/// The most stable versions the recommendation reads.
pub const MAX_CATALOG: i64 = 5000;

/// The versions `(slug, version)` named, published or revoked (the caller
/// refuses a revoked one); missing ones are absent from the answer.
pub async fn versions(
    db: &Db,
    wanted: &[(String, String)],
) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "agent_builder.versions";
    if wanted.len() > MAX_SELECTED {
        return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
            .with_context("reason", "too_many_parts")
            .with_context("count", wanted.len()));
    }
    let slugs: Vec<&str> = wanted.iter().map(|(s, _)| s.as_str()).collect();
    let versions: Vec<&str> = wanted.iter().map(|(_, v)| v.as_str()).collect();
    db.within(QUERY, async {
        sqlx::query_as(&version_sql(
            "(p.slug, v.version) IN (SELECT * FROM unnest($1::text[], $2::text[])) \
             ORDER BY p.slug, v.id",
        ))
        .bind(&slugs)
        .bind(&versions)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The files `(version_id, path, content)` of `version_ids`, by version
/// and path (byte order). More than [`MAX_BUILD_BYTES`] in all is
/// `FG-AGENT-0008 reason=too_large` (nothing read).
pub async fn contents(
    db: &Db,
    version_ids: &[i64],
) -> Result<Vec<(i64, String, Vec<u8>)>, ForgeError> {
    const QUERY: &str = "agent_builder.contents";
    db.within(QUERY, async {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT sum(octet_length(content))::bigint FROM forge.agent_part_files \
             WHERE version_id = ANY($1)",
        )
        .bind(version_ids)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        if total.unwrap_or(0) > MAX_BUILD_BYTES {
            return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
                .with_context("reason", "too_large")
                .with_context("bytes", total.unwrap_or(0)));
        }
        sqlx::query_as(
            "SELECT version_id, path, content FROM forge.agent_part_files \
             WHERE version_id = ANY($1) ORDER BY version_id, path COLLATE \"C\"",
        )
        .bind(version_ids)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// Every published (not revoked) stable version, by slug then id, at most
/// [`MAX_CATALOG`] (the recommendation's catalog).
pub async fn stable_versions(db: &Db) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "agent_builder.stable_versions";
    db.within(QUERY, async {
        sqlx::query_as(&version_sql(
            "v.state = 'published' AND v.channel = 'stable' AND p.deprecated_at IS NULL \
             ORDER BY p.slug, v.id LIMIT $1",
        ))
        .bind(MAX_CATALOG)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// A build to record.
#[derive(Debug, Clone)]
pub struct NewBuild<'a> {
    pub project: ProjectId,
    pub requested_by: UserId,
    pub adapter_version_id: Option<i64>,
    /// `[{"slug","version","selected_by","reason"}]`.
    pub selection: &'a serde_json::Value,
    pub dry_run: bool,
    pub request_id: &'a str,
}

/// How a build ended.
#[derive(Debug, Clone)]
pub enum BuildOutcome<'a> {
    Succeeded {
        lock_digest: &'a str,
        merge_request: Option<MergeRequestId>,
    },
    Failed {
        error_code: &'a str,
    },
}

/// Record `build` as it ended (one row; the table is derived, no audit).
pub async fn record_build(
    db: &Db,
    build: &NewBuild<'_>,
    outcome: &BuildOutcome<'_>,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "agent_builder.record_build";
    let (state, lock_digest, mr, error_code) = match outcome {
        BuildOutcome::Succeeded {
            lock_digest,
            merge_request,
        } => (
            "succeeded",
            Some(*lock_digest),
            merge_request.map(|m| m.get()),
            None,
        ),
        BuildOutcome::Failed { error_code } => ("failed", None, None, Some(*error_code)),
    };
    db.within(QUERY, async {
        sqlx::query_scalar(
            "INSERT INTO forge.agent_builds (project_id, requested_by, adapter_version_id, \
             selection, lock_digest, dry_run, mr_id, state, error_code, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id",
        )
        .bind(build.project.get())
        .bind(build.requested_by.get())
        .bind(build.adapter_version_id)
        .bind(build.selection)
        .bind(lock_digest)
        .bind(build.dry_run)
        .bind(mr)
        .bind(state)
        .bind(error_code)
        .bind(build.request_id)
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// One recorded build.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredBuild {
    pub id: i64,
    pub requested_by: i64,
    pub selection: serde_json::Value,
    pub lock_digest: Option<String>,
    pub dry_run: bool,
    pub mr_iid: Option<i64>,
    pub state: String,
    pub error_code: Option<String>,
    pub request_id: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// The project's newest builds, newest first, at most `limit` (1..=50).
pub async fn builds(
    db: &Db,
    project: ProjectId,
    limit: i64,
) -> Result<Vec<StoredBuild>, ForgeError> {
    const QUERY: &str = "agent_builder.builds";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT b.id, b.requested_by, b.selection, b.lock_digest, b.dry_run, \
             m.iid AS mr_iid, b.state, b.error_code, b.request_id, \
             to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
             FROM forge.agent_builds AS b LEFT JOIN forge.merge_requests AS m ON m.id = b.mr_id \
             WHERE b.project_id = $1 ORDER BY b.id DESC LIMIT $2",
        )
        .bind(project.get())
        .bind(limit.clamp(1, 50))
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}
