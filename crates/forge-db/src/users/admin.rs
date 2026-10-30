//! Keeping forge administrable (D3 MR2, ADR-0019 §10, §11 and Amendment):
//! the disable guard's counts, `bootstrap_admin` (the first-install and
//! recovery path) and what `forge doctor` `users.administration` reads.
//!
//! Locking order, for every command that changes who administers forge
//! (`disable`, `bootstrap_admin`, later any `is_admin` change):
//! 1. the transaction-scoped advisory lock `users.admin_set`,
//! 2. the target user row `FOR UPDATE`,
//! 3. the counts below, without locks.
//!
//! Group rows are never locked here: member changes lock the group row and
//! then the other owners' user rows `FOR SHARE` (`groups::members`), so
//! locking user → group would be the reverse order and could deadlock. The
//! user row lock alone serializes a disable with a member change that relies
//! on that user as the remaining Owner.
//!
//! **Every change of `users.is_admin` or of an administrator's state**
//! (today `disable` and `bootstrap_admin`; later a "grant / revoke admin"
//! command, LDAP sync or any HTTP route) **must take [`lock_admin_set`]
//! first**, in the order above. A writer that skips it can race the
//! last-admin count of a concurrent `disable` and leave forge without an
//! administrator. (`create` with `is_admin = true` only adds an
//! administrator, so it cannot break that count and does not take it.)
//!
//! The guard and the doctor query know no group state: groups have no
//! archived / pending-deletion state yet (M1, step D plan Q11). When one is
//! added (D5), both must exclude such groups (ADR-0019 Amendment).

use forge_core::ids::{GroupId, NamespaceId, UserId};
use forge_core::{AuditDetails, PathSegment, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{insert, not_found, validate, NewUser, RESOURCE};
use crate::change::{ChangeContext, ChangeRecord, ChangeTx};
use crate::{query_error, read_query_error, Db};

/// The advisory lock key of "the set of administrators". A `namespace:`
/// path lock (`namespaces::lock_path`) hashes another prefix; a collision
/// would only serialize two unrelated writers.
const ADMIN_SET_LOCK: &str = "users.admin_set";

/// How many ownerless group ids the doctor finding carries.
const SHOWN_GROUP_IDS: i64 = 10;

/// Take the transaction-scoped advisory lock `users.admin_set` (held until
/// the transaction ends). Always the first lock of `disable` and
/// `bootstrap_admin`.
pub(crate) async fn lock_admin_set(conn: &mut PgConnection) -> Result<(), ForgeError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(ADMIN_SET_LOCK)
        .execute(conn)
        .await
        .map(|_| ())
        .map_err(|e| query_error("users.lock_admin_set", e))
}

/// `FG-AUTH-0009` with its `reason`.
fn conflict(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::AdministrationConflict).with_context("reason", reason)
}

/// What disabling a user would leave behind (read after the user row is
/// locked, under `users.admin_set`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Leaves {
    /// No other active human administrator would remain.
    pub without_admin: bool,
    /// Top-level groups whose only direct active Owner is the user.
    pub ownerless_groups: Vec<i64>,
}

impl Leaves {
    pub fn is_clean(&self) -> bool {
        !self.without_admin && self.ownerless_groups.is_empty()
    }

    pub fn ownerless_count(&self) -> i64 {
        i64::try_from(self.ownerless_groups.len()).unwrap_or(i64::MAX)
    }

    /// The guard's refusal, `None` when the disable leaves forge
    /// administrable. The last administrator is reported first.
    pub fn refusal(&self, user: UserId) -> Option<ForgeError> {
        if self.without_admin {
            return Some(conflict("last_admin").with_context("user", user.get()));
        }
        let first = self.ownerless_groups.first()?;
        Some(
            conflict("last_owner")
                .with_context("user", user.get())
                .with_context("group_id", *first)
                .with_context("ownerless_groups", self.ownerless_count()),
        )
    }
}

