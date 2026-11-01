//! User commands (step D1, ADR-0015, ADR-0016 §6): create a user together
//! with their personal namespace, and disable a user. Each command is one
//! `ChangeTx`: the state rows and the one audit row commit together
//! (`forge.audited_change_present` refuses anything else). `user.*` changes
//! are audit-only: they publish no domain event (the `token.revoked` events of
//! the tokens a disable revokes belong to the tokens, ADR-0019). A disable
//! that would leave forge without an active human administrator, or a
//! top-level group without a direct active Owner, or a project without any
//! active Owner (D4 MR4), is refused unless forced
//! ([`Guard`]); [`bootstrap_admin`] restores an administrator (D3 MR2,
//! ADR-0019 §10–§11, `admin.rs`).

use forge_core::ids::{NamespaceId, UserId};
use forge_core::{AuditDetails, PathSegment, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::namespaces::{is_tombstoned, lock_path, path_taken};
use crate::{query_error, Db};

mod admin;
mod rename;

pub(crate) use admin::lock_admin_set;
pub use admin::{administration, bootstrap_admin, Administration, Bootstrap, Bootstrapped};
pub use rename::{rename_decided, UserRenameFacts};

/// Where a user's credentials are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    Local,
    Ldap,
}

impl AuthSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ldap => "ldap",
        }
    }
}

/// The request to create a user. `username` becomes the personal
/// namespace's top-level segment (`PathSegment::parse_top_level`).
#[derive(Debug, Clone)]
pub struct NewUser {
    pub kind: UserKind,
    pub username: String,
    pub display_name: String,
    /// Required (and unique ignoring case) for `UserKind::Human`.
    pub email: Option<String>,
    pub employee_number: Option<String>,
    pub external_id: Option<String>,
    pub auth_source: AuthSource,
    pub is_admin: bool,
}

impl NewUser {
    /// A local, non-admin human.
    pub fn human(username: &str, display_name: &str, email: &str) -> Self {
        Self {
            kind: UserKind::Human,
            username: username.to_owned(),
            display_name: display_name.to_owned(),
            email: Some(email.to_owned()),
            employee_number: None,
            external_id: None,
            auth_source: AuthSource::Local,
            is_admin: false,
        }
    }
}

/// What `create` wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    pub user_id: UserId,
    pub namespace_id: NamespaceId,
}

/// What `disable` did.
#[must_use = "AlreadyDisabled wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableOutcome {
    /// `active` became `disabled`: one `user.disabled` audit row.
    Disabled,
    /// Disabled with [`Guard::Force`] although the guard would have refused:
    /// what it left behind (also in the audit row). `forge doctor` reports
    /// it as `FG-AUTH-0010` until it is recovered.
    DisabledPastGuard {
        /// Top-level groups left without a direct active Owner.
        ownerless_groups: i64,
        /// Projects left without any active Owner, direct or inherited
        /// (D4 MR4).
        ownerless_projects: i64,
        /// No active human administrator is left.
        left_without_admin: bool,
    },
    /// Already disabled: nothing was written, not even an audit row.
    AlreadyDisabled,
}

/// Whether `disable` enforces the administration guard (ADR-0019 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Guard {
    /// Refuse a disable that would leave no active human administrator or a
    /// top-level group without a direct active Owner, or a project without
    /// any active Owner (`FG-AUTH-0009`).
    #[default]
    Enforce,
    /// Disable anyway and record it (`forced`). Only the operator CLI
    /// (`forge admin user disable --force`) passes this; HTTP never does.
    Force,
}

/// The audit resource type of users (tables.toml `audit_resource`).
const RESOURCE: &str = "user";

/// A unique violation the caller can act on, by constraint name; anything
/// else stays a query error. The key value (an email, an employee number)
/// never enters the error.
fn conflict(query: &'static str, e: sqlx::Error) -> ForgeError {
    let constraint = e
        .as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_owned);
    match constraint.as_deref() {
        Some("users_email_lower") => {
            ForgeError::new(ErrorCode::UserEmailTaken).with_context("reason", "email_taken")
        }
        Some("users_employee_number") => ForgeError::new(ErrorCode::UserIdentifierTaken)
            .with_context("reason", "employee_number_taken"),
        Some("users_external_id") => ForgeError::new(ErrorCode::UserIdentifierTaken)
            .with_context("reason", "external_id_taken"),
        Some("users_username_key" | "namespaces_sibling_key" | "namespaces_full_path_key") => {
            path_taken("sibling")
        }
        _ => query_error(query, e),
    }
}

/// `FG-REPO-0005` for a user (`reason=absent`); the name never enters it.
fn not_found() -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", RESOURCE)
        .with_context("reason", "absent")
}

