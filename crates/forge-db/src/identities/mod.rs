//! OIDC identities linked to users (M2 MR4, ADR-0021 §4): the sign-in's
//! link-or-create step ([`login_decided`]) and the operator's link
//! ([`link_by_operator`], `forge admin user link-identity`).
//!
//! The rule of the table `forge.user_identities` (plan 09 §8.1): the IdP's
//! `(issuer, subject)` is the key; an existing user is found by the
//! employee number and only with a verified, matching e-mail; an e-mail
//! alone never links; an administrator is linked only by the operator.
//! Every refusal is `FG-AUTH-0014` with `reason` (never a claim value).
//!
//! Each command is one `ChangeTx`: the identity row, the user (when one is
//! created) and the audit rows (`user.identity_linked`, `user.created`)
//! commit together. `last_login_at` is unaudited and written after the
//! change ([`record_login`]).

use forge_core::ids::{NamespaceId, UserId};
use forge_core::{AuditDetails, IdentityLinkVia, PathSegment, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::users::{AuthSource, NewUser};
use crate::{query_error, Db};

/// The audit resource type (tables.toml `audit_resource`).
const RESOURCE: &str = "user";
/// The longest issuer / subject stored (the table's CHECKs).
const MAX_CHARS: usize = 255;
/// The longest display name a created user gets (forge.users' CHECK).
const MAX_DISPLAY_NAME: usize = 255;

/// What the verified ID token says about the person (ADR-0021 §4). The
/// subject and the claims are personal data: never logged, never audited.
#[derive(Debug, Clone, Copy)]
pub struct LoginIdentity<'a> {
    /// `iss`, equal to the configured issuer.
    pub issuer: &'a str,
    /// `sub`.
    pub subject: &'a str,
    /// The `auth.oidc.employee_number_claim` claim.
    pub employee_number: Option<&'a str>,
    pub email: Option<&'a str>,
    /// `email_verified = true` (anything else is false).
    pub email_verified: bool,
    pub preferred_username: Option<&'a str>,
    /// `name`.
    pub name: Option<&'a str>,
}

/// `auth.oidc.auto_link` (ADR-0021 §13): `false` links no existing user
/// automatically; the operator links them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkPolicy {
    pub auto_link: bool,
}

/// The account a sign-in found (or will create), as the admission decision
/// sees it (`forge_auth::AdmissionAccount`; forge-db sits below forge-auth).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginAccount {
    New,
    Active,
    Disabled,
    NotHuman,
}

/// What [`login_decided`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The identity was linked before; nothing was written.
    Existing { user: UserId },
    /// Linked to the user with the employee number (`user.identity_linked`).
    Linked { user: UserId },
    /// A new user with their personal namespace (`user.created` and
    /// `user.identity_linked` in one change).
    Created {
        user: UserId,
        namespace_id: NamespaceId,
    },
}

impl LoginOutcome {
    pub fn user(self) -> UserId {
        match self {
            Self::Existing { user } | Self::Linked { user } | Self::Created { user, .. } => user,
        }
    }

    /// For the log: `existing`, `linked`, `created`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Existing { .. } => "existing",
            Self::Linked { .. } => "linked",
            Self::Created { .. } => "created",
        }
    }
}

/// What [`link_by_operator`] did.
#[must_use = "AlreadyLinked wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorLink {
    /// One `user.identity_linked` row (`via=operator`).
    Linked,
    /// The identity is already this user's: nothing was written.
    AlreadyLinked,
}

/// `FG-AUTH-0014 IdentityLinkConflict` with the log `reason`.
fn conflict(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::IdentityLinkConflict).with_context("reason", reason)
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// The issuer and subject fit the table (1..=255 characters, no NUL); the
/// caller verified the ID token, so a misfit is a caller bug.
fn fits(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= MAX_CHARS && !value.contains('\0')
}