/// The disable guard's counts for an active user of `kind` (ADR-0019 §10):
/// only active human administrators count, and only top-level groups need
/// a direct active Owner (subgroups inherit theirs).
pub(super) async fn what_disable_leaves(
    conn: &mut PgConnection,
    user: UserId,
    kind: UserKind,
    is_admin: bool,
) -> Result<Leaves, ForgeError> {
    let without_admin = if kind == UserKind::Human && is_admin {
        let others: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM forge.users \
             WHERE kind = 'human' AND state = 'active' AND is_admin AND id <> $1",
        )
        .bind(user.get())
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| query_error("users.disable.admin_count", e))?;
        others == 0
    } else {
        false
    };
    let ownerless_groups: Vec<i64> = sqlx::query_scalar(
        "SELECT m.group_id FROM forge.group_members m \
         JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = m.group_id \
         WHERE m.user_id = $1 AND m.role = 'owner' AND n.parent_id IS NULL \
         AND NOT EXISTS (SELECT 1 FROM forge.group_members o \
             JOIN forge.users u ON u.id = o.user_id \
             WHERE o.group_id = m.group_id AND o.role = 'owner' AND o.user_id <> $1 \
             AND u.state = 'active') \
         ORDER BY m.group_id",
    )
    .bind(user.get())
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| query_error("users.disable.owner_count", e))?;
    Ok(Leaves {
        without_admin,
        ownerless_groups,
    })
}

/// What `bootstrap_admin` is asked to do.
#[derive(Debug, Clone)]
pub enum Bootstrap {
    /// Make an existing active human an administrator.
    Promote(UserId),
    /// Create a new human administrator (with their personal namespace,
    /// through the same path as `users::create`; `is_admin` is set).
    Create(NewUser),
}

/// What `bootstrap_admin` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bootstrapped {
    pub user_id: UserId,
    /// The new user's personal namespace; `None` for `Promote`.
    pub namespace_id: Option<NamespaceId>,
}

/// Refuse while an active human administrator exists (`admin_exists`).
async fn ensure_no_admin(conn: &mut PgConnection) -> Result<(), ForgeError> {
    let admins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.users WHERE kind = 'human' AND state = 'active' AND is_admin",
    )
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("users.bootstrap_admin.admin_count", e))?;
    if admins > 0 {
        return Err(conflict("admin_exists").with_context("active_human_admins", admins));
    }
    Ok(())
}

/// The first-install and recovery path (`forge admin bootstrap-admin`):
/// make an active human an administrator, or create one, when forge has no
/// active human administrator. One `ChangeTx` with one audit row
/// `user.admin_bootstrapped` (`AuditDetails::AdminBootstrapped`, ids and
/// flags only); no event (`user.*` is audit-only).
///
/// Errors (nothing is written): `FG-AUTH-0009` `admin_exists` while an
/// active human administrator exists, `not_human` for a bot or service
/// account, `user_disabled` for a disabled user; `FG-REPO-0005` for an
/// unknown user; for `Create`, the errors of `users::create`.
pub async fn bootstrap_admin(
    db: &Db,
    ctx: &ChangeContext,
    request: &Bootstrap,
) -> Result<Bootstrapped, ForgeError> {
    // A new user's request is checked before anything is locked.
    let create = match request {
        Bootstrap::Promote(_) => None,
        Bootstrap::Create(new) => {
            if new.kind != UserKind::Human {
                return Err(conflict("not_human"));
            }
            let segment = PathSegment::parse_top_level(&new.username)?;
            validate(new)?;
            let admin = NewUser {
                is_admin: true,
                ..new.clone() // clone: the caller keeps its request
            };
            Some((segment, admin))
        }
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    tx.step(lock_admin_set).await?;
    let done = match (request, create) {
        (Bootstrap::Promote(user), _) => {
            let user = *user;
            let row: Option<(String, String)> = tx
                .step(|c| async move {
                    sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR UPDATE")
                        .bind(user.get())
                        .fetch_optional(c)
                        .await
                        .map_err(|e| query_error("users.bootstrap_admin.lock", e))
                })
                .await?;
            let Some((kind, state)) = row else {
                return Err(not_found());
            };
            if kind != UserKind::Human.as_str() {
                return Err(conflict("not_human").with_context("user", user.get()));
            }
            if state != "active" {
                return Err(conflict("user_disabled").with_context("user", user.get()));
            }
            tx.step(ensure_no_admin).await?;
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.users SET is_admin = true, updated_at = now() WHERE id = $1",
                )
                .bind(user.get())
                .execute(c)
                .await
                .map_err(|e| query_error("users.bootstrap_admin.promote", e))
            })
            .await?;
            Bootstrapped {
                user_id: user,
                namespace_id: None,
            }
        }
        (Bootstrap::Create(_), Some((segment, admin))) => {
            tx.step(ensure_no_admin).await?;
            let created = insert(&mut tx, &admin, segment.as_str()).await?;
            Bootstrapped {
                user_id: created.user_id,
                namespace_id: Some(created.namespace_id),
            }
        }
        (Bootstrap::Create(_), None) => {
            // `create` is Some for every Create above; a broken invariant.
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("operation", "users.bootstrap_admin.dispatch"));
        }
    };
    let details = AuditDetails::AdminBootstrapped {
        created: done.namespace_id.is_some(),
        namespace_id: done.namespace_id,
    };
    tx.commit(ChangeRecord::audit_only(
        &details,
        RESOURCE,
        done.user_id.get(),
    ))
    .await?
    .require_recorded(details.action())?;
    Ok(done)
}

