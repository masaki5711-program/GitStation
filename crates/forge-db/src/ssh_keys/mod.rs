//! People's SSH public keys (M4 C13, ADR-0033 §2): add, list, delete and
//! authenticate. `add` / `delete` each write one audit row on the owner
//! (resource `user`; `ssh_key.added` / `ssh_key.deleted`, audit only, no
//! event). A key is public data: the read pool sees every column; the
//! lookup by fingerprint of the SSH server runs on the write pool (it
//! records `last_used_at`).

use forge_core::ids::UserId;
use forge_core::ssh_key::{is_fingerprint, SshPublicKey};
use forge_core::{AuditDetails, RequestId, SshKeyIdentity, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::PgConnection;

use crate::change::{ChangeContext, ChangeRecord, ChangeTx, Settle};
use crate::{query_error, read_query_error, Db};

/// The most keys one user may hold.
pub const MAX_KEYS_PER_USER: i64 = 50;
/// The longest title, in characters.
const MAX_TITLE_CHARS: usize = 100;
/// The audit resource type (tables.toml `audit_resource`): the owner.
const RESOURCE: &str = "user";
/// The unique constraint on the fingerprint.
const FINGERPRINT_KEY: &str = "user_ssh_keys_fingerprint_key";

/// An RFC 3339 UTC rendering of a timestamptz column, computed by the server.
macro_rules! utc {
    ($col:literal) => {
        concat!(
            "to_char(",
            $col,
            " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    };
}

/// The request to add a key.
#[derive(Debug, Clone)]
pub struct NewSshKey {
    pub user: UserId,
    /// 1..=100 characters, no control or invisible format character; empty
    /// = the key's comment (or the algorithm when it has none).
    pub title: String,
    pub key: SshPublicKey,
}

/// One key as the owner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshKeyInfo {
    pub key_id: i64,
    pub title: String,
    pub algorithm: String,
    /// `SHA256:…`.
    pub fingerprint: String,
    /// `<algorithm> <base64>`.
    pub public_key: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub last_used_at: Option<String>,
}

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SshKeyInvalid).with_context("reason", reason)
}

/// Invisible format characters that would disguise a title (as token names).
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

/// The title to store: the given one trimmed, else the key's comment, else
/// the algorithm. `FG-AUTH-0023 reason=title_invalid` (`rule`).
fn title_of(new: &NewSshKey) -> Result<String, ForgeError> {
    let given = new.title.trim();
    let title = if !given.is_empty() {
        given.to_owned()
    } else if !new.key.comment.is_empty() {
        new.key.comment.clone() // clone: the key stays with the request
    } else {
        new.key.algorithm.as_str().to_owned()
    };
    let len = title.chars().count();
    if len > MAX_TITLE_CHARS {
        return Err(invalid("title_invalid")
            .with_context("rule", "length")
            .with_context("len", len));
    }
    if title
        .chars()
        .any(|c| c.is_control() || is_invisible_format(c))
    {
        return Err(invalid("title_invalid").with_context("rule", "control_character"));
    }
    Ok(title)
}

/// Whether `e` is SQLSTATE 23505 on `constraint`.
fn is_unique_violation(e: &sqlx::Error, constraint: &str) -> bool {
    e.as_database_error()
        .is_some_and(|d| d.code().as_deref() == Some("23505") && d.constraint() == Some(constraint))
}

