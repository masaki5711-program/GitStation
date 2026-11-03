//! Service accounts (M3 S0, plan 11 §5 S0, ADR-0019 Addendum S0): a
//! `forge.users` row with `kind = 'service_account'` (its personal
//! namespace, memberships and tokens are a user's) plus its
//! `forge.service_account_profiles` row — the person who answers for it
//! (`owner`), why it exists (`purpose`) and who created it.
//!
//! - [`create`]: the user, its namespace and its profile in one change,
//!   one `service_account.created` audit row (resource `user`, no event).
//! - [`update`]: display name, purpose, owner; one
//!   `service_account.updated` row (nothing written when nothing changes).
//! - [`target`], [`get`], [`list`]: reads on the read pool.
//! - [`health`]: `forge doctor`'s `service_accounts.tokens_expiring`
//!   (`FG-AUTH-0019`) and `service_accounts.ownerless` (`FG-AUTH-0010
//!   reason=ownerless_service_account`).
//!
//! Tokens are issued by `tokens::issue_for_service_account` and revoked by
//! `tokens::revoke`; disabling is `users::disable` (it revokes the
//! account's tokens with `revoked_reason = 'user_disabled'`). These
//! commands do not decide who may call them: forge-hosting calls them on
//! a proof of the matching action (`authorize()`), the database CHECKs and
//! the guard triggers `service_account_profiles_kind_guard`,
//! `user_identities_human_guard` and `users_kind_immutable` are the backstop.

use std::time::Duration;

use forge_core::ids::UserId;
use forge_core::{AuditDetails, PathSegment, ServiceAccountField, UserKind};
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::users::{AuthSource, NewUser};
use crate::{query_error, read_query_error, Db};

#[cfg(test)]
mod tests;

/// The longest purpose, in characters (the CHECK
/// `service_account_profiles_purpose_length`).
pub const MAX_PURPOSE_CHARS: usize = 500;
/// The most service accounts [`list`] returns (it says when it cut).
pub const MAX_LISTED: i64 = 500;
/// `forge doctor` warns when a service account's last active token expires
/// within this window (plan 11 §5 S0: rotate before the 365-day limit).
pub const EXPIRY_WARNING: Duration = Duration::from_secs(14 * 24 * 3600);
/// How many ids a doctor finding names (never names).
const SHOWN_IDS: i64 = 10;

/// The audit resource of every service account change (tables.toml).
const RESOURCE: &str = "user";

/// The request to create a service account.
#[derive(Debug, Clone)]
pub struct NewServiceAccount {
    /// Also the personal namespace's path (`PathSegment::parse_top_level`).
    pub username: String,
    pub display_name: String,
    /// The person who answers for it: an active human.
    pub owner: UserId,
    /// 1..=500 characters, no control characters.
    pub purpose: String,
}

/// What [`create`] wrote: the account's id and its personal namespace.
pub use crate::users::Created;

/// The fields an update may change (`None`: unchanged).
#[derive(Debug, Clone, Default)]
pub struct Changes {
    pub display_name: Option<String>,
    pub purpose: Option<String>,
    pub owner: Option<UserId>,
}

/// What [`update`] did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// One `service_account.updated` row naming these fields.
    Updated(Vec<ServiceAccountField>),
    /// Every given value equals the stored one: nothing was written.
    Unchanged,
}

/// A user as an administration route decides on it (`authorize()`'s
/// `Resource::ServiceAccount`): its kind and whether it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub kind: UserKind,
    pub active: bool,
}

/// One service account as the administration sees it (no personal data:
/// a service account has none, and the owner is named by id and username).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAccount {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
    pub active: bool,
    /// `None` only for a service account created before S0 (no profile).
    pub owner: Option<Owner>,
    pub purpose: Option<String>,
    pub created_by: Option<UserId>,
    /// RFC 3339, UTC.
    pub created_at: String,
    /// Active (not revoked, not expired) tokens.
    pub active_tokens: i64,
    /// The latest expiry of an active token (RFC 3339, UTC).
    pub last_token_expiry: Option<String>,
}

