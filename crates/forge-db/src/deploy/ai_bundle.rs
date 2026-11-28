//! `pfs-ai-bundle` releases (plan 17 P17-7, ADR-0031 Amendment P17-7):
//! the signed release rows, their creation (audited, decided again on the
//! locked project), and the facts the release deployments need — the
//! stage before, the rollback target.

use forge_core::deploy::{DeployChange, DeployTier};
use forge_core::{ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::{caller_of, record, TS};
use crate::change::{ChangeContext, ChangeTx, Settle};
use crate::pipelines::decided_facts;
use crate::projects::ProjectUpdateFacts;
use crate::{query_error, read_query_error, Db};

/// The most releases a list answers.
pub const MAX_RELEASES: i64 = 100;

/// A release as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRelease {
    pub id: i64,
    pub project_id: i64,
    pub kind: String,
    pub version: String,
    pub tag: String,
    pub tag_object: String,
    pub commit_sha: String,
    pub manifest: serde_json::Value,
    pub digest: String,
    pub key_id: String,
    pub signature: String,
    pub bench_check_run_id: i64,
    pub files: i32,
    pub created_by: i64,
    pub created_at: String,
}

/// What a release inserts (the caller built, signed and verified it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRelease {
    pub version: String,
    pub tag_object: String,
    pub commit_sha: String,
    pub manifest: serde_json::Value,
    pub digest: String,
    pub key_id: String,
    pub signature: String,
    pub bench_check_run_id: i64,
    pub files: i32,
}

type ReleaseRow = (
    i64,
    i64,
    String,
    String,
    String,
    String,
    String,
    serde_json::Value,
    String,
    String,
    String,
    i64,
    i32,
    i64,
    String,
);

fn columns(a: &str) -> String {
    format!(
        "{a}.id, {a}.project_id, {a}.kind, {a}.version, {a}.tag, {a}.tag_object, \
         {a}.commit_sha, {a}.manifest, {a}.digest, {a}.key_id, {a}.signature, \
         {a}.bench_check_run_id, {a}.files, {a}.created_by, \
         to_char({a}.created_at AT TIME ZONE 'UTC', '{TS}')"
    )
}

fn release_of(r: ReleaseRow) -> StoredRelease {
    StoredRelease {
        id: r.0,
        project_id: r.1,
        kind: r.2,
        version: r.3,
        tag: r.4,
        tag_object: r.5,
        commit_sha: r.6,
        manifest: r.7,
        digest: r.8,
        key_id: r.9,
        signature: r.10,
        bench_check_run_id: r.11,
        files: r.12,
        created_by: r.13,
        created_at: r.14,
    }
}

/// The SQL condition "release `r` was not signed by a revoked key" (a
/// compromised key's releases are never deployed or gone back to).
pub(crate) fn key_not_revoked_sql(r: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM forge.agent_signing_keys k \
           WHERE k.kid = {r}.key_id AND k.revoked_at IS NOT NULL)"
    )
}

/// The SQL condition "deployment `d` (the deployments alias) succeeded
/// with a person's approval and is of a release of its project **older**
/// than the release at `sha` (an SQL expression), signed by a key that is
/// not revoked" — the only rollback targets of a release (review P17-7:
/// never forward to the release just left, never an unapproved SHA).
pub(crate) fn approved_older_release_sql(d: &str, sha: &str) -> String {
    format!(
        "{d}.approved_by IS NOT NULL AND EXISTS (SELECT 1 FROM forge.ai_bundle_releases ar \
           WHERE ar.project_id = {d}.project_id AND ar.commit_sha = {d}.sha \
             AND ar.id < (SELECT cr.id FROM forge.ai_bundle_releases cr \
               WHERE cr.project_id = {d}.project_id AND cr.commit_sha = {sha}) \
             AND {})",
        key_not_revoked_sql("ar")
    )
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleReleaseRefused).with_context("reason", reason)
}

pub(crate) fn deploy_refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::BundleDeployRefused).with_context("reason", reason)
}

/// The releases of `project`, newest first (at most [`MAX_RELEASES`]).
pub async fn list_releases(db: &Db, project: ProjectId) -> Result<Vec<StoredRelease>, ForgeError> {
    const QUERY: &str = "ai_bundle.list";
    let rows: Vec<ReleaseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_releases r WHERE r.project_id = $1 \
                 ORDER BY r.id DESC LIMIT $2",
                columns("r")
            ))
            .bind(project.get())
            .bind(MAX_RELEASES)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(release_of).collect())
}