fn account_of(kind: &str, state: &str) -> LoginAccount {
    if kind != UserKind::Human.as_str() {
        LoginAccount::NotHuman
    } else if state == "active" {
        LoginAccount::Active
    } else {
        LoginAccount::Disabled
    }
}

/// A unique violation of `user_identities` as the conflict it means.
fn identity_conflict(query: &'static str, e: sqlx::Error) -> ForgeError {
    let constraint = e
        .as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_owned);
    match constraint.as_deref() {
        Some("user_identities_issuer_subject_key") => conflict("subject_linked"),
        Some("user_identities_user_issuer_key") => conflict("already_linked"),
        _ => query_error(query, e),
    }
}

/// The audit row `user.identity_linked` of `user`.
fn linked_entry(user: UserId, issuer: &str, via: IdentityLinkVia) -> AuditEntry {
    let details = AuditDetails::UserIdentityLinked {
        issuer: issuer.to_owned(),
        via,
    };
    ChangeRecord::audit_only(&details, RESOURCE, user.get()).audit
}

async fn insert_identity(
    tx: &mut ChangeTx,
    user: UserId,
    issuer: &str,
    subject: &str,
) -> Result<(), ForgeError> {
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.user_identities (user_id, issuer, subject, last_login_at) \
             VALUES ($1, $2, $3, now())",
        )
        .bind(user.get())
        .bind(issuer)
        .bind(subject)
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| identity_conflict("identities.insert", e))
    })
    .await
}