/// Who administers forge, as `forge doctor` `users.administration` sees it
/// (read pool, no locks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Administration {
    /// Every user row (any kind or state).
    pub users: i64,
    pub active_human_admins: i64,
    /// Top-level groups without a direct active Owner.
    pub ownerless_groups: i64,
    /// The first of them (at most 10), by id.
    pub ownerless_group_ids: Vec<GroupId>,
}

impl Administration {
    /// No users at all: a new install, before `forge admin bootstrap-admin`.
    /// Not orphaned (nothing was lost), but not administrable yet either.
    pub fn is_fresh_install(&self) -> bool {
        self.users == 0
    }

    /// `FG-AUTH-0010` (warn) when there is no active human administrator
    /// (`reason=no_admin`) or a top-level group has no direct active Owner
    /// (`reason=ownerless_group`); counts and group ids, never names.
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.active_human_admins > 0 && self.ownerless_groups == 0 {
            return Ok(());
        }
        let reason = if self.active_human_admins == 0 {
            "no_admin"
        } else {
            "ownerless_group"
        };
        let mut e = ForgeError::new(ErrorCode::AdministrationOrphaned)
            .with_context("reason", reason)
            .with_context("active_human_admins", self.active_human_admins)
            .with_context("ownerless_groups", self.ownerless_groups);
        if !self.ownerless_group_ids.is_empty() {
            let ids: Vec<String> = self
                .ownerless_group_ids
                .iter()
                .map(|g| g.get().to_string())
                .collect();
            e = e.with_context("group_ids", ids.join(","));
        }
        Err(e)
    }
}

/// Read who administers forge on the read pool; `None` before the schema
/// migration (`forge doctor` skips the check then).
pub async fn administration(db: &Db) -> Result<Option<Administration>, ForgeError> {
    const QUERY: &str = "users.administration";
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let migrated: bool =
            sqlx::query_scalar("SELECT pg_catalog.to_regclass('forge.group_members') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        if !migrated {
            return Ok(None);
        }
        // One statement, so the user counts and the ownerless groups come
        // from one snapshot (a separate REPEATABLE READ transaction would
        // need `SET TRANSACTION`, which the db-write-boundary gate keeps
        // inside ChangeTx). Ownerless = top-level groups (their namespace
        // has no parent) without a direct active Owner.
        let (users, active_human_admins, ownerless_groups, shown): (i64, i64, i64, Vec<i64>) =
            sqlx::query_as(
                "WITH counts AS ( \
                     SELECT pg_catalog.count(*) AS users, pg_catalog.count(*) FILTER \
                         (WHERE kind = 'human' AND state = 'active' AND is_admin) AS admins \
                     FROM forge.users), \
                 ownerless AS ( \
                     SELECT g.id FROM forge.groups g \
                     JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
                     WHERE n.parent_id IS NULL \
                     AND NOT EXISTS (SELECT 1 FROM forge.group_members m \
                         JOIN forge.users u ON u.id = m.user_id \
                         WHERE m.group_id = g.id AND m.role = 'owner' \
                         AND u.state = 'active')) \
                 SELECT c.users, c.admins, \
                     (SELECT pg_catalog.count(*) FROM ownerless), \
                     ARRAY(SELECT id FROM ownerless ORDER BY id LIMIT $1) \
                 FROM counts c",
            )
            .bind(SHOWN_GROUP_IDS)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(Administration {
            users,
            active_human_admins,
            ownerless_groups,
            ownerless_group_ids: shown.into_iter().map(GroupId::new).collect(),
        }))
    })
    .await
}