/// The person a service account belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub id: UserId,
    pub username: String,
    pub active: bool,
}

/// What [`list`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub items: Vec<ServiceAccount>,
    /// More than [`MAX_LISTED`] exist.
    pub truncated: bool,
}

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::UserRequestInvalid).with_context("reason", reason)
}

/// The purpose's rules, before anything is written (`FG-AUTH-0006` with
/// `reason` and `len`, never the text).
fn check_purpose(purpose: &str) -> Result<(), ForgeError> {
    let len = purpose.chars().count();
    if len == 0 || len > MAX_PURPOSE_CHARS || purpose.trim().is_empty() {
        return Err(invalid("purpose_length").with_context("len", len));
    }
    if purpose.chars().any(char::is_control) {
        return Err(invalid("purpose_control_character").with_context("len", len));
    }
    Ok(())
}

/// Lock the owner-to-be (`FOR SHARE`, so a concurrent disable waits) and
/// check it is an active person: `FG-AUTH-0006` `owner_absent`,
/// `owner_not_human` or `owner_disabled` otherwise.
async fn lock_owner(tx: &mut ChangeTx, owner: UserId) -> Result<(), ForgeError> {
    let row: Option<(String, String)> = tx
        .step(|c| async move {
            sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR SHARE")
                .bind(owner.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("service_accounts.owner", e))
        })
        .await?;
    let refuse = |reason| invalid(reason).with_context("owner", owner.get());
    match row {
        None => Err(refuse("owner_absent")),
        Some((kind, _)) if kind != UserKind::Human.as_str() => Err(refuse("owner_not_human")),
        Some((_, state)) if state != "active" => Err(refuse("owner_disabled")),
        Some(_) => Ok(()),
    }
}

/// Create a service account: the user (`kind = 'service_account'`, no
/// e-mail or other personal data, never an administrator), its personal
/// namespace and its profile, with one `service_account.created` audit row
/// (`AuditDetails::ServiceAccountCreated`). `created_by` is the
/// administrator (the change's actor).
///
/// Errors (nothing is written): the `users::create` ones for the username
/// and display name (`FG-REPO-0007` / `0008` / `0009`, `FG-AUTH-0006`), and
/// `FG-AUTH-0006` `purpose_length`, `purpose_control_character`,
/// `owner_absent`, `owner_not_human`, `owner_disabled`.
pub async fn create(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewServiceAccount,
    created_by: UserId,
) -> Result<Created, ForgeError> {
    let segment = PathSegment::parse_top_level(&new.username)?;
    let user = NewUser {
        kind: UserKind::ServiceAccount,
        username: new.username.clone(), // clone: NewUser owns its fields
        display_name: new.display_name.clone(), // clone: as above
        email: None,
        employee_number: None,
        external_id: None,
        auth_source: AuthSource::Local,
        is_admin: false,
    };
    crate::users::validate(&user)?;
    check_purpose(&new.purpose)?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        lock_owner(&mut tx, new.owner).await?;
        let created = crate::users::insert(&mut tx, &user, segment.as_str()).await?;
        let purpose = new.purpose.as_str();
        let owner = new.owner;
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.service_account_profiles \
                 (user_id, owner_user_id, purpose, created_by) VALUES ($1, $2, $3, $4)",
            )
            .bind(created.user_id.get())
            .bind(owner.get())
            .bind(purpose)
            .bind(created_by.get())
            .execute(c)
            .await
            .map_err(|e| query_error("service_accounts.create.profile", e))
        })
        .await?;
        let details = AuditDetails::ServiceAccountCreated {
            namespace_id: created.namespace_id,
            owner,
            purpose_chars: i64::try_from(purpose.chars().count()).unwrap_or(i64::MAX),
        };
        let record = ChangeRecord::audit_only(&details, RESOURCE, created.user_id.get());
        Ok(Settle::Commit(record, created))
    }
    .await;
    tx.settle("service_account.created", outcome).await
}

