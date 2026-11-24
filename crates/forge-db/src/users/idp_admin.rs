//! The administrator flag from the identity provider's client role
//! (ADR-0021 amendment 2026-11-24): `auth.oidc.admin_role`, read at each
//! sign-in and refresh check, grants and revokes `users.is_admin` of a
//! person, and `users.admin_source` says who manages the flag.
//!
//! The rule ([`idp_admin_change`]), for an active human only:
//! - the role is present and the person is not an administrator → granted
//!   (`is_admin = true`, `admin_source = 'oidc'`);
//! - the role is absent and the flag was granted by the role
//!   (`admin_source = 'oidc'`) → revoked (`is_admin = false`,
//!   `admin_source = 'cli'`);
//! - anything else is left alone: an administrator made by the CLI
//!   (`admin_source = 'cli'`, the break-glass path) keeps the flag without
//!   the role, a bot or a service account never gains it, a disabled user
//!   is not touched.
//!
//! A change is one `ChangeTx` under `users.admin_set` (the lock order of
//! `admin.rs`) with one `user.admin_changed` audit row; no change writes
//! nothing. Revoking the last administrator is not refused: the identity
//! provider decides, `forge doctor` reports `FG-AUTH-0010 reason=no_admin`
//! and `forge admin bootstrap-admin` recovers.

use forge_core::ids::UserId;
use forge_core::{AdminSource, AuditDetails, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{not_found, RESOURCE};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::users::admin::lock_admin_set;
use crate::{query_error, read_query_error, Db};

/// What [`sync_idp_admin`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdpAdmin {
    /// Nothing to change (or not a person this rule manages).
    Unchanged,
    /// The role made the person an administrator.
    Granted,
    /// The role was gone and the flag it had granted was cleared.
    Revoked,
}

impl IdpAdmin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Granted => "granted",
            Self::Revoked => "revoked",
        }
    }
}

/// The account facts the rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminFacts {
    pub kind: UserKind,
    pub active: bool,
    pub is_admin: bool,
    pub source: AdminSource,
}

/// The new `is_admin` the role asks for, `None` when nothing changes (the
/// rule of the module documentation).
pub fn idp_admin_change(facts: AdminFacts, has_role: bool) -> Option<bool> {
    if facts.kind != UserKind::Human || !facts.active {
        return None;
    }
    match (has_role, facts.is_admin, facts.source) {
        (true, false, _) => Some(true),
        (false, true, AdminSource::Oidc) => Some(false),
        _ => None,
    }
}

type FactsRow = (String, bool, bool, String);

fn facts_of(user: UserId, row: FactsRow) -> Result<AdminFacts, ForgeError> {
    let (kind, active, is_admin, source) = row;
    let kind = super::parse_kind(user.get(), &kind)?;
    let source = AdminSource::parse(&source).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_admin_source")
            .with_context("user_id", user.get())
    })?;
    Ok(AdminFacts {
        kind,
        active,
        is_admin,
        source,
    })
}

const FACTS: &str = "SELECT kind, state = 'active', is_admin, admin_source FROM forge.users \
                     WHERE id = $1";
const FACTS_LOCKED: &str = "SELECT kind, state = 'active', is_admin, admin_source \
                            FROM forge.users WHERE id = $1 FOR UPDATE";

/// Apply the identity provider's role to `user` (a sign-in or a refresh
/// check that read a verified token). A first read without locks skips
/// the common case (nothing to change) without a transaction; a change is
/// decided again under `users.admin_set` and the user row `FOR UPDATE`.
///
/// Errors: `FG-REPO-0005` for an unknown user; the database's
/// (`FG-DB-0001`, …). Nothing is written on an error.
pub async fn sync_idp_admin(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    has_role: bool,
) -> Result<IdpAdmin, ForgeError> {
    let seen: Option<FactsRow> = db
        .within("users.idp_admin.read", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("users.idp_admin.acquire", e))?;
            sqlx::query_as(FACTS)
                .bind(user.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| query_error("users.idp_admin.read", e))
        })
        .await?;
    let Some(seen) = seen else {
        return Err(not_found());
    };
    if idp_admin_change(facts_of(user, seen)?, has_role).is_none() {
        return Ok(IdpAdmin::Unchanged);
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = locked_change(&mut tx, user, has_role).await;
    tx.settle("user.admin_changed", outcome).await
}

