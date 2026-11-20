//! The snapshot of a person's organisation claims (ADR-0021 amendment
//! 2026-11-20, 「組織の claim の写し」): `forge.user_org_profiles`, one row per
//! user, written by forge-oidc at the sign-in and at every verified refresh
//! check of `auth.oidc.claims_source = access_token` ([`record`]). Display
//! only: nothing is authorized by it (Phase 2 is deferred).
//!
//! - Valid claims equal to the stored ones (same digest and issuer) move
//!   `confirmed_at` only (no audit row, `Settle::CommitDerived`).
//! - Changed valid claims (or the first snapshot) write the values and one
//!   `user.org_profile_changed` audit row on the user (before / after of the
//!   changed fields; the values are cut to the digests when the row would
//!   pass [`MAX_AUDIT_VALUES_BYTES`]).
//! - Invalid claims keep the last valid values and set `invalid_since`
//!   (unaudited); the caller logs `FG-AUTH-0028` once, when
//!   [`Recorded::Invalid`] says `newly`. With no row yet an empty snapshot
//!   is written (audited) with `invalid_since` set.
//!
//! The reads: [`read`] (one person's whole snapshot, the write pool: the
//! read pool never sees positions, the rank or the plants), [`primary_codes`]
//! (the primary department code of several users, the read pool: the
//! people pickers) and [`health`] (doctor).

use std::collections::HashMap;

use forge_core::ids::UserId;
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit resource type (tables.toml `audit_resource`).
const RESOURCE: &str = "user";
/// The audit action of a changed snapshot.
pub const CHANGED_ACTION: &str = "user.org_profile_changed";
/// The most entries of one list (`org_units`, `positions`, `plants`): the
/// PFS registry's limit and the table's CHECK.
pub const MAX_ENTRIES: usize = 64;
/// Above this many bytes of before / after values the audit row keeps the
/// changed field names and the digests only (the audit details' limit is
/// 8 KiB).
pub const MAX_AUDIT_VALUES_BYTES: usize = 6 * 1024;

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

/// One department the person belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrgUnit {
    /// `^[A-Z0-9][A-Z0-9_-]{0,19}$` (the PFS registry's org code).
    pub code: String,
    /// The person's main department (at most one).
    pub primary: bool,
}

/// The organisation claims as validated (forge-oidc parses them; the
/// table's CHECKs are the backstop). The primary department comes first.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrgSnapshot {
    pub org_units: Vec<OrgUnit>,
    /// `^[a-z][a-z0-9_]{0,63}$` each.
    pub positions: Vec<String>,
    /// 0..=999.
    pub position_rank: Option<i32>,
    /// `^[A-Z0-9]{1,4}$` each.
    pub plants: Vec<String>,
    pub plant_code: Option<String>,
}

impl OrgSnapshot {
    /// SHA-256 (hex) of the canonical JSON of the snapshot (the fields in
    /// declaration order, the lists in their stored order).
    pub fn digest(&self) -> Result<String, ForgeError> {
        let bytes = serde_json::to_vec(self).map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "org_snapshot_unserializable")
                .with_context("detail", e)
        })?;
        Ok(Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    /// The primary department's code, if any.
    pub fn primary_code(&self) -> Option<&str> {
        self.org_units
            .iter()
            .find(|u| u.primary)
            .map(|u| u.code.as_str())
    }

    fn org_units_json(&self) -> Result<String, ForgeError> {
        serde_json::to_string(&self.org_units).map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "org_snapshot_unserializable")
                .with_context("detail", e)
        })
    }

    /// The fields of `self` that differ from `other`, by name.
    fn changed_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.org_units != other.org_units {
            out.push("org_units");
        }
        if self.positions != other.positions {
            out.push("positions");
        }
        if self.position_rank != other.position_rank {
            out.push("position_rank");
        }
        if self.plants != other.plants {
            out.push("plants");
        }
        if self.plant_code != other.plant_code {
            out.push("plant_code");
        }
        out
    }

    fn field_value(&self, field: &str) -> serde_json::Value {
        match field {
            "org_units" => serde_json::json!(self.org_units),
            "positions" => serde_json::json!(self.positions),
            "position_rank" => serde_json::json!(self.position_rank),
            "plants" => serde_json::json!(self.plants),
            "plant_code" => serde_json::json!(self.plant_code),
            _ => serde_json::Value::Null,
        }
    }
}

