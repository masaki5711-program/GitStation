//! A project's `ai_assets.enabled` (plan 17 §0.4, P17-3, ADR-0035;
//! migration 20261222100000): `forge.project_ai_assets`, no row = off.
//!
//! - **A person's command** (audited, `ChangeTx`): [`set_enabled_decided`]
//!   (`ai_assets.enabled` / `ai_assets.disabled` on the project).
//! - **Reads**: [`setting`] (the route, the `forge/risk` worker).
//!
//! Every decision is the caller's (`AiAssetsSettingManage`, decided again
//! on the locked project by `decide`).
//!
//! A change asks for `forge/risk` again (review MEDIUM 3): in the same
//! transaction, one `check.risk` job for the head of each open merge
//! request of the project (at most [`MAX_REASSESSED`], newest first), keyed
//! with the setting's `version` — a head assessed with the setting off is
//! assessed again with the AI asset rules on, and the other way round.

use forge_core::ai_assets::AiAssetsChange;
use forge_core::ids::{ProjectId, UserId};
use forge_core::AuditDetails;
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::projects::{lock_shared, locked_access_facts, ProjectUpdateFacts};
use crate::review::auto_merge::ChangeOutcome;
use crate::{query_error, read_query_error, Db};

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// The most open merge requests whose head a change asks to assess again.
pub const MAX_REASSESSED: i64 = 500;

/// A project's `ai_assets.enabled` as stored (or the default, off).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiAssetsSetting {
    pub project: ProjectId,
    pub enabled: bool,
    /// Who changed it last (`None`: never changed).
    pub changed_by: Option<UserId>,
    /// When (UTC, RFC 3339).
    pub changed_at: Option<String>,
}

type Row = (bool, i64, String);

fn sql(lock: &str) -> String {
    format!(
        "SELECT enabled, changed_by, to_char(changed_at AT TIME ZONE 'UTC', {TS}) \
         FROM forge.project_ai_assets WHERE project_id = $1 {lock}"
    )
}

fn parse(project: ProjectId, row: Option<Row>) -> AiAssetsSetting {
    match row {
        Some((enabled, by, at)) => AiAssetsSetting {
            project,
            enabled,
            changed_by: Some(UserId::new(by)),
            changed_at: Some(at),
        },
        None => AiAssetsSetting {
            project,
            enabled: false,
            changed_by: None,
            changed_at: None,
        },
    }
}

async fn setting_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<AiAssetsSetting, ForgeError> {
    let row: Option<Row> = sqlx::query_as(&sql("FOR UPDATE"))
        .bind(project.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("ai_assets.setting_on", e))?;
    Ok(parse(project, row))
}

/// The setting of `project` (read pool, bounded by the pool's timeout).
pub async fn setting(db: &Db, project: ProjectId) -> Result<AiAssetsSetting, ForgeError> {
    const QUERY: &str = "ai_assets.setting";
    db.within(QUERY, async {
        let row: Option<Row> = sqlx::query_as(&sql(""))
            .bind(project.get())
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(parse(project, row))
    })
    .await
}

fn record(change: AiAssetsChange) -> ChangeRecord {
    let details = AuditDetails::AiAssets(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(change.project.get()),
            project_id: Some(change.project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// Turn `project`'s `ai_assets.enabled` on (`enabled`) or off for
/// `caller`, decided again on the locked project (`decide`: the caller's
/// reconfirmed `AiAssetsSettingManage`). One `ai_assets.enabled` /
/// `ai_assets.disabled` audit row on the project; already so
/// ([`ChangeOutcome::Unchanged`]) writes nothing. Turning it off removes
/// no other row (plan 17 §0.4: releases and history stay).
pub async fn set_enabled_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    caller: UserId,
    enabled: bool,
    decide: impl FnOnce(&ProjectUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(ChangeOutcome, AiAssetsSetting, P), ForgeError> {
    let action = if enabled {
        "ai_assets.enabled"
    } else {
        "ai_assets.disabled"
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let locked = lock_shared(&mut tx, project).await?;
        let decided = decide(&locked_access_facts(&mut tx, project, caller, &locked).await?)?;
        let before = tx
            .step(|c| async move { setting_on(c, project).await })
            .await?;
        if before.enabled == enabled {
            return Ok(Settle::Discard((ChangeOutcome::Unchanged, before, decided)));
        }
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.project_ai_assets (project_id, enabled, changed_by, changed_at) \
                 VALUES ($1, $2, $3, now()) ON CONFLICT (project_id) DO UPDATE SET \
                 enabled = EXCLUDED.enabled, changed_by = EXCLUDED.changed_by, \
                 changed_at = now(), version = forge.project_ai_assets.version + 1",
            )
            .bind(project.get())
            .bind(enabled)
            .bind(caller.get())
            .execute(c)
            .await
            .map_err(|e| query_error("ai_assets.set_enabled", e))
            .map(drop)
        })
        .await?;
        let after = tx
            .step(|c| async move { setting_on(c, project).await })
            .await?;
        let origin = ctx.request_id.clone(); // clone: each job owns its origin
        let asked = tx
            .step(|c| async move { reassess(c, project, origin).await })
            .await?;
        tracing::info!(
            target: "forge_db",
            event = "forge.ai_assets.reassess",
            project_id = project.get(),
            heads = asked,
            "open merge request heads asked to assess forge/risk again"
        );
        let change = AiAssetsChange {
            project,
            from: before.enabled,
            to: enabled,
        };
        Ok(Settle::Commit(
            record(change),
            (ChangeOutcome::Changed, after, decided),
        ))
    }
    .await;
    tx.settle(action, outcome).await
}

/// One `check.risk` job per open merge request head of `project`, keyed
/// with the setting's current version (module docs). How many were asked.
async fn reassess(
    conn: &mut PgConnection,
    project: ProjectId,
    origin: forge_core::RequestId,
) -> Result<usize, ForgeError> {
    let version: i64 =
        sqlx::query_scalar("SELECT version FROM forge.project_ai_assets WHERE project_id = $1")
            .bind(project.get())
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| query_error("ai_assets.version", e))?;
    let heads: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, head_sha FROM forge.merge_requests \
         WHERE project_id = $1 AND state = 'open' AND head_sha IS NOT NULL \
         ORDER BY id DESC LIMIT $2",
    )
    .bind(project.get())
    .bind(MAX_REASSESSED)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| query_error("ai_assets.open_heads", e))?;
    for (mr, head) in &heads {
        let head = forge_core::ObjectId::parse(head).map_err(|_| {
            ForgeError::new(forge_errors::ErrorCode::Internal)
                .with_context("reason", "invalid_head_sha")
                .with_context("merge_request_id", *mr)
        })?;
        let job = crate::checks::risk::risk_job(
            &crate::checks::risk::RiskRequest { project, head },
            &format!("mr{mr}:ai{version}"),
            origin.clone(), // clone: each job owns its origin
            None,
        )?;
        let _enqueued = crate::jobs::enqueue_on(&mut *conn, &job).await?;
    }
    Ok(heads.len())
}

#[cfg(test)]
#[path = "ai_assets_tests.rs"]
mod tests;