fn request_invalid(reason: &'static str, len: usize) -> ForgeError {
    ForgeError::new(ErrorCode::UserRequestInvalid)
        .with_context("reason", reason)
        .with_context("len", len)
}

/// The field rules of forge.users (its CHECKs, migration 20261028100100),
/// checked before anything is written: `FG-AUTH-0006` with `reason` and
/// `len`, never the value.
fn validate(new: &NewUser) -> Result<(), ForgeError> {
    if new.kind == UserKind::Human && new.email.is_none() {
        return Err(request_invalid("email_missing", 0));
    }
    let fields = [
        (Some(new.display_name.as_str()), 255, "display_name_length"),
        (new.email.as_deref(), 320, "email_length"),
        (new.employee_number.as_deref(), 64, "employee_number_length"),
        (new.external_id.as_deref(), 255, "external_id_length"),
    ];
    for (value, max, reason) in fields {
        let Some(value) = value else { continue };
        let len = value.chars().count();
        if len == 0 || len > max {
            return Err(request_invalid(reason, len));
        }
        // PostgreSQL text cannot hold NUL; refuse it here, not as a query error.
        if value.contains('\0') {
            return Err(request_invalid("nul_character", len));
        }
    }
    Ok(())
}

/// Insert the user and their personal namespace (`kind = 'user'`,
/// `owner_id = users.id`, `segment = full_path = username`, `depth = 1`) and
/// the audit row `user.created` (resource `user`/`users.id`, details
/// `AuditDetails::UserCreated`) in one transaction; no event.
///
/// Errors (nothing is written): `FG-REPO-0007` invalid name, `FG-REPO-0008`
/// reserved name, `FG-REPO-0009` taken (`reason=sibling`) or tombstoned
/// (`reason=tombstoned`), `FG-AUTH-0005` email taken (ignoring case,
/// `reason=email_taken`), `FG-AUTH-0006` a field breaks its rule,
/// `FG-AUTH-0007` employee number or external id taken.
pub async fn create(db: &Db, ctx: &ChangeContext, new: &NewUser) -> Result<Created, ForgeError> {
    let segment = PathSegment::parse_top_level(&new.username)?;
    validate(new)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let created = insert(&mut tx, new, segment.as_str()).await?;
    let details = AuditDetails::UserCreated {
        kind: new.kind,
        namespace_id: created.namespace_id,
        is_admin: new.is_admin,
    };
    tx.commit(ChangeRecord::audit_only(
        &details,
        RESOURCE,
        created.user_id.get(),
    ))
    .await?
    .require_recorded(details.action())?;
    Ok(created)
}

/// The writes of `create` (and of `bootstrap_admin`'s `Create`), inside the
/// caller's change: the user and their personal namespace at `path` (a
/// validated top-level segment). The caller commits with its audit row.
async fn insert(tx: &mut ChangeTx, new: &NewUser, path: &str) -> Result<Created, ForgeError> {
    // A released path stays unusable. Every writer of tombstones takes the
    // path lock first, so a tombstone committed by another transaction is
    // either visible to the check below or written after this one commits.
    tx.step(|c| lock_path(c, path)).await?;
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    let user_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.users (kind, username, display_name, email, employee_number, \
                 external_id, auth_source, is_admin) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
            )
            .bind(new.kind.as_str())
            .bind(path)
            .bind(new.display_name.as_str())
            .bind(new.email.as_deref())
            .bind(new.employee_number.as_deref())
            .bind(new.external_id.as_deref())
            .bind(new.auth_source.as_str())
            .bind(new.is_admin)
            .fetch_one(c)
            .await
            .map_err(|e| conflict("users.create.user", e))
        })
        .await?;
    let namespace_id: i64 = tx
        .step(|c| async move {
            sqlx::query_scalar(
                "INSERT INTO forge.namespaces (kind, owner_id, segment, full_path, depth) \
                 VALUES ('user', $1, $2, $2, 1) RETURNING id",
            )
            .bind(user_id)
            .bind(path)
            .fetch_one(c)
            .await
            .map_err(|e| conflict("users.create.namespace", e))
        })
        .await?;
    // Defence in depth only: if a tombstone writer skipped the path lock (a
    // defect), this narrows the window in which its path slips through; the
    // guarantee is the lock rule every tombstone writer follows.
    if tx.step(|c| is_tombstoned(c, path)).await? {
        return Err(path_taken("tombstoned"));
    }
    Ok(Created {
        user_id: UserId::new(user_id),
        namespace_id: NamespaceId::new(namespace_id),
    })
}