/// What the provider sent this time.
#[derive(Debug, Clone, Copy)]
pub enum Observed<'a> {
    /// Valid claims (missing claims are empty and valid).
    Valid(&'a OrgSnapshot),
    /// The claims failed validation (forge-oidc logs which and why).
    Invalid,
}

/// What [`record`] did.
#[must_use = "Invalid { newly: true } is the one time FG-AUTH-0028 is logged"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// The first snapshot of the user (one audit row).
    Created,
    /// The same values: `confirmed_at` moved (no audit row).
    Unchanged,
    /// Other values (one audit row).
    Changed,
    /// Invalid claims: the last valid values stay; `newly` when
    /// `invalid_since` was set by this call.
    Invalid { newly: bool },
}

/// One person's stored snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgProfile {
    pub snapshot: OrgSnapshot,
    /// RFC 3339, UTC.
    pub confirmed_at: String,
    pub changed_at: String,
    /// Since when the provider's claims fail validation (the values are the
    /// last valid ones).
    pub invalid_since: Option<String>,
}

/// doctor's view (`auth.oidc.org_claims`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgHealth {
    pub profiles: i64,
    pub invalid: i64,
    /// The oldest `confirmed_at` (RFC 3339, UTC) and its age in seconds.
    pub oldest_confirmed_at: Option<String>,
    pub oldest_age_secs: Option<i64>,
}

type StoredRow = (
    String,
    String,
    Vec<String>,
    Option<i32>,
    Vec<String>,
    Option<String>,
    String,
    bool,
);

fn snapshot_of(
    org_units: &str,
    positions: Vec<String>,
    position_rank: Option<i32>,
    plants: Vec<String>,
    plant_code: Option<String>,
) -> Result<OrgSnapshot, ForgeError> {
    let org_units: Vec<OrgUnit> = serde_json::from_str(org_units).map_err(|e| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_org_units_invalid")
            .with_context("detail", e)
    })?;
    Ok(OrgSnapshot {
        org_units,
        positions,
        position_rank,
        plants,
        plant_code,
    })
}

/// The audit row of a changed snapshot: the changed fields' before / after
/// (or the digests only above [`MAX_AUDIT_VALUES_BYTES`]).
fn changed_entry(
    user: UserId,
    issuer: &str,
    before: Option<&OrgSnapshot>,
    after: &OrgSnapshot,
    invalid: bool,
) -> Result<AuditEntry, ForgeError> {
    let empty = OrgSnapshot::default();
    let fields = after.changed_fields(before.unwrap_or(&empty));
    let pick = |s: &OrgSnapshot| {
        let mut map = serde_json::Map::new();
        for f in &fields {
            map.insert((*f).to_owned(), s.field_value(f));
        }
        serde_json::Value::Object(map)
    };
    let before_values = before.map_or(serde_json::Value::Null, pick);
    let after_values = pick(after);
    let size = before_values.to_string().len() + after_values.to_string().len();
    let mut details = serde_json::json!({
        "user_id": user.get(),
        "issuer": issuer,
        "fields": fields,
        "digest_before": before.map(OrgSnapshot::digest).transpose()?,
        "digest_after": after.digest()?,
        "invalid": invalid,
    });
    if let Some(map) = details.as_object_mut() {
        if size <= MAX_AUDIT_VALUES_BYTES {
            map.insert("before".to_owned(), before_values);
            map.insert("after".to_owned(), after_values);
        } else {
            map.insert("values_omitted".to_owned(), serde_json::Value::Bool(true));
        }
    }
    Ok(AuditEntry {
        action: CHANGED_ACTION,
        outcome: Outcome::Succeeded,
        resource_type: RESOURCE,
        resource_id: Some(user.get()),
        project_id: None,
        error_code: None,
        details,
    })
}