/// The sign-in's link-or-create step (ADR-0021 §3, §4), with the admission
/// decided inside the change: `decide` gets the account the identity
/// belongs to (or `New`) and must be `forge_auth::authorize(anonymous,
/// SessionCreate, Admission { access_role, account })` — its error is
/// returned unchanged and nothing is written. Only forge-oidc calls this
/// (test `decided_callers`).
///
/// In order, in one change (the employee number's user row `FOR UPDATE`):
/// 1. `(issuer, subject)` is linked → that user (`Existing`, after
///    `decide`; nothing written — the caller records the login,
///    [`record_login`]). This includes an administrator the operator
///    linked (ADR-0021 §4 2a): `privileged_account` only stops the
///    *automatic* link of step 3, so a linked administrator with the
///    access role signs in like anyone else (`SessionCreate` allows an
///    active human; forge-oidc `tests/flow.rs`
///    `linked_disabled_users_and_bots_are_refused_and_linked_admins_sign_in`);
/// 2. no employee number claim → `employee_number_missing` (ADR-0021 §4:
///    the claim is required, so a broken mapper never duplicates users);
/// 3. a user has the employee number → `decide` on that account (a bot is
///    `NotHuman`), then `auto_link_disabled`, `employee_number_linked`
///    (linked to another subject of this issuer), `privileged_account` (an
///    administrator), `email_missing`, `email_unverified`, `email_mismatch`;
///    otherwise linked (`via=employee_number`);
/// 4. nobody has it → `decide(New)`, then `email_missing`,
///    `email_unverified`, `username_unavailable` (`preferred_username`
///    missing, invalid, reserved, taken, tombstoned), `email_taken`;
///    otherwise the user is created like `users::create` (path lock,
///    tombstone check, personal namespace; `auth_source = 'local'`, never
///    an administrator) and linked (`via=first_login`).
///
/// A race with another sign-in of the same identity is `subject_linked` /
/// `already_linked` / `employee_number_taken` (nothing written).
pub async fn login_decided(
    db: &Db,
    ctx: &ChangeContext,
    identity: &LoginIdentity<'_>,
    policy: LinkPolicy,
    decide: impl FnOnce(LoginAccount) -> Result<(), ForgeError>,
) -> Result<LoginOutcome, ForgeError> {
    if !fits(identity.issuer) || !fits(identity.subject) {
        return Err(internal("identity_out_of_range"));
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = login_steps(&mut tx, identity, policy, decide).await;
    tx.settle("user.identity_linked", outcome).await
}

async fn login_steps(
    tx: &mut ChangeTx,
    identity: &LoginIdentity<'_>,
    policy: LinkPolicy,
    decide: impl FnOnce(LoginAccount) -> Result<(), ForgeError>,
) -> Result<Settle<LoginOutcome>, ForgeError> {
    let (issuer, subject) = (identity.issuer, identity.subject);
    let linked: Option<(i64, String, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT u.id, u.kind, u.state FROM forge.user_identities i \
                 JOIN forge.users u ON u.id = i.user_id \
                 WHERE i.issuer = $1 AND i.subject = $2 FOR SHARE OF u",
            )
            .bind(issuer)
            .bind(subject)
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("identities.login.linked", e))
        })
        .await?;
    if let Some((user, kind, state)) = linked {
        decide(account_of(&kind, &state))?;
        return Ok(Settle::Discard(LoginOutcome::Existing {
            user: UserId::new(user),
        }));
    }

    let Some(employee_number) = identity.employee_number.filter(|e| !e.is_empty()) else {
        return Err(conflict("employee_number_missing"));
    };
    let email = identity.email.filter(|e| !e.is_empty());
    let holder: Option<(i64, String, String, bool, Option<bool>, bool)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT u.id, u.kind, u.state, u.is_admin, lower(u.email) = lower($3), \
                 EXISTS (SELECT 1 FROM forge.user_identities i \
                         WHERE i.user_id = u.id AND i.issuer = $2) \
                 FROM forge.users u WHERE u.employee_number = $1 FOR UPDATE",
            )
            .bind(employee_number)
            .bind(issuer)
            .bind(email)
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("identities.login.employee_number", e))
        })
        .await?;

    if let Some((user, kind, state, is_admin, same_email, has_identity)) = holder {
        let user = UserId::new(user);
        decide(account_of(&kind, &state))?;
        if !policy.auto_link {
            return Err(conflict("auto_link_disabled"));
        }
        if has_identity {
            return Err(conflict("employee_number_linked"));
        }
        if is_admin {
            return Err(conflict("privileged_account"));
        }
        check_email(email, identity.email_verified)?;
        if same_email != Some(true) {
            return Err(conflict("email_mismatch"));
        }
        insert_identity(tx, user, issuer, subject).await?;
        let record = ChangeRecord {
            audit: linked_entry(user, issuer, IdentityLinkVia::EmployeeNumber),
            affected: Vec::new(),
            events: Vec::new(),
        };
        return Ok(Settle::Commit(record, LoginOutcome::Linked { user }));
    }

    decide(LoginAccount::New)?;
    let email = check_email(email, identity.email_verified)?;
    let username = identity
        .preferred_username
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| conflict("username_unavailable"))?;
    let segment =
        PathSegment::parse_top_level(&username).map_err(|_| conflict("username_unavailable"))?;
    let display_name: String = identity
        .name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(segment.as_str())
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DISPLAY_NAME)
        .collect();
    let new = NewUser {
        kind: UserKind::Human,
        username: segment.as_str().to_owned(),
        display_name,
        email: Some(email.to_owned()),
        employee_number: Some(employee_number.to_owned()),
        external_id: None,
        auth_source: AuthSource::Local,
        is_admin: false,
    };
    crate::users::validate(&new).map_err(|e| {
        let detail = e
            .context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map_or_else(String::new, |(_, v)| v.clone());
        conflict("claim_invalid").with_context("detail", detail)
    })?;
    let created = crate::users::insert(tx, &new, segment.as_str())
        .await
        .map_err(|e| {
            let code = e.code();
            if matches!(
                code,
                ErrorCode::NamespacePathTaken
                    | ErrorCode::NamespacePathReserved
                    | ErrorCode::NamespacePathInvalid
            ) {
                conflict("username_unavailable")
            } else if code == ErrorCode::UserEmailTaken {
                conflict("email_taken")
            } else if code == ErrorCode::UserIdentifierTaken {
                conflict("employee_number_taken")
            } else {
                // A database failure stays what it is (FG-DB-*).
                e
            }
        })?;
    insert_identity(tx, created.user_id, issuer, subject).await?;
    let details = AuditDetails::UserCreatedOnLogin {
        namespace_id: created.namespace_id,
    };
    let mut record = ChangeRecord::audit_only(&details, RESOURCE, created.user_id.get());
    record.affected.push(linked_entry(
        created.user_id,
        issuer,
        IdentityLinkVia::FirstLogin,
    ));
    Ok(Settle::Commit(
        record,
        LoginOutcome::Created {
            user: created.user_id,
            namespace_id: created.namespace_id,
        },
    ))
}

