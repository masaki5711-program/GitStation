//! The reads of the copy: [`health`] (doctor), [`unit_paths`] /
//! [`primary_paths`] (display) and [`derive`] (fail closed).
//!
//! The age gates (ADR-0038 §4) are the same for all three: the person's
//! snapshot must be valid and confirmed within `org.max_profile_age_hours`,
//! the copy synced (`forge.org_master_state`) within
//! `org.master_max_age_hours` — both from [`Db::org_ages`]. [`derive`]
//! answers [`Derived::Unknown`] when a gate fails; the display reads still
//! show the names, marked [`ShownPath::stale`] (「最終確認時点」, the last
//! known names).

use std::collections::HashMap;

use forge_core::ids::UserId;
use forge_errors::{ErrorCode, ForgeError};

use super::{
    chains, path_of, Derived, DerivedOrg, MasterHealth, MaxAges, OrgPath, UnknownReason, MAX_USERS,
};
use crate::{query_error, read_query_error, Db};

/// At most this many codes per [`unit_paths`] call (a snapshot's limit).
pub const MAX_UNIT_CODES: usize = 64;

/// A path as the screens show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownPath {
    pub path: OrgPath,
    /// An age or validity gate failed: the names are the last known ones
    /// (the snapshot is invalid or old, or the copy is old).
    pub stale: bool,
}

/// doctor's counts (the read pool): the rows now and the last sync.
pub async fn health(db: &Db) -> Result<MasterHealth, ForgeError> {
    let row: (i64, i64, Option<String>, Option<i64>) = db
        .within("org_master.health", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT (SELECT count(code) FROM forge.org_units_master), \
                 (SELECT count(code) FROM forge.org_positions_master), \
                 to_char(s.synced_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 extract(epoch FROM now() - s.synced_at)::bigint \
                 FROM (SELECT 1) one \
                 LEFT JOIN forge.org_master_state s ON s.id",
            )
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| read_query_error("org_master.health", e))
        })
        .await?;
    Ok(MasterHealth {
        units: row.0,
        positions: row.1,
        last_synced_at: row.2,
        age_secs: row.3,
    })
}

/// The SQL of the copy's age in seconds (`NULL`: never synced).
const MASTER_AGE: &str = "(SELECT extract(epoch FROM now() - synced_at)::bigint \
                          FROM forge.org_master_state WHERE id)";

fn older(age: i64, max: std::time::Duration) -> bool {
    u64::try_from(age).unwrap_or(0) > max.as_secs()
}

/// Whether the facts behind a path are past a gate (display).
fn stale(ages: MaxAges, invalid: bool, profile_age: i64, master_age: Option<i64>) -> bool {
    invalid
        || older(profile_age, ages.profile)
        || master_age.is_none_or(|age| older(age, ages.master))
}

/// The paths of `user`'s units that resolve (display: the self profile;
/// codes that do not resolve are left out, and so is everything without a
/// plant: the master is keyed by plant). The write pool for the snapshot
/// (its plant is not on the read pool). At most [`MAX_UNIT_CODES`] codes.
pub async fn unit_paths(db: &Db, user: UserId) -> Result<HashMap<String, ShownPath>, ForgeError> {
    type Row = (String, Option<String>, bool, i64, Option<i64>);
    let row: Option<Row> = db
        .within("org_master.unit_paths", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.unit_paths.acquire", e))?;
            sqlx::query_as(&format!(
                "SELECT org_units::text, plant_code, invalid_since IS NOT NULL, \
                 extract(epoch FROM now() - confirmed_at)::bigint, {MASTER_AGE} \
                 FROM forge.user_org_profiles WHERE user_id = $1"
            ))
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.unit_paths", e))
        })
        .await?;
    let Some((org_units, Some(plant), invalid, profile_age, master_age)) = row else {
        return Ok(HashMap::new());
    };
    let units = stored_units(&org_units)?;
    let starts: Vec<(String, String)> = units
        .into_iter()
        .take(MAX_UNIT_CODES)
        .map(|u| (plant.clone(), u.code)) // clone: one key per code
        .collect();
    let is_stale = stale(db.org_ages(), invalid, profile_age, master_age);
    let found = chains(db, &starts).await?;
    Ok(found
        .into_iter()
        .filter_map(|((_, code), chain)| {
            path_of(&chain).ok().map(|path| {
                (
                    code,
                    ShownPath {
                        path,
                        stale: is_stale,
                    },
                )
            })
        })
        .collect())
}