/// The release `version` of `project` (`None`: none).
pub async fn release(
    db: &Db,
    project: ProjectId,
    version: &str,
) -> Result<Option<StoredRelease>, ForgeError> {
    const QUERY: &str = "ai_bundle.get";
    let row: Option<ReleaseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_releases r \
                 WHERE r.project_id = $1 AND r.version = $2",
                columns("r")
            ))
            .bind(project.get())
            .bind(version)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(release_of))
}

/// The release of `project` at commit `sha` (`None`: none; the PFS
/// loader's lookup by the deployed SHA).
pub async fn release_at(
    db: &Db,
    project: ProjectId,
    sha: &str,
) -> Result<Option<StoredRelease>, ForgeError> {
    const QUERY: &str = "ai_bundle.at";
    let row: Option<ReleaseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_releases r \
                 WHERE r.project_id = $1 AND r.commit_sha = $2",
                columns("r")
            ))
            .bind(project.get())
            .bind(sha)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(release_of))
}

/// What the rollback of an environment resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackTarget {
    /// Nothing succeeded on the environment yet, or what runs is no release.
    NothingRunning,
    /// What runs is the oldest approved release there: nothing to go back to.
    NoEarlier { current: Box<StoredRelease> },
    /// Go back from `current` to `target`.
    To {
        current: Box<StoredRelease>,
        target: Box<StoredRelease>,
    },
}

/// The rollback of `tier` of `project` (plan 17 §1.11: "the release last
/// approved before"): `current` is the release of the environment's last
/// succeeded deployment; the target is the newest release **older than
/// it** (by release order) that succeeded on this environment with a
/// person's approval and whose signing key is not revoked. Going back
/// twice goes further back, never forward to the release just left.
pub async fn rollback_target(
    db: &Db,
    project: ProjectId,
    tier: DeployTier,
) -> Result<RollbackTarget, ForgeError> {
    const QUERY: &str = "ai_bundle.rollback_target";
    let current: Option<ReleaseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.deployments d JOIN forge.ai_bundle_releases r \
                   ON r.project_id = d.project_id AND r.commit_sha = d.sha \
                 WHERE d.id = (SELECT d2.id FROM forge.deployments d2 \
                   WHERE d2.project_id = $1 AND d2.tier = $2 AND d2.status = 'succeeded' \
                   ORDER BY d2.finished_at DESC, d2.id DESC LIMIT 1)",
                columns("r")
            ))
            .bind(project.get())
            .bind(tier.as_str())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let Some(current) = current.map(release_of) else {
        return Ok(RollbackTarget::NothingRunning);
    };
    let target: Option<ReleaseRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT {} FROM forge.ai_bundle_releases r \
                 WHERE r.project_id = $1 AND r.id < $3 AND {} AND EXISTS ( \
                   SELECT 1 FROM forge.deployments d WHERE d.project_id = r.project_id \
                     AND d.tier = $2 AND d.sha = r.commit_sha AND d.status = 'succeeded' \
                     AND d.approved_by IS NOT NULL) \
                 ORDER BY r.id DESC LIMIT 1",
                columns("r"),
                key_not_revoked_sql("r")
            ))
            .bind(project.get())
            .bind(tier.as_str())
            .bind(current.id)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(match target.map(release_of) {
        Some(target) => RollbackTarget::To {
            current: Box::new(current),
            target: Box::new(target),
        },
        None => RollbackTarget::NoEarlier {
            current: Box::new(current),
        },
    })
}