/// Change a service account's display name, purpose or owner. The profile
/// row is locked `FOR UPDATE`; a new owner is checked like [`create`]'s.
/// Writes nothing when every given value equals the stored one.
///
/// Errors (nothing is written): `FG-REPO-0005` (`resource=service_account`,
/// `reason=absent`) when `user` is no service account with a profile,
/// `FG-AUTH-0006` for a field that breaks its rule (`display_name_length`,
/// `nul_character`, the purpose and owner reasons of [`create`]).
pub async fn update(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    changes: &Changes,
) -> Result<UpdateOutcome, ForgeError> {
    if let Some(name) = &changes.display_name {
        let len = name.chars().count();
        if len == 0 || len > 255 {
            return Err(invalid("display_name_length").with_context("len", len));
        }
        if name.contains('\0') {
            return Err(invalid("nul_character").with_context("len", len));
        }
    }
    if let Some(purpose) = &changes.purpose {
        check_purpose(purpose)?;
    }
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(String, String, i64)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT u.display_name, p.purpose, p.owner_user_id \
                     FROM forge.service_account_profiles p \
                     JOIN forge.users u ON u.id = p.user_id \
                     WHERE p.user_id = $1 FOR UPDATE",
                )
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("service_accounts.update.lock", e))
            })
            .await?;
        let Some((display_name, purpose, owner)) = row else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "service_account")
                .with_context("reason", "absent"));
        };
        let owner_from = UserId::new(owner);
        let mut fields = Vec::new();
        let new_name = changes
            .display_name
            .as_deref()
            .filter(|n| *n != display_name);
        let new_purpose = changes.purpose.as_deref().filter(|p| *p != purpose);
        let new_owner = changes.owner.filter(|o| *o != owner_from);
        if let Some(name) = new_name {
            fields.push(ServiceAccountField::DisplayName);
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.users SET display_name = $2, updated_at = now() WHERE id = $1",
                )
                .bind(user.get())
                .bind(name)
                .execute(c)
                .await
                .map_err(|e| query_error("service_accounts.update.user", e))
            })
            .await?;
        }
        if let Some(to) = new_owner {
            lock_owner(&mut tx, to).await?;
            fields.push(ServiceAccountField::Owner);
        }
        if new_purpose.is_some() {
            fields.push(ServiceAccountField::Purpose);
        }
        if new_owner.is_some() || new_purpose.is_some() {
            let owner_to = new_owner.unwrap_or(owner_from);
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.service_account_profiles \
                     SET owner_user_id = $2, purpose = COALESCE($3, purpose), updated_at = now() \
                     WHERE user_id = $1",
                )
                .bind(user.get())
                .bind(owner_to.get())
                .bind(new_purpose)
                .execute(c)
                .await
                .map_err(|e| query_error("service_accounts.update.profile", e))
            })
            .await?;
        }
        if fields.is_empty() {
            return Ok(Settle::Discard(UpdateOutcome::Unchanged));
        }
        fields.sort();
        let details = AuditDetails::ServiceAccountUpdated {
            owner_from,
            owner_to: new_owner.unwrap_or(owner_from),
            fields: fields.clone(), // clone: the audit row and the answer both keep them
        };
        let record = ChangeRecord::audit_only(&details, RESOURCE, user.get());
        Ok(Settle::Commit(record, UpdateOutcome::Updated(fields)))
    }
    .await;
    tx.settle("service_account.updated", outcome).await
}