/// The primary unit's path of each of `users` whose snapshot names a
/// primary unit and a plant that resolve in the copy (display: the people
/// pickers and the approval list; at most [`MAX_USERS`]). The snapshot's
/// plant is read on the write pool (the read pool never sees the plants);
/// only the names and the stale mark leave this function.
pub async fn primary_paths(
    db: &Db,
    users: &[UserId],
) -> Result<HashMap<UserId, ShownPath>, ForgeError> {
    if users.is_empty() {
        return Ok(HashMap::new());
    }
    if users.len() > MAX_USERS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_users")
            .with_context("count", users.len()));
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    type Row = (i64, String, String, bool, i64, Option<i64>);
    let rows: Vec<Row> = db
        .within("org_master.primary_units", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.primary_units.acquire", e))?;
            sqlx::query_as(&format!(
                "SELECT p.user_id, p.plant_code, u ->> 'code', p.invalid_since IS NOT NULL, \
                 extract(epoch FROM now() - p.confirmed_at)::bigint, {MASTER_AGE} \
                 FROM forge.user_org_profiles p \
                 CROSS JOIN LATERAL jsonb_array_elements(p.org_units) u \
                 WHERE p.user_id = ANY($1) AND p.plant_code IS NOT NULL \
                   AND u ->> 'primary' = 'true'"
            ))
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.primary_units", e))
        })
        .await?;
    let starts: Vec<(String, String)> = rows.iter().map(|r| (r.1.clone(), r.2.clone())).collect(); // clone: keys
    let found = chains(db, &starts).await?;
    let ages = db.org_ages();
    let mut out = HashMap::new();
    for (user, plant, code, invalid, profile_age, master_age) in rows {
        if let Some(path) = found.get(&(plant, code)).and_then(|c| path_of(c).ok()) {
            out.insert(
                UserId::new(user),
                ShownPath {
                    path,
                    stale: stale(ages, invalid, profile_age, master_age),
                },
            );
        }
    }
    Ok(out)
}

fn stored_units(raw: &str) -> Result<Vec<crate::org_profiles::OrgUnit>, ForgeError> {
    // The reason only: the parser's text could quote stored values.
    serde_json::from_str(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_org_units_invalid")
    })
}

type ProfileSql = (
    String,
    Option<String>,
    Vec<String>,
    Option<i32>,
    String,
    bool,
    i64,
    Option<i64>,
);

/// `user`'s derived organisation, failing closed (ADR-0038 §4; see
/// [`UnknownReason`] for the order of the checks), with the ages of
/// [`Db::org_ages`]. The write pool for the snapshot (the plant and the
/// positions are not on the read pool).
pub async fn derive(db: &Db, user: UserId) -> Result<Derived, ForgeError> {
    let row: Option<ProfileSql> = db
        .within("org_master.derive", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.derive.acquire", e))?;
            sqlx::query_as(&format!(
                "SELECT p.org_units::text, p.plant_code, p.positions, p.position_rank, p.digest, \
                 p.invalid_since IS NOT NULL, \
                 extract(epoch FROM now() - p.confirmed_at)::bigint, {MASTER_AGE} \
                 FROM forge.user_org_profiles p WHERE p.user_id = $1"
            ))
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.derive", e))
        })
        .await?;
    let Some((org_units, plant, positions, rank, digest, invalid, profile_age, master_age)) = row
    else {
        return Ok(Derived::Unknown(UnknownReason::NoSnapshot));
    };
    let ages = db.org_ages();
    if invalid {
        return Ok(Derived::Unknown(UnknownReason::InvalidSnapshot));
    }
    if older(profile_age, ages.profile) {
        return Ok(Derived::Unknown(UnknownReason::StaleSnapshot));
    }
    let Some(master_age) = master_age else {
        return Ok(Derived::Unknown(UnknownReason::MasterMissing));
    };
    if older(master_age, ages.master) {
        return Ok(Derived::Unknown(UnknownReason::MasterStale));
    }
    let Some(primary) = stored_units(&org_units)?.into_iter().find(|u| u.primary) else {
        return Ok(Derived::Unknown(UnknownReason::NoPrimary));
    };
    let Some(plant) = plant else {
        return Ok(Derived::Unknown(UnknownReason::NoPlant));
    };
    let key = (plant, primary.code);
    let found = chains(db, std::slice::from_ref(&key)).await?;
    let chain = found.get(&key).map(Vec::as_slice).unwrap_or_default();
    Ok(match path_of(chain) {
        Ok(path) => Derived::Known(Box::new(DerivedOrg {
            plant_code: key.0,
            primary: path,
            position_rank: rank,
            positions,
            digest,
        })),
        Err(reason) => Derived::Unknown(reason),
    })
}