/// Add a key for `new.user`: one `ssh_key.added` audit row. The owner's
/// row is locked (`FOR UPDATE`) so concurrent adds keep the per-user
/// limit. Errors (nothing written): `FG-AUTH-0023` with `reason` =
/// `title_invalid`, `owner_disabled`, `limit`, `duplicate` (the
/// fingerprint is registered to anyone); `FG-REPO-0005` for an unknown
/// owner.
pub async fn add(db: &Db, ctx: &ChangeContext, new: &NewSshKey) -> Result<SshKeyInfo, ForgeError> {
    let user = new.user;
    let fingerprint = new.key.fingerprint();
    let refuse = |reason: &'static str| {
        invalid(reason)
            .with_context("user", user.get())
            .with_context("fingerprint", &fingerprint)
            .with_context("algorithm", new.key.algorithm.as_str())
    };
    let title = title_of(new).map_err(|e| e.with_context("user", user.get()))?;
    let public_key = new.key.openssh();
    let algorithm = new.key.algorithm.as_str();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let owner: Option<(String, String)> = tx
            .step(|c| async move {
                sqlx::query_as("SELECT kind, state FROM forge.users WHERE id = $1 FOR UPDATE")
                    .bind(user.get())
                    .fetch_optional(c)
                    .await
                    .map_err(|e| query_error("ssh_keys.add.owner", e))
            })
            .await?;
        let Some((kind, state)) = owner else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "user")
                .with_context("reason", "absent"));
        };
        if kind == UserKind::Bot.as_str() {
            return Err(refuse("owner_bot"));
        }
        if state != "active" {
            return Err(refuse("owner_disabled"));
        }
        let held: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar("SELECT count(*) FROM forge.user_ssh_keys WHERE user_id = $1")
                    .bind(user.get())
                    .fetch_one(c)
                    .await
                    .map_err(|e| query_error("ssh_keys.add.count", e))
            })
            .await?;
        if held >= MAX_KEYS_PER_USER {
            return Err(refuse("limit").with_context("limit", MAX_KEYS_PER_USER));
        }
        let (fp, title_ref, public_ref) = (&fingerprint, &title, &public_key);
        let inserted: Result<(i64, String), ForgeError> = tx
            .step(|c| async move {
                sqlx::query_as(concat!(
                    "INSERT INTO forge.user_ssh_keys (user_id, title, algorithm, public_key, \
                     fingerprint_sha256) VALUES ($1, $2, $3, $4, $5) RETURNING id, ",
                    utc!("created_at")
                ))
                .bind(user.get())
                .bind(title_ref.as_str())
                .bind(algorithm)
                .bind(public_ref.as_str())
                .bind(fp.as_str())
                .fetch_one(c)
                .await
                .map_err(|e| {
                    if is_unique_violation(&e, FINGERPRINT_KEY) {
                        invalid("duplicate")
                    } else {
                        query_error("ssh_keys.add.insert", e)
                    }
                })
            })
            .await;
        let (key_id, created_at) = inserted.map_err(|e| {
            if e.code() == ErrorCode::SshKeyInvalid {
                refuse("duplicate")
            } else {
                e
            }
        })?;
        let details = AuditDetails::SshKeyAdded {
            key_id,
            user,
            algorithm: algorithm.to_owned(),
            fingerprint: fingerprint.clone(), // clone: the answer keeps it
        };
        let record = ChangeRecord::audit_only(&details, RESOURCE, user.get());
        Ok(Settle::Commit(
            record,
            SshKeyInfo {
                key_id,
                title: title.clone(), // clone: borrowed by the insert above
                algorithm: algorithm.to_owned(),
                fingerprint: fingerprint.clone(), // clone: borrowed by refuse
                public_key: public_key.clone(),   // clone: borrowed by the insert
                created_at,
                last_used_at: None,
            },
        ))
    }
    .await;
    tx.settle("ssh_key.added", outcome).await
}

/// What `delete` did.
#[must_use = "Absent wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// One `ssh_key.deleted` audit row.
    Deleted,
    /// No such key (deleted meanwhile): nothing written.
    Absent,
}

/// Delete key `key_id`: one `ssh_key.deleted` audit row. Does not check
/// who owns it: an HTTP route decides with `authorize(SshKeyDelete)` on the
/// owner [`owner`] read first and refuses another user's key as not found.
pub async fn delete(
    db: &Db,
    ctx: &ChangeContext,
    key_id: i64,
) -> Result<DeleteOutcome, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let row: Option<(i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "DELETE FROM forge.user_ssh_keys WHERE id = $1 \
                     RETURNING user_id, fingerprint_sha256",
                )
                .bind(key_id)
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("ssh_keys.delete", e))
            })
            .await?;
        let Some((user_id, fingerprint)) = row else {
            return Ok(Settle::Discard(DeleteOutcome::Absent));
        };
        let details = AuditDetails::SshKeyDeleted {
            key_id,
            user: UserId::new(user_id),
            fingerprint,
        };
        Ok(Settle::Commit(
            ChangeRecord::audit_only(&details, RESOURCE, user_id),
            DeleteOutcome::Deleted,
        ))
    }
    .await;
    tx.settle("ssh_key.deleted", outcome).await
}

type ListRow = (i64, String, String, String, String, String, Option<String>);

