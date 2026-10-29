//! User commands (step D1, ADR-0015, ADR-0016 §6): create a user together
//! with their personal namespace, and disable a user. Each command is one
//! `ChangeTx`: the state rows and the one audit row commit together
//! (`forge.audited_change_present` refuses anything else). `user.*` changes
//! are audit-only: they publish no domain event.

use forge_core::ids::{NamespaceId, UserId};
use forge_core::{AuditDetails, PathSegment, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, Outcome};
use crate::namespaces::{is_tombstoned, lock_path};
use crate::{query_error, Db};

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
    /// Already disabled: nothing was written, not even an audit row.
    AlreadyDisabled,
}

/// The audit resource type of users (tables.toml `audit_resource`).
const RESOURCE: &str = "user";

fn path_taken(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NamespacePathTaken).with_context("reason", reason)
}

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

/// A commit of a change without events is never deduplicated; a `Deduped`
/// would mean the change was rolled back, so it is not reported as success.
fn recorded(committed: Committed, action: &'static str) -> Result<(), ForgeError> {
    match committed {
        Committed::Recorded { .. } => Ok(()),
        Committed::Deduped => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unexpected_dedup")
            .with_context("action", action)),
    }
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
    let path = segment.as_str();
    let mut tx = ChangeTx::begin(db, ctx).await?;
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
    let created = Created {
        user_id: UserId::new(user_id),
        namespace_id: NamespaceId::new(namespace_id),
    };
    let details = AuditDetails::UserCreated {
        kind: new.kind,
        namespace_id: created.namespace_id,
        is_admin: new.is_admin,
    };
    let committed = tx.commit(audit_only(&details, created.user_id)).await?;
    recorded(committed, details.action())?;
    Ok(created)
}

/// `active` becomes `disabled` with one audit row `user.disabled`
/// (`AuditDetails::UserDisabled`); no event. Disabling a disabled user is
/// `AlreadyDisabled` and writes nothing. An unknown user is `FG-REPO-0005`
/// (`resource=user`, `reason=absent`).
pub async fn disable(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
) -> Result<DisableOutcome, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let row: Option<(String, String)> = tx
        .step(|c| async move {
            sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR UPDATE")
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("users.disable.lock", e))
        })
        .await?;
    let Some((kind, state)) = row else {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", RESOURCE)
            .with_context("reason", "absent"));
    };
    if state == "disabled" {
        // Dropping the transaction rolls back the lock; nothing was written.
        return Ok(DisableOutcome::AlreadyDisabled);
    }
    let kind = UserKind::parse(&kind).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_user_kind")
            .with_context("table", "users")
    })?;
    tx.step(|c| async move {
        sqlx::query("UPDATE forge.users SET state = 'disabled', updated_at = now() WHERE id = $1")
            .bind(user.get())
            .execute(c)
            .await
            .map_err(|e| query_error("users.disable.update", e))
    })
    .await?;
    let details = AuditDetails::UserDisabled { kind };
    let committed = tx.commit(audit_only(&details, user)).await?;
    recorded(committed, details.action())?;
    Ok(DisableOutcome::Disabled)
}

/// The record of an audit-only user change.
fn audit_only(details: &AuditDetails, user_id: UserId) -> ChangeRecord {
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: RESOURCE,
            resource_id: Some(user_id.get()),
            project_id: None,
            error_code: None,
            details: details.to_value(),
        },
        events: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
