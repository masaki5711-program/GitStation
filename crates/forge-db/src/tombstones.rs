//! Releasing a namespace tombstone, decided inside its transaction (D5
//! MR2, ADR-0015 `NamespaceTombstoneRelease`): a released path can be used
//! again. One `namespace_tombstone.released` audit row on the tombstone's
//! cause (`cause_type` / `cause_id`, the resource whose move or deletion
//! wrote it — the audit trigger of forge.namespace_tombstones requires it).
//!
//! Lock order: the path advisory lock of the tombstone's path first (as a
//! move or create of that path takes it before it reads tombstones; it is
//! also what serializes two deleters of the row), then the tombstone row,
//! then the namespace it names or its parent with the caller's roles there,
//! then the caller's account.

use forge_core::ids::{NamespaceId, TombstoneId, UserId};
use forge_core::{AuditDetails, Role, TombstoneKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::access::{lock_account, Account};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::namespaces::lock_path;
use crate::roles::{lock_roles, RoleAnchor};
use crate::{query_error, Db};

/// What `decide` needs to build forge-auth's `TombstoneRelease`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneAccess {
    /// A `moved` tombstone whose target exists: the caller's current role
    /// on it (Owner for the owner of a personal namespace).
    Moved {
        target: NamespaceId,
        target_role: Option<Role>,
    },
    /// A `deleted` tombstone, or a `moved` one whose target is gone.
    Deleted {
        recorded_owner: Option<UserId>,
        /// The caller's current role on the tombstone's parent namespace.
        caller_parent_role: Option<Role>,
        has_parent: bool,
    },
}

/// What [`release_decided`]'s `decide` sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TombstoneReleaseFacts {
    pub account: Account,
    pub tombstone: TombstoneId,
    pub access: TombstoneAccess,
}

/// What a release did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    /// The path that can be used again.
    pub path: String,
    pub kind: TombstoneKind,
}

fn not_found() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", "tombstone")
        .with_context("reason", "absent")
}

/// `viewer`'s role on `namespace` for a release decision: Owner on their
/// own personal namespace, else the native role (`roles::lock_roles`). The
/// namespace row is read `FOR SHARE`; `None` when it is gone.
async fn role_on(
    tx: &mut ChangeTx,
    namespace: i64,
    viewer: UserId,
) -> Result<Option<Option<Role>>, ForgeError> {
    let row: Option<(String, i64)> = tx
        .step(|c| async move {
            sqlx::query_as("SELECT kind, owner_id FROM forge.namespaces WHERE id = $1 FOR SHARE")
                .bind(namespace)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("tombstones.release.namespace", e))
        })
        .await?;
    let Some((kind, owner)) = row else {
        return Ok(None);
    };
    if kind == "user" {
        return Ok(Some((owner == viewer.get()).then_some(Role::Owner)));
    }
    let roles = tx
        .step(|c| {
            lock_roles(
                c,
                RoleAnchor::Namespace(NamespaceId::new(namespace)),
                viewer,
            )
        })
        .await?;
    Ok(Some(roles.native))
}

async fn path_of(conn: &mut PgConnection, id: TombstoneId) -> Result<Option<String>, ForgeError> {
    sqlx::query_scalar("SELECT old_path FROM forge.namespace_tombstones WHERE id = $1")
        .bind(id.get())
        .fetch_optional(conn)
        .await
        .map_err(|e| query_error("tombstones.release.path", e))
}

/// Release `tombstone` for `viewer`, deciding inside the transaction
/// (module docs; `decide` — forge-hosting: `Resource::Tombstone` — sees the
/// kind and the caller's roles before anything is written). Deletes the
/// row and writes one `namespace_tombstone.released` audit row on its
/// cause. Every exit that does not commit rolls back explicitly. Errors:
/// `FG-REPO-0005 resource=tombstone` and whatever `decide` returns.
pub async fn release_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    tombstone: TombstoneId,
    viewer: UserId,
    decide: impl FnOnce(&TombstoneReleaseFacts) -> Result<P, ForgeError>,
) -> Result<(Released, P), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let Some(path) = tx.step(|c| path_of(c, tombstone)).await? else {
            return Err(not_found());
        };
        let locked_path = path.as_str();
        tx.step(|c| lock_path(c, locked_path)).await?;
        type Row = (
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            String,
            i64,
        );
        let row: Option<Row> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT old_path, kind, namespace_id, parent_namespace_id, owner_at_write, \
                     cause_type, cause_id FROM forge.namespace_tombstones WHERE id = $1",
                )
                .bind(tombstone.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("tombstones.release.lock", e))
            })
            .await?;
        // No row lock (forge_app has no UPDATE on the table, which a
        // locking read needs): every deleter of a tombstone — a release, a
        // reclaiming move — takes its path lock first, so the path lock held
        // here serializes them. old_path is never updated.
        let Some((old_path, kind, namespace, parent, recorded, cause_type, cause_id)) = row else {
            return Err(not_found());
        };
        if old_path != path {
            return Err(not_found());
        }
        let mut access = None;
        let mut target_id = None;
        if kind == "moved" {
            if let Some(ns) = namespace {
                if let Some(role) = role_on(&mut tx, ns, viewer).await? {
                    target_id = Some(NamespaceId::new(ns));
                    access = Some(TombstoneAccess::Moved {
                        target: NamespaceId::new(ns),
                        target_role: role,
                    });
                }
            }
        }
        let kind_out = if access.is_some() {
            TombstoneKind::Moved
        } else {
            TombstoneKind::Deleted
        };
        let access = match access {
            Some(a) => a,
            None => {
                let caller_parent_role = match parent {
                    Some(p) => role_on(&mut tx, p, viewer).await?.flatten(),
                    None => None,
                };
                TombstoneAccess::Deleted {
                    recorded_owner: recorded.map(UserId::new),
                    caller_parent_role,
                    has_parent: parent.is_some(),
                }
            }
        };
        let account = tx.step(|c| lock_account(c, viewer)).await?;
        let decided = decide(&TombstoneReleaseFacts {
            account,
            tombstone,
            access,
        })?;
        let resource: &'static str = match cause_type.as_str() {
            "user" => "user",
            "group" => "group",
            "project" => "project",
            _ => {
                return Err(ForgeError::new(ErrorCode::NamespaceDataInvalid)
                    .with_context("reason", "unknown_tombstone_cause")
                    .with_context("tombstone_id", tombstone.get()))
            }
        };
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.namespace_tombstones WHERE id = $1")
                .bind(tombstone.get())
                .execute(c)
                .await
                .map_err(|e| query_error("tombstones.release.delete", e))
        })
        .await?;
        let details = AuditDetails::TombstoneReleased {
            tombstone_id: tombstone,
            kind: kind_out,
            namespace_id: target_id.or(namespace.map(NamespaceId::new)),
            reclaim: false,
        };
        let mut record = ChangeRecord::audit_only(&details, resource, cause_id);
        if resource == "project" {
            record.audit.project_id = Some(forge_core::ProjectId::new(cause_id));
        }
        Ok(Settle::Commit(
            record,
            (
                Released {
                    path: old_path,
                    kind: kind_out,
                },
                decided,
            ),
        ))
    }
    .await;
    tx.settle("namespace_tombstone.released", outcome).await
}