/// The owner's keys, newest first (read pool).
pub async fn list(db: &Db, user: UserId) -> Result<Vec<SshKeyInfo>, ForgeError> {
    let rows: Vec<ListRow> = db
        .within("ssh_keys.list", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(concat!(
                "SELECT id, title, algorithm, fingerprint_sha256, public_key, ",
                utc!("created_at"),
                ", ",
                utc!("last_used_at"),
                " FROM forge.user_ssh_keys WHERE user_id = $1 ORDER BY created_at DESC, id DESC"
            ))
            .bind(user.get())
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("ssh_keys.list", e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(key_id, title, algorithm, fingerprint, public_key, created_at, last_used_at)| {
                SshKeyInfo {
                    key_id,
                    title,
                    algorithm,
                    fingerprint,
                    public_key,
                    created_at,
                    last_used_at,
                }
            },
        )
        .collect())
}

/// The owner of key `key_id` (`None`: no such key). Read pool; the loader
/// of `DELETE /api/v1/user/ssh-keys/{id}` decides on it.
pub async fn owner(db: &Db, key_id: i64) -> Result<Option<UserId>, ForgeError> {
    let owner: Option<i64> = db
        .within("ssh_keys.owner", async {
            let mut conn = db.acquire().await?;
            sqlx::query_scalar("SELECT user_id FROM forge.user_ssh_keys WHERE id = $1")
                .bind(key_id)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| read_query_error("ssh_keys.owner", e))
        })
        .await?;
    Ok(owner.map(UserId::new))
}

#[derive(sqlx::FromRow)]
struct AuthRow {
    id: i64,
    user_id: i64,
    active: bool,
    stale: bool,
    username: String,
    kind: String,
}

/// Resolve a key by its fingerprint (write pool). `FG-AUTH-0001
/// reason=ssh_key_unknown` (also for a string that is no fingerprint),
/// `FG-AUTH-0004 reason=owner_disabled`, `FG-DB-0001`, `FG-DB-0019` (a
/// stored kind this build cannot read). With `record_use`, `last_used_at`
/// is refreshed at most every 10 minutes, unaudited; a failure there is
/// logged once (warn) and authentication proceeds.
pub async fn authenticate(
    db: &Db,
    request_id: &RequestId,
    fingerprint: &str,
    record_use: bool,
) -> Result<SshKeyIdentity, ForgeError> {
    if !is_fingerprint(fingerprint) {
        return Err(
            ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "ssh_key_unknown")
        );
    }
    db.within("ssh_keys.authenticate", async {
        let mut conn = db
            .write_pool()
            .acquire()
            .await
            .map_err(|e| query_error("ssh_keys.authenticate.acquire", e))?;
        let row: Option<AuthRow> = sqlx::query_as(
            "SELECT k.id, k.user_id, u.state = 'active' AS active, \
             (k.last_used_at IS NULL OR k.last_used_at < now() - interval '10 minutes') AS stale, \
             u.username, u.kind \
             FROM forge.user_ssh_keys k JOIN forge.users u ON u.id = k.user_id \
             WHERE k.fingerprint_sha256 = $1",
        )
        .bind(fingerprint)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| query_error("ssh_keys.authenticate.lookup", e))?;
        let Some(row) = row else {
            return Err(ForgeError::new(ErrorCode::Unauthenticated)
                .with_context("reason", "ssh_key_unknown")
                .with_context("fingerprint", fingerprint));
        };
        if !row.active {
            return Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")
                .with_context("user", row.user_id)
                .with_context("ssh_key_id", row.id));
        }
        let kind = crate::users::parse_kind(row.user_id, &row.kind)?;
        if record_use && row.stale {
            touch(&mut conn, request_id, row.id).await;
        }
        Ok(SshKeyIdentity {
            key_id: row.id,
            user_id: UserId::new(row.user_id),
            username: row.username,
            kind,
        })
    })
    .await
}

/// Record the use (see [`authenticate`]).
async fn touch(conn: &mut PgConnection, request_id: &RequestId, key_id: i64) {
    let result = sqlx::query(
        "UPDATE forge.user_ssh_keys SET last_used_at = now() WHERE id = $1 \
         AND (last_used_at IS NULL OR last_used_at < now() - interval '10 minutes')",
    )
    .bind(key_id)
    .execute(conn)
    .await;
    if let Err(e) = result {
        let e = query_error("ssh_keys.authenticate.touch", e);
        tracing::warn!(
            target: "forge_db",
            request_id = %request_id,
            error_code = e.code().id(),
            ssh_key_id = key_id,
            error = %e,
            "recording the SSH key's last use failed; authentication proceeds"
        );
    }
}

#[cfg(test)]
mod tests;