/// `active` becomes `disabled` with one audit row `user.disabled`
/// (`AuditDetails::UserDisabled`, with `revoked_tokens` and the guard's
/// `forced` / `ownerless_groups` / `ownerless_projects` /
/// `left_without_admin`); the user's active
/// tokens are revoked in the same change (`revoked_reason = 'user_disabled'`,
/// one `token.revoked` event each, ADR-0019). Disabling a disabled user is
/// `AlreadyDisabled` and writes nothing. An unknown user is `FG-REPO-0005`
/// (`resource=user`, `reason=absent`).
///
/// Enforces the administration guard ([`Guard::Enforce`]): leaving no
/// active human administrator is `FG-AUTH-0009` `last_admin`, leaving a
/// top-level group without a direct active Owner, or a project without any
/// active Owner (direct or inherited, D4 MR4), is `last_owner`; a refused
/// disable writes nothing and revokes no token. [`disable_with`] can force it.
pub async fn disable(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
) -> Result<DisableOutcome, ForgeError> {
    disable_with(db, ctx, user, Guard::Enforce).await
}

/// [`disable`] with an explicit [`Guard`]. Locks `users.admin_set`, then the
/// user row `FOR UPDATE`, then counts without locks (ADR-0019 §11); group
/// and project rows are never locked.
pub async fn disable_with(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    guard: Guard,
) -> Result<DisableOutcome, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        tx.step(lock_admin_set).await?;
        let row: Option<(String, String, bool)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT kind, state, is_admin FROM forge.users WHERE id = $1 FOR UPDATE",
                )
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("users.disable.lock", e))
            })
            .await?;
        let Some((kind, state, is_admin)) = row else {
            return Err(not_found());
        };
        if state == "disabled" {
            // Nothing was written; `settle` rolls the lock back explicitly.
            return Ok(Settle::Discard(DisableOutcome::AlreadyDisabled));
        }
        let kind = UserKind::parse(&kind).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_user_kind")
                .with_context("table", "users")
        })?;
        let leaves = tx
            .step(|c| admin::what_disable_leaves(c, user, kind, is_admin))
            .await?;
        let forced = guard == Guard::Force;
        if !forced {
            if let Some(refused) = leaves.refusal(user) {
                // Nothing was written and no token was revoked; `settle` rolls
                // the locks back explicitly.
                return Err(refused);
            }
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.users SET state = 'disabled', updated_at = now() WHERE id = $1",
            )
            .bind(user.get())
            .execute(c)
            .await
            .map_err(|e| query_error("users.disable.update", e))
        })
        .await?;
        // ADR-0019: the user's active tokens die with the account, in the same
        // change, so re-enabling the user never revives them. `tokens::issue`
        // locks the user row too, so no token is issued in between.
        let revoked: Vec<(i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'user_disabled' \
                 WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() \
                 RETURNING id, public_id",
                )
                .bind(user.get())
                .fetch_all(c)
                .await
                .map_err(|e| query_error("users.disable.revoke_tokens", e))
            })
            .await?;
        let details = AuditDetails::UserDisabled {
            kind,
            revoked_tokens: i64::try_from(revoked.len()).unwrap_or(i64::MAX),
            forced,
            ownerless_groups: leaves.ownerless_count(),
            ownerless_projects: leaves.ownerless_project_count(),
            left_without_admin: leaves.without_admin,
        };
        let mut record = ChangeRecord::audit_only(&details, RESOURCE, user.get());
        // One token.revoked event per token (at most MAX_ACTIVE_TOKENS = 50,
        // inside the 100 events one change may carry).
        for (token_id, public_id) in revoked {
            let token = AuditDetails::TokenRevoked {
                token_id,
                user,
                public_id,
            };
            record.events.push(crate::tokens::event(&token, token_id)?);
        }
        let outcome = if leaves.is_clean() {
            DisableOutcome::Disabled
        } else {
            DisableOutcome::DisabledPastGuard {
                ownerless_groups: leaves.ownerless_count(),
                ownerless_projects: leaves.ownerless_project_count(),
                left_without_admin: leaves.without_admin,
            }
        };
        Ok(Settle::Commit(record, outcome))
    }
    .await;
    tx.settle("user.disabled", outcome).await
}

/// The id of the user named `username` (the operator CLI names users this
/// way). Reads on the read pool. An unknown name is `FG-REPO-0005`
/// (`resource=user`, `reason=absent`); the name never enters the error.
pub async fn id_by_username(db: &Db, username: &str) -> Result<UserId, ForgeError> {
    let id: Option<i64> = db
        .within("users.id_by_username", async {
            let mut conn = db.acquire().await?;
            sqlx::query_scalar("SELECT id FROM forge.users WHERE username = $1")
                .bind(username)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| crate::read_query_error("users.id_by_username", e))
        })
        .await?;
    id.map(UserId::new).ok_or_else(not_found)
}

#[cfg(test)]
mod tests;