/// Make a release of `project` for the caller, decided again on the
/// locked project and caller (`decide`: forge-deploy reconfirms
/// `authorize()`), with `ai_assets.enabled` read under the lock: one
/// release row and one `ai_asset_release.created` audit row. `verify`
/// sees the row as inserted (the manifest as stored) before the commit.
/// Errors (nothing written): `decide`'s, `verify`'s, `FG-AIASSET-0008
/// reason=ai_assets_disabled` / `already_released` (the version or the
/// commit), the database's.
pub async fn create_bundle_decided(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    new: &NewRelease,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<(), ForgeError>,
    verify: impl FnOnce(&StoredRelease) -> Result<(), ForgeError>,
) -> Result<StoredRelease, ForgeError> {
    const QUERY: &str = "ai_bundle.create";
    let user = caller_of(ctx)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        decide(&decided_facts(&mut tx, project, UserId::new(user)).await?)?;
        let enabled: bool = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT coalesce((SELECT enabled FROM forge.project_ai_assets \
                       WHERE project_id = $1 FOR SHARE), false)",
                )
                .bind(project.get())
                .fetch_one(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if !enabled {
            return Err(refused("ai_assets_disabled").with_context("project_id", project.get()));
        }
        let version = new.version.clone(); // clone: the step owns its bind
        let sha = new.commit_sha.clone(); // clone: as version
        let already: Option<String> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT version FROM forge.ai_bundle_releases \
                     WHERE project_id = $1 AND (version = $2 OR commit_sha = $3) LIMIT 1",
                )
                .bind(project.get())
                .bind(version)
                .bind(sha)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        if let Some(released) = already {
            return Err(refused("already_released").with_context("released_version", released));
        }
        let sql = format!(
            "WITH ins AS (INSERT INTO forge.ai_bundle_releases (project_id, kind, version, tag, \
               tag_object, commit_sha, manifest, digest, key_id, signature, bench_check_run_id, \
               files, created_by) \
             VALUES ($1, 'pfs-ai-bundle', $2, 'assets/' || $2, $3, $4, $5, $6, $7, $8, $9, $10, \
               $11) RETURNING *) SELECT {} FROM ins r",
            columns("r")
        );
        let row: ReleaseRow = tx
            .step(|c| async move {
                sqlx::query_as(&sql)
                    .bind(project.get())
                    .bind(&new.version)
                    .bind(&new.tag_object)
                    .bind(&new.commit_sha)
                    .bind(&new.manifest)
                    .bind(&new.digest)
                    .bind(&new.key_id)
                    .bind(&new.signature)
                    .bind(new.bench_check_run_id)
                    .bind(new.files)
                    .bind(user)
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error(QUERY, e))
            })
            .await?;
        let stored = release_of(row);
        verify(&stored)?;
        let change = DeployChange::BundleReleased {
            project,
            release: stored.id,
            version: stored.version.clone(), // clone: the audit row owns it
            sha: stored.commit_sha.clone(),  // clone: as version
            digest: stored.digest.clone(),   // clone: as version
            key_id: stored.key_id.clone(),   // clone: as version
            files: stored.files,
        };
        Ok(Settle::Commit(record(change), stored))
    }
    .await;
    tx.settle("ai_asset_release.created", outcome).await
}

/// Whether `project`'s deployments are releases only (its
/// `ai_assets.enabled`), read in the request's transaction.
pub(crate) async fn releases_only(
    tx: &mut ChangeTx,
    project: ProjectId,
    query: &'static str,
) -> Result<bool, ForgeError> {
    tx.step(|c| async move {
        sqlx::query_scalar(
            "SELECT coalesce((SELECT enabled FROM forge.project_ai_assets \
               WHERE project_id = $1 FOR SHARE), false)",
        )
        .bind(project.get())
        .fetch_one(c)
        .await
        .map_err(|e| query_error(query, e))
    })
    .await
}

/// The checks of a release deployment in the request's transaction: the
/// release exists at `sha`, the environment requires an approval, and the
/// stage before succeeded with this release (`FG-AIASSET-0009`).
pub(crate) async fn check_release_request(
    tx: &mut ChangeTx,
    project: ProjectId,
    tier: DeployTier,
    sha: &str,
    requires_approval: bool,
    query: &'static str,
) -> Result<(), ForgeError> {
    let owned = sha.to_owned();
    let released: bool = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM forge.ai_bundle_releases \
                 WHERE project_id = $1 AND commit_sha = $2)",
            )
            .bind(project.get())
            .bind(owned)
            .fetch_one(c)
            .await
            .map_err(|e| query_error(query, e))
        })
        .await?;
    if !released {
        return Err(deploy_refused("release_not_found"));
    }
    let owned = sha.to_owned();
    let revoked: bool = tx
        .step(|c| async move {
            sqlx::query_scalar(&format!(
                "SELECT NOT {} FROM forge.ai_bundle_releases r \
                 WHERE r.project_id = $1 AND r.commit_sha = $2",
                key_not_revoked_sql("r")
            ))
            .bind(project.get())
            .bind(owned)
            .fetch_one(c)
            .await
            .map_err(|e| query_error(query, e))
        })
        .await?;
    if revoked {
        return Err(deploy_refused("signing_key_revoked"));
    }
    if !requires_approval {
        return Err(deploy_refused("approval_required").with_context("environment", tier.as_str()));
    }
    let Some(before) = tier.bundle_stage_before() else {
        return Ok(());
    };
    let owned = sha.to_owned();
    let passed: bool = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM forge.deployments WHERE project_id = $1 \
                   AND tier = $2 AND sha = $3 AND status = 'succeeded' \
                   AND approved_by IS NOT NULL)",
            )
            .bind(project.get())
            .bind(before.as_str())
            .bind(owned)
            .fetch_one(c)
            .await
            .map_err(|e| query_error(query, e))
        })
        .await?;
    if !passed {
        return Err(deploy_refused("stage_skipped")
            .with_context("environment", tier.bundle_stage())
            .with_context("needs", before.bundle_stage()));
    }
    Ok(())
}

#[cfg(test)]
#[path = "ai_bundle_tests.rs"]
mod tests;