/// The kind and state of user `user` (`None`: no such user), for the
/// administration routes' loaders. Read pool, one statement.
pub async fn target(db: &Db, user: UserId) -> Result<Option<Target>, ForgeError> {
    const QUERY: &str = "service_accounts.target";
    let row: Option<(String, String)> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1")
                .bind(user.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    row.map(|(kind, state)| {
        Ok(Target {
            kind: crate::users::parse_kind(user.get(), &kind)?,
            active: state == "active",
        })
    })
    .transpose()
}

/// The columns of [`get`] and [`list`] (one row per service account).
const SELECT: &str = concat!(
    "SELECT u.id, u.username, u.display_name, u.state = 'active', ",
    "p.owner_user_id, o.username, o.state = 'active', p.purpose, p.created_by, ",
    "to_char(u.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), ",
    "(SELECT count(*) FROM forge.tokens t WHERE t.user_id = u.id ",
    " AND t.revoked_at IS NULL AND t.expires_at > now()), ",
    "(SELECT to_char(max(t.expires_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') ",
    " FROM forge.tokens t WHERE t.user_id = u.id ",
    " AND t.revoked_at IS NULL AND t.expires_at > now()) ",
    "FROM forge.users u ",
    "LEFT JOIN forge.service_account_profiles p ON p.user_id = u.id ",
    "LEFT JOIN forge.users o ON o.id = p.owner_user_id ",
    "WHERE u.kind = 'service_account' "
);

type Row = (
    i64,
    String,
    String,
    bool,
    Option<i64>,
    Option<String>,
    Option<bool>,
    Option<String>,
    Option<i64>,
    String,
    i64,
    Option<String>,
);

fn account(row: Row) -> ServiceAccount {
    let (
        id,
        username,
        display_name,
        active,
        owner_id,
        owner_username,
        owner_active,
        purpose,
        created_by,
        created_at,
        active_tokens,
        last_token_expiry,
    ) = row;
    ServiceAccount {
        id: UserId::new(id),
        username,
        display_name,
        active,
        owner: match (owner_id, owner_username) {
            (Some(id), Some(username)) => Some(Owner {
                id: UserId::new(id),
                username,
                active: owner_active.unwrap_or(false),
            }),
            (Some(_) | None, _) => None,
        },
        purpose,
        created_by: created_by.map(UserId::new),
        created_at,
        active_tokens,
        last_token_expiry,
    }
}

/// Service account `user` (`None`: absent, or a user of another kind).
pub async fn get(db: &Db, user: UserId) -> Result<Option<ServiceAccount>, ForgeError> {
    const QUERY: &str = "service_accounts.get";
    let row: Option<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} AND u.id = $1"))
                .bind(user.get())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(account))
}

/// Every service account by id (at most [`MAX_LISTED`]; `truncated` says
/// there are more).
pub async fn list(db: &Db) -> Result<Listed, ForgeError> {
    const QUERY: &str = "service_accounts.list";
    let rows: Vec<Row> = db
        .within(QUERY, async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(&format!("{SELECT} ORDER BY u.id LIMIT $1"))
                .bind(MAX_LISTED + 1)
                .fetch_all(&mut *conn)
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > MAX_LISTED;
    let mut items: Vec<ServiceAccount> = rows.into_iter().map(account).collect();
    items.truncate(usize::try_from(MAX_LISTED).unwrap_or(usize::MAX));
    Ok(Listed { items, truncated })
}

/// What `forge doctor` reads about service accounts (active ones only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    /// Active service accounts holding active tokens that all expire within
    /// the warning window (they lose access unless a token is rotated).
    pub expiring: i64,
    /// The first of them (at most 10), by id.
    pub expiring_ids: Vec<UserId>,
    /// The soonest of those expiries (RFC 3339, UTC).
    pub soonest_expiry: Option<String>,
    /// Active service accounts without an active human owner (the owner
    /// was disabled, or the account has no profile).
    pub ownerless: i64,
    /// The first of them (at most 10), by id.
    pub ownerless_ids: Vec<UserId>,
}