async fn locked_change(
    tx: &mut ChangeTx,
    user: UserId,
    has_role: bool,
) -> Result<Settle<IdpAdmin>, ForgeError> {
    tx.step(lock_admin_set).await?;
    let row: Option<FactsRow> = tx
        .step(|c| async move {
            sqlx::query_as(FACTS_LOCKED)
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("users.idp_admin.lock", e))
        })
        .await?;
    let Some(row) = row else {
        return Err(not_found());
    };
    let Some(to) = idp_admin_change(facts_of(user, row)?, has_role) else {
        // Another sign-in of the same person changed it meanwhile.
        return Ok(Settle::Discard(IdpAdmin::Unchanged));
    };
    tx.step(|c| write_flag(c, user, to)).await?;
    let details = AuditDetails::UserAdminChanged { to };
    let done = if to {
        IdpAdmin::Granted
    } else {
        IdpAdmin::Revoked
    };
    Ok(Settle::Commit(
        ChangeRecord::audit_only(&details, RESOURCE, user.get()),
        done,
    ))
}

async fn write_flag(c: &mut PgConnection, user: UserId, to: bool) -> Result<(), ForgeError> {
    let source = if to {
        AdminSource::Oidc
    } else {
        AdminSource::Cli
    };
    sqlx::query(
        "UPDATE forge.users SET is_admin = $2, admin_source = $3, updated_at = now() \
         WHERE id = $1",
    )
    .bind(user.get())
    .bind(to)
    .bind(source.as_str())
    .execute(c)
    .await
    .map(|_| ())
    .map_err(|e| query_error("users.idp_admin.write", e))
}

/// What [`set_admin_source`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminSourceSet {
    Changed {
        from: AdminSource,
    },
    /// Already managed by `to`: nothing written.
    Unchanged,
}

/// `forge admin user admin-source <user> --source cli|oidc`: hand an
/// administrator's flag to the identity provider's role (`oidc`: the next
/// sign-in or refresh check without the role clears it) or keep it with
/// the CLI (`cli`: break-glass, never cleared by the role). One
/// `user.admin_source_changed` audit row.
///
/// Errors (nothing is written): `FG-AUTH-0009` `not_admin` for a user who
/// is not an administrator, `not_human`, `user_disabled`; `FG-REPO-0005`
/// for an unknown user.
pub async fn set_admin_source(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    to: AdminSource,
) -> Result<AdminSourceSet, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        tx.step(lock_admin_set).await?;
        let row: Option<FactsRow> = tx
            .step(|c| async move {
                sqlx::query_as(FACTS_LOCKED)
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("users.admin_source.lock", e))
            })
            .await?;
        let Some(row) = row else {
            return Err(not_found());
        };
        let facts = facts_of(user, row)?;
        let refuse = |reason: &'static str| {
            ForgeError::new(ErrorCode::AdministrationConflict)
                .with_context("reason", reason)
                .with_context("user", user.get())
        };
        if facts.kind != UserKind::Human {
            return Err(refuse("not_human"));
        }
        if !facts.active {
            return Err(refuse("user_disabled"));
        }
        if !facts.is_admin {
            return Err(refuse("not_admin"));
        }
        if facts.source == to {
            return Ok(Settle::Discard(AdminSourceSet::Unchanged));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.users SET admin_source = $2, updated_at = now() WHERE id = $1",
            )
            .bind(user.get())
            .bind(to.as_str())
            .execute(c)
            .await
            .map(|_| ())
            .map_err(|e| query_error("users.admin_source.write", e))
        })
        .await?;
        let details = AuditDetails::UserAdminSourceChanged {
            from: facts.source,
            to,
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, user.get()),
            AdminSourceSet::Changed { from: facts.source },
        ))
    }
    .await;
    tx.settle("user.admin_source_changed", outcome).await
}

/// The active human administrators by who manages their flag (`forge
/// doctor` `auth.oidc.admin_role`; read pool, no locks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AdminSources {
    pub cli: i64,
    pub oidc: i64,
}

pub async fn admin_sources(db: &Db) -> Result<AdminSources, ForgeError> {
    const QUERY: &str = "users.admin_sources";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let (cli, oidc): (i64, i64) = sqlx::query_as(
            "SELECT pg_catalog.count(*) FILTER (WHERE admin_source = 'cli'), \
                    pg_catalog.count(*) FILTER (WHERE admin_source = 'oidc') \
             FROM forge.users WHERE kind = 'human' AND state = 'active' AND is_admin",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(AdminSources { cli, oidc })
    })
    .await
}