/// A usable e-mail claim: present and verified.
fn check_email(email: Option<&str>, verified: bool) -> Result<&str, ForgeError> {
    let email = email.ok_or_else(|| conflict("email_missing"))?;
    if !verified {
        return Err(conflict("email_unverified"));
    }
    Ok(email)
}

/// Record a sign-in on the identity (`last_login_at = now()`, unaudited,
/// ADR-0021 §4). A failure is returned for the caller to log as
/// `FG-DB-0002` at warn and go on: losing the time loses nothing else.
pub async fn record_login(db: &Db, issuer: &str, subject: &str) -> Result<(), ForgeError> {
    db.within("identities.record_login", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("identities.record_login.acquire", e))?;
        sqlx::query(
            "UPDATE forge.user_identities SET last_login_at = now() \
             WHERE issuer = $1 AND subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|e| query_error("identities.record_login", e))
    })
    .await
}

/// `forge admin user link-identity` (ADR-0021 §4 2a, ADR-0019 §8): link
/// `(issuer, subject)` to `user` as the host operator — actor `system` only
/// (`FG-SYS-0001 reason=operator_not_system` otherwise: never from HTTP).
/// The way to link an administrator, and to settle every `FG-AUTH-0014`.
///
/// One `user.identity_linked` row (`via=operator`). The same identity of
/// the same user is `AlreadyLinked` (nothing written); an identity of
/// another user is `FG-AUTH-0014 subject_linked`; a user with another
/// identity of this issuer `already_linked` (re-linking needs a DELETE the
/// application role does not have: a later, approved migration). An
/// unknown user is `FG-REPO-0005`; an issuer or subject outside 1..=255
/// characters `FG-AUTH-0006` (`issuer_length` / `subject_length`).
pub async fn link_by_operator(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    issuer: &str,
    subject: &str,
) -> Result<OperatorLink, ForgeError> {
    if ctx.actor != Actor::System {
        return Err(internal("operator_not_system"));
    }
    for (value, reason) in [(issuer, "issuer_length"), (subject, "subject_length")] {
        if !fits(value) {
            return Err(ForgeError::new(ErrorCode::UserRequestInvalid)
                .with_context("reason", reason)
                .with_context("len", value.chars().count()));
        }
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let known: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT id FROM forge.users WHERE id = $1 FOR UPDATE")
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("identities.link_by_operator.user", e))
            })
            .await?;
        if known.is_none() {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", RESOURCE)
                .with_context("reason", "absent"));
        }
        let owner: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "SELECT user_id FROM forge.user_identities WHERE issuer = $1 AND subject = $2",
                )
                .bind(issuer)
                .bind(subject)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("identities.link_by_operator.owner", e))
            })
            .await?;
        match owner {
            Some(owner) if owner == user.get() => {
                return Ok(Settle::Discard(OperatorLink::AlreadyLinked))
            }
            Some(_) => return Err(conflict("subject_linked")),
            None => {}
        }
        insert_identity(&mut tx, user, issuer, subject).await?;
        let record = ChangeRecord {
            audit: linked_entry(user, issuer, IdentityLinkVia::Operator),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(record, OperatorLink::Linked))
    }
    .await;
    tx.settle("user.identity_linked", outcome).await
}

#[cfg(test)]
mod tests;