impl Health {
    /// `service_accounts.tokens_expiring`: `FG-AUTH-0019` (warn) with
    /// `expiring_service_accounts`, `user_ids` and `soonest_expiry`.
    pub fn check_expiring(&self) -> Result<(), ForgeError> {
        if self.expiring == 0 {
            return Ok(());
        }
        Err(ForgeError::new(ErrorCode::ServiceAccountTokenExpiring)
            .with_context("reason", "tokens_expiring")
            .with_context("expiring_service_accounts", self.expiring)
            .with_context("user_ids", ids(&self.expiring_ids))
            .with_context(
                "soonest_expiry",
                self.soonest_expiry.clone().unwrap_or_default(), // clone: the error owns it
            )
            .with_context("within_days", EXPIRY_WARNING.as_secs() / 86_400))
    }

    /// `service_accounts.ownerless`: `FG-AUTH-0010` (warn)
    /// `reason=ownerless_service_account` with `ownerless_service_accounts`
    /// and `user_ids`.
    pub fn check_ownerless(&self) -> Result<(), ForgeError> {
        if self.ownerless == 0 {
            return Ok(());
        }
        Err(ForgeError::new(ErrorCode::AdministrationOrphaned)
            .with_context("reason", "ownerless_service_account")
            .with_context("ownerless_service_accounts", self.ownerless)
            .with_context("user_ids", ids(&self.ownerless_ids)))
    }
}

fn ids(ids: &[UserId]) -> String {
    ids.iter()
        .map(|u| u.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// The service accounts whose last active token expires within `within`,
/// and those without an active human owner, in one statement on the read
/// pool; `None` before the S0 migration (the doctor skips).
pub async fn health(db: &Db, within: Duration) -> Result<Option<Health>, ForgeError> {
    const QUERY: &str = "service_accounts.health";
    // Whole seconds as a bigint interval (no float cast; S0 review).
    let within_secs = i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
    db.within(QUERY, async {
        let mut conn = db.acquire().await?;
        let migrated: bool = sqlx::query_scalar(
            "SELECT pg_catalog.to_regclass('forge.service_account_profiles') IS NOT NULL",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        if !migrated {
            return Ok(None);
        }
        type HealthRow = (i64, Vec<i64>, Option<String>, i64, Vec<i64>);
        let (expiring, expiring_ids, soonest, ownerless, ownerless_ids): HealthRow =
            sqlx::query_as(
                "WITH sa AS ( \
                   SELECT u.id FROM forge.users u \
                   WHERE u.kind = 'service_account' AND u.state = 'active'), \
                 last AS ( \
                   SELECT t.user_id, max(t.expires_at) AS expires_at FROM forge.tokens t \
                   JOIN sa ON sa.id = t.user_id \
                   WHERE t.revoked_at IS NULL AND t.expires_at > now() \
                   GROUP BY t.user_id), \
                 expiring AS ( \
                   SELECT user_id, expires_at FROM last \
                   WHERE expires_at <= now() + $1::bigint * interval '1 second'), \
                 ownerless AS ( \
                   SELECT sa.id FROM sa \
                   LEFT JOIN forge.service_account_profiles p ON p.user_id = sa.id \
                   LEFT JOIN forge.users o ON o.id = p.owner_user_id \
                   WHERE o.id IS NULL OR o.state <> 'active' OR o.kind <> 'human') \
                 SELECT (SELECT count(*) FROM expiring), \
                   COALESCE((SELECT array_agg(user_id ORDER BY user_id) FROM \
                     (SELECT user_id FROM expiring ORDER BY user_id LIMIT $2) e), '{}'), \
                   (SELECT to_char(min(expires_at) AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM expiring), \
                   (SELECT count(*) FROM ownerless), \
                   COALESCE((SELECT array_agg(id ORDER BY id) FROM \
                     (SELECT id FROM ownerless ORDER BY id LIMIT $2) o), '{}')",
            )
            .bind(within_secs)
            .bind(SHOWN_IDS)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(Health {
            expiring,
            expiring_ids: expiring_ids.into_iter().map(UserId::new).collect(),
            soonest_expiry: soonest,
            ownerless,
            ownerless_ids: ownerless_ids.into_iter().map(UserId::new).collect(),
        }))
    })
    .await
}
