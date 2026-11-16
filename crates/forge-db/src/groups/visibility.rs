//! A group's visibility (plan 15 MR 3, ADR-0010 Amendment "グループの可視性"):
//! the change decided inside its transaction ([`change_visibility_decided`],
//! `group.visibility_changed`, audit-only) and the doctor's count of rows
//! stored wider than the chain above them ([`wider_than_parent`]).
//!
//! The effective visibility of a group or project is the narrowest on its
//! chain to the top level, so a narrowing never rewrites the rows below
//! (they become private through the chain) and a widening above a private
//! group is refused (`FG-REPO-0033`): it would have no effect.

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::{AuditDetails, Visibility};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{not_found, GroupAccess, GroupUpdateFacts, RESOURCE};
use crate::access::lock_account;
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::roles::{chain_private_locked_on, lock_roles, RoleAnchor, CAP};
use crate::{query_error, read_query_error, Db};

/// What a visibility change did (or, for a preview, would do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupVisibilityOutcome {
    /// The stored visibility before.
    pub from: Visibility,
    /// Written (one audit row). `false` for a preview and when the group
    /// already had that visibility.
    pub changed: bool,
    /// The live projects below whose effective visibility changes with it
    /// (internal → private when narrowing, private → internal when
    /// widening); 0 when nothing changes. What the screen asks the Owner to
    /// confirm before narrowing (plan 15 §3.2).
    pub affected_projects: i64,
}

/// `$1` the group's namespace, `$2` the cap: the live projects below it
/// whose every group between it and them is internal and that are
/// internal themselves — the projects whose effective visibility is the
/// group's chain's. The walk descends only through internal groups.
const AFFECTED_SQL: &str = "WITH RECURSIVE down (id, kind, owner_id, step) AS ( \
       SELECT n.id, n.kind, n.owner_id, 1 FROM forge.namespaces n WHERE n.id = $1 \
       UNION ALL \
       SELECT n.id, n.kind, n.owner_id, d.step + 1 \
       FROM forge.namespaces n JOIN down d ON n.parent_id = d.id \
       WHERE d.kind = 'group' AND d.step < $2 \
         AND (n.kind <> 'group' OR EXISTS (SELECT 1 FROM forge.groups g \
           WHERE g.id = n.owner_id AND g.visibility = 'internal'))) \
     SELECT count(*) FROM down d JOIN forge.projects p ON p.id = d.owner_id \
     WHERE d.kind = 'project' AND p.visibility = 'internal' AND p.deleted_at IS NULL";

async fn affected(conn: &mut PgConnection, namespace: NamespaceId) -> Result<i64, ForgeError> {
    sqlx::query_scalar(AFFECTED_SQL)
        .bind(namespace.get())
        .bind(CAP)
        .fetch_one(conn)
        .await
        .map_err(|e| query_error("groups.visibility.affected", e))
}