fn record_of(entry: AuditEntry) -> ChangeRecord {
    ChangeRecord {
        audit: entry,
        affected: Vec::new(),
        events: Vec::new(),
    }
}

async fn write_values(
    tx: &mut ChangeTx,
    user: UserId,
    issuer: &str,
    snapshot: &OrgSnapshot,
    invalid: bool,
    insert: bool,
) -> Result<(), ForgeError> {
    let org_units = snapshot.org_units_json()?;
    let digest = snapshot.digest()?;
    let sql = if insert {
        "INSERT INTO forge.user_org_profiles (user_id, issuer, org_units, positions, \
         position_rank, plants, plant_code, digest, confirmed_at, changed_at, invalid_since) \
         VALUES ($1, $2, $3::jsonb, $4, $5, $6, $7, $8, now(), now(), \
         CASE WHEN $9 THEN now() END)"
    } else {
        "UPDATE forge.user_org_profiles SET issuer = $2, org_units = $3::jsonb, positions = $4, \
         position_rank = $5, plants = $6, plant_code = $7, digest = $8, confirmed_at = now(), \
         changed_at = now(), invalid_since = CASE WHEN $9 THEN now() END WHERE user_id = $1"
    };
    let (org_units, digest) = (&org_units, &digest);
    tx.step(|c| async move {
        sqlx::query(sql)
            .bind(user.get())
            .bind(issuer)
            .bind(org_units.as_str())
            .bind(&snapshot.positions)
            .bind(snapshot.position_rank)
            .bind(&snapshot.plants)
            .bind(snapshot.plant_code.as_deref())
            .bind(digest.as_str())
            .bind(invalid)
            .execute(c)
            .await
            .map(|_| ())
            .map_err(|e| query_error("org_profiles.record.write", e))
    })
    .await
}

/// Record what the provider sent for `user` (see the module documentation).
/// `ctx` is the sign-in's (the person) or the refresh check's (`system`).
/// The user's row is locked first so two sign-ins of one person write one
/// row. An unknown user is `FG-REPO-0005`.
pub async fn record(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    issuer: &str,
    observed: Observed<'_>,
) -> Result<Recorded, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = record_steps(&mut tx, user, issuer, observed).await;
    tx.settle(CHANGED_ACTION, outcome).await
}

async fn record_steps(
    tx: &mut ChangeTx,
    user: UserId,
    issuer: &str,
    observed: Observed<'_>,
) -> Result<Settle<Recorded>, ForgeError> {
    let known: Option<i64> = tx
        .step(|c| async move {
            sqlx::query_scalar("SELECT id FROM forge.users WHERE id = $1 FOR NO KEY UPDATE")
                .bind(user.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("org_profiles.record.user", e))
        })
        .await?;
    if known.is_none() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", RESOURCE)
            .with_context("reason", "absent"));
    }
    let stored: Option<StoredRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT issuer, org_units::text, positions, position_rank, plants, plant_code, \
                 digest, invalid_since IS NOT NULL \
                 FROM forge.user_org_profiles WHERE user_id = $1 FOR UPDATE",
            )
            .bind(user.get())
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("org_profiles.record.read", e))
        })
        .await?;
    let Some((
        stored_issuer,
        org_units,
        positions,
        position_rank,
        plants,
        plant_code,
        digest,
        was_invalid,
    )) = stored
    else {
        let (snapshot, invalid) = match observed {
            Observed::Valid(s) => (s.clone(), false), // clone: the empty case owns its own
            Observed::Invalid => (OrgSnapshot::default(), true),
        };
        write_values(tx, user, issuer, &snapshot, invalid, true).await?;
        let entry = changed_entry(user, issuer, None, &snapshot, invalid)?;
        let done = if invalid {
            Recorded::Invalid { newly: true }
        } else {
            Recorded::Created
        };
        return Ok(Settle::Commit(record_of(entry), done));
    };
    let Observed::Valid(snapshot) = observed else {
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.user_org_profiles SET invalid_since = now() \
                 WHERE user_id = $1 AND invalid_since IS NULL",
            )
            .bind(user.get())
            .execute(c)
            .await
            .map(|_| ())
            .map_err(|e| query_error("org_profiles.record.invalid", e))
        })
        .await?;
        return Ok(Settle::CommitDerived(Recorded::Invalid {
            newly: !was_invalid,
        }));
    };
    if digest == snapshot.digest()? && stored_issuer == issuer {
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.user_org_profiles SET confirmed_at = now(), invalid_since = NULL \
                 WHERE user_id = $1",
            )
            .bind(user.get())
            .execute(c)
            .await
            .map(|_| ())
            .map_err(|e| query_error("org_profiles.record.confirm", e))
        })
        .await?;
        return Ok(Settle::CommitDerived(Recorded::Unchanged));
    }
    let before = snapshot_of(&org_units, positions, position_rank, plants, plant_code)?;
    write_values(tx, user, issuer, snapshot, false, false).await?;
    let entry = changed_entry(user, issuer, Some(&before), snapshot, false)?;
    Ok(Settle::Commit(record_of(entry), Recorded::Changed))
}

/// `user`'s stored snapshot (`None`: never recorded). The write pool (the
/// read pool never sees positions, the rank or the plants).
pub async fn read(db: &Db, user: UserId) -> Result<Option<OrgProfile>, ForgeError> {
    type Row = (
        String,
        Vec<String>,
        Option<i32>,
        Vec<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    );
    let row: Option<Row> = db
        .within("org_profiles.read", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_profiles.read.acquire", e))?;
            sqlx::query_as(concat!(
                "SELECT org_units::text, positions, position_rank, plants, plant_code, ",
                utc!("confirmed_at"),
                ", ",
                utc!("changed_at"),
                ", ",
                utc!("invalid_since"),
                " FROM forge.user_org_profiles WHERE user_id = $1"
            ))
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("org_profiles.read", e))
        })
        .await?;
    row.map(
        |(org_units, positions, rank, plants, plant_code, confirmed, changed, invalid)| {
            Ok(OrgProfile {
                snapshot: snapshot_of(&org_units, positions, rank, plants, plant_code)?,
                confirmed_at: confirmed,
                changed_at: changed,
                invalid_since: invalid,
            })
        },
    )
    .transpose()
}

/// The primary department code of each of `users` that has one (the read
/// pool; the people pickers and the approval list). At most 1000 ids
/// (`FG-SYS-0001 reason=too_many_users` above: a caller bug).
pub async fn primary_codes(
    db: &Db,
    users: &[UserId],
) -> Result<HashMap<UserId, String>, ForgeError> {
    if users.is_empty() {
        return Ok(HashMap::new());
    }
    if users.len() > 1000 {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_users")
            .with_context("count", users.len()));
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let rows: Vec<(i64, String)> = db
        .within("org_profiles.primary_codes", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT p.user_id, u ->> 'code' FROM forge.user_org_profiles p \
                 CROSS JOIN LATERAL jsonb_array_elements(p.org_units) u \
                 WHERE p.user_id = ANY($1) AND u ->> 'primary' = 'true'",
            )
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("org_profiles.primary_codes", e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, code)| (UserId::new(id), code))
        .collect())
}

/// doctor's counts (the read pool).
pub async fn health(db: &Db) -> Result<OrgHealth, ForgeError> {
    let row: (i64, i64, Option<String>, Option<i64>) = db
        .within("org_profiles.health", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(concat!(
                "SELECT count(user_id), count(user_id) FILTER (WHERE invalid_since IS NOT NULL), ",
                utc!("min(confirmed_at)"),
                ", extract(epoch FROM now() - min(confirmed_at))::bigint \
                 FROM forge.user_org_profiles"
            ))
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("org_profiles.health", e))
        })
        .await?;
    Ok(OrgHealth {
        profiles: row.0,
        invalid: row.1,
        oldest_confirmed_at: row.2,
        oldest_age_secs: row.3,
    })
}

#[cfg(test)]
mod tests;