/// Set `group`'s stored visibility to `to` for `viewer`, deciding inside
/// the transaction: the group row is locked `FOR UPDATE`, then the
/// caller's roles (`FOR SHARE` on the membership rows), then the account
/// row; `decide` (forge-hosting: `Authorized::reconfirm`) sees those facts.
/// After it: `internal` below a private chain is `FG-REPO-0033
/// reason=group_above_private` (nothing written). `preview` decides and
/// counts but never writes (the narrowing confirmation of plan 15 §3.2).
/// A real change writes one `group.visibility_changed` audit row
/// (audit-only) with `from`, `to` and `affected_projects`. Errors:
/// `FG-REPO-0005 resource=group`, whatever `decide` returns, `FG-REPO-0033`.
pub async fn change_visibility_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    group: GroupId,
    viewer: UserId,
    to: Visibility,
    preview: bool,
    decide: impl FnOnce(&GroupUpdateFacts) -> Result<P, ForgeError>,
) -> Result<(GroupVisibilityOutcome, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let stored: Option<(String, i64, Option<i64>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT g.visibility, n.id, n.parent_id FROM forge.groups g \
                     JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
                     WHERE g.id = $1 FOR UPDATE OF g",
                )
                .bind(group.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("groups.visibility.lock", e))
            })
            .await?;
        let Some((raw, namespace, parent)) = stored else {
            return Err(not_found("group"));
        };
        let namespace = NamespaceId::new(namespace);
        let roles = tx
            .step(|c| lock_roles(c, RoleAnchor::Group(group), viewer))
            .await?;
        let account = tx.step(|c| lock_account(c, viewer)).await?;
        let decided = decide(&GroupUpdateFacts {
            account,
            group: GroupAccess {
                group,
                namespace,
                role: roles.native,
            },
        })?;
        let from = Visibility::parse(&raw).ok_or_else(|| {
            ForgeError::new(ErrorCode::NamespaceDataInvalid)
                .with_context("reason", "unknown_visibility")
                .with_context("group_id", group.get())
                .with_context("raw", raw.chars().take(32).collect::<String>())
        })?;
        let above_private = match parent {
            Some(parent) => {
                // The groups above locked `FOR SHARE` (review): a concurrent
                // narrowing above waits, or is seen.
                tx.step(|c| chain_private_locked_on(c, NamespaceId::new(parent)))
                    .await?
            }
            None => false,
        };
        if to == Visibility::Internal && above_private {
            return Err(ForgeError::new(ErrorCode::VisibilityAboveParent)
                .with_context("reason", "group_above_private")
                .with_context("group_id", group.get()));
        }
        // Below a private chain nothing changes effectively (the chain
        // stays private); otherwise the projects that follow this group.
        let affected_projects = if from == to || above_private {
            0
        } else {
            tx.step(|c| affected(c, namespace)).await?
        };
        let result = |changed| GroupVisibilityOutcome {
            from,
            changed,
            affected_projects,
        };
        if preview || from == to {
            return Ok(Settle::Discard((result(false), decided)));
        }
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.groups SET visibility = $2, updated_at = now() WHERE id = $1")
                .bind(group.get())
                .bind(to.as_str())
                .execute(c)
                .await
                .map_err(|e| query_error("groups.visibility.update", e))
        })
        .await?;
        let details = AuditDetails::GroupVisibilityChanged {
            group_id: group,
            from,
            to,
            affected_projects,
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, group.get()),
            (result(true), decided),
        ))
    }
    .await;
    tx.settle("group.visibility_changed", outcome).await
}

/// What `forge doctor` `groups.visibility` reports (plan 15 §4.1, info):
/// rows stored `internal` below a private group — legal (a narrowing never
/// rewrites the rows below) and private in effect, but a later widening of
/// the group above would expose them all at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WiderThanParent {
    pub groups: i64,
    /// Live projects only.
    pub projects: i64,
}

/// `$1` the cap: every namespace strictly below a group whose stored
/// visibility is not `internal`, then the internal groups and live
/// internal projects among them.
const WIDER_SQL: &str = "WITH RECURSIVE below (id, step) AS ( \
       SELECT c.id, 1 FROM forge.groups g \
       JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
       JOIN forge.namespaces c ON c.parent_id = n.id \
       WHERE g.visibility <> 'internal' \
       UNION \
       SELECT n.id, b.step + 1 FROM forge.namespaces n JOIN below b ON n.parent_id = b.id \
       WHERE b.step < $1) \
     SELECT \
       (SELECT count(DISTINCT n.id) FROM forge.namespaces n \
          JOIN forge.groups g ON n.kind = 'group' AND g.id = n.owner_id \
          WHERE n.id IN (SELECT id FROM below) AND g.visibility = 'internal'), \
       (SELECT count(DISTINCT n.id) FROM forge.namespaces n \
          JOIN forge.projects p ON n.kind = 'project' AND p.id = n.owner_id \
          WHERE n.id IN (SELECT id FROM below) AND p.visibility = 'internal' \
            AND p.deleted_at IS NULL)";

/// Count the rows stored wider than their chain on the read pool, in one
/// statement; `None` before `groups.visibility` exists (the doctor skips).
pub async fn wider_than_parent(db: &Db) -> Result<Option<WiderThanParent>, ForgeError> {
    const QUERY: &str = "groups.wider_than_parent";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let migrated: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute \
             WHERE attrelid = pg_catalog.to_regclass('forge.groups') \
               AND attname = 'visibility' AND NOT attisdropped)",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        if !migrated {
            return Ok(None);
        }
        let (groups, projects): (i64, i64) = sqlx::query_as(WIDER_SQL)
            .bind(CAP)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(WiderThanParent { groups, projects }))
    })
    .await
}
