//! People's organisations for approvals (ADR-0038 §3, step C): the same
//! gates as [`super::derive`] (a valid snapshot confirmed within
//! `org.max_profile_age_hours`, a master synced within
//! `org.master_max_age_hours`, a primary unit and a plant that resolve),
//! answered as [`PersonOrg`] with the **whole** chain above the primary
//! unit (a selector on a division needs more than the section and the
//! department).
//!
//! - [`person_orgs`]: named people (the requester, an approver at approval
//!   time). Every person asked for is in the answer, known or not.
//! - [`plant_people`]: everyone of one plant whose organisation is known
//!   (the candidates of a selector: who can approve, and whether
//!   `escalate: parent` applies). More than [`MAX_PLANT_PEOPLE`] is
//!   `None` — "could not tell", which never escalates.
//! - [`unit_chain`]: one unit's chain in the master (`relation = unit`).
//!
//! The write pool: the plants, positions and ranks are not on the read
//! pool (ADR-0038 §6).

use std::collections::HashMap;

use forge_core::approver::{ChainUnit, OrgLevel, OrgSnapshot, PersonOrg};
use forge_core::ids::UserId;
use forge_errors::{ErrorCode, ForgeError};

use super::read::{older, stored_units, MASTER_AGE};
use super::{chains, path_of, ChainRow, UnknownReason, MAX_USERS};
use crate::{query_error, Db};

/// A person whose snapshot passed the gates, waiting for the chain read:
/// (user, units, plant, positions, rank, digest).
type Pending = (UserId, String, String, Vec<String>, Option<i32>, String);

/// The most people of one plant read for a selector's candidates.
pub const MAX_PLANT_PEOPLE: usize = 2000;

type ProfileRow = (
    i64,
    String,
    Option<String>,
    Vec<String>,
    Option<i32>,
    String,
    bool,
    i64,
);

const PROFILE_COLUMNS: &str = "p.user_id, p.org_units::text, p.plant_code, p.positions, \
     p.position_rank, p.digest, p.invalid_since IS NOT NULL, \
     extract(epoch FROM now() - p.confirmed_at)::bigint";

/// The chain of a valid path (the unit first), every unit at a strictly
/// higher level than the one below and linked by `parent_code`; the walk
/// stops where that no longer holds (fewer units above: never more people
/// in a unit than the master says).
fn full_chain(rows: &[ChainRow]) -> Result<Vec<ChainUnit>, UnknownReason> {
    path_of(rows)?;
    let mut out: Vec<ChainUnit> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let Some(level) = OrgLevel::parse(&row.level) else {
            break;
        };
        if i > 0 {
            let prev = &rows[i - 1];
            let linked = prev.parent_code.as_deref() == Some(row.code.as_str());
            let above = out.last().is_some_and(|u| level > u.level);
            if !row.valid || !linked || !above {
                break;
            }
        }
        out.push(ChainUnit {
            code: row.code.clone(), // clone: the snapshot owns its codes
            level,
        });
        if row.parent_code.is_none() {
            break;
        }
    }
    Ok(out)
}

/// The organisations of `rows` (each already read with the master's age).
async fn orgs_of(
    db: &Db,
    rows: Vec<ProfileRow>,
    master_age: Option<i64>,
) -> Result<HashMap<UserId, PersonOrg>, ForgeError> {
    let ages = db.org_ages();
    let unknown = |r: UnknownReason| PersonOrg::Unknown(r.as_str().to_owned());
    let mut out = HashMap::with_capacity(rows.len());
    let mut pending: Vec<Pending> = Vec::new();
    for (user, units, plant, positions, rank, digest, invalid, profile_age) in rows {
        let user = UserId::new(user);
        let gate = if invalid {
            Some(UnknownReason::InvalidSnapshot)
        } else if older(profile_age, ages.profile) {
            Some(UnknownReason::StaleSnapshot)
        } else {
            match master_age {
                None => Some(UnknownReason::MasterMissing),
                Some(age) if older(age, ages.master) => Some(UnknownReason::MasterStale),
                Some(_) => None,
            }
        };
        if let Some(reason) = gate {
            out.insert(user, unknown(reason));
            continue;
        }
        let Some(primary) = stored_units(&units)?.into_iter().find(|u| u.primary) else {
            out.insert(user, unknown(UnknownReason::NoPrimary));
            continue;
        };
        let Some(plant) = plant else {
            out.insert(user, unknown(UnknownReason::NoPlant));
            continue;
        };
        pending.push((user, plant, primary.code, positions, rank, digest));
    }
    let starts: Vec<(String, String)> = pending
        .iter()
        .map(|p| (p.1.clone(), p.2.clone())) // clone: the chain keys
        .collect();
    let found = chains(db, &starts).await?;
    for (user, plant, code, positions, rank, digest) in pending {
        let rows = found
            .get(&(plant.clone(), code)) // clone: the key
            .map(Vec::as_slice)
            .unwrap_or_default();
        let org = match full_chain(rows) {
            Ok(chain) => PersonOrg::Known(OrgSnapshot {
                plant,
                chain,
                rank,
                positions,
                digest,
            }),
            Err(reason) => unknown(reason),
        };
        out.insert(user, org);
    }
    Ok(out)
}

async fn master_age(db: &Db) -> Result<Option<i64>, ForgeError> {
    db.within("org_master.people.master_age", async {
        let mut conn = db.acquire().await?;
        sqlx::query_scalar::<_, Option<i64>>(&format!("SELECT {MASTER_AGE}"))
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.people.master_age", e))
    })
    .await
}

/// The organisation of each of `users` (at most [`MAX_USERS`]): known, or
/// the §4 reason it is not (`no_snapshot` for a person never signed in
/// through PFS).
pub async fn person_orgs(
    db: &Db,
    users: &[UserId],
) -> Result<HashMap<UserId, PersonOrg>, ForgeError> {
    if users.is_empty() {
        return Ok(HashMap::new());
    }
    if users.len() > MAX_USERS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_users")
            .with_context("count", users.len()));
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let rows: Vec<ProfileRow> = db
        .within("org_master.person_orgs", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.person_orgs.acquire", e))?;
            sqlx::query_as(&format!(
                "SELECT {PROFILE_COLUMNS} FROM forge.user_org_profiles p WHERE p.user_id = ANY($1)"
            ))
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.person_orgs", e))
        })
        .await?;
    let age = master_age(db).await?;
    let mut out = orgs_of(db, rows, age).await?;
    for user in users {
        out.entry(*user)
            .or_insert_with(|| PersonOrg::Unknown(UnknownReason::NoSnapshot.as_str().to_owned()));
    }
    Ok(out)
}

/// Everyone of `plant` whose organisation is known now, or `None` when the
/// plant holds more than [`MAX_PLANT_PEOPLE`] snapshots (could not tell).
pub async fn plant_people(
    db: &Db,
    plant: &str,
) -> Result<Option<Vec<(UserId, OrgSnapshot)>>, ForgeError> {
    let cap = i64::try_from(MAX_PLANT_PEOPLE).unwrap_or(i64::MAX);
    let rows: Vec<ProfileRow> = db
        .within("org_master.plant_people", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.plant_people.acquire", e))?;
            sqlx::query_as(&format!(
                "SELECT {PROFILE_COLUMNS} FROM forge.user_org_profiles p \
                 WHERE p.plant_code = $1 ORDER BY p.user_id LIMIT $2"
            ))
            .bind(plant)
            .bind(cap + 1)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.plant_people", e))
        })
        .await?;
    if rows.len() > MAX_PLANT_PEOPLE {
        return Ok(None);
    }
    let age = master_age(db).await?;
    let mut people: Vec<(UserId, OrgSnapshot)> = orgs_of(db, rows, age)
        .await?
        .into_iter()
        .filter_map(|(u, org)| match org {
            PersonOrg::Known(s) => Some((u, s)),
            PersonOrg::Unknown(_) => None,
        })
        .collect();
    people.sort_by_key(|(u, _)| u.get());
    Ok(Some(people))
}

/// The chain of unit `code` of `plant` in the master (the unit first), or
/// `None` when the master does not hold it, it is not valid yet, or its
/// hierarchy is broken.
pub async fn unit_chain(
    db: &Db,
    plant: &str,
    code: &str,
) -> Result<Option<Vec<ChainUnit>>, ForgeError> {
    let key = (plant.to_owned(), code.to_owned());
    let found = chains(db, std::slice::from_ref(&key)).await?;
    let rows = found.get(&key).map(Vec::as_slice).unwrap_or_default();
    Ok(full_chain(rows).ok())
}

/// One unit found by [`search_units`] (names only: what the read pool sees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundUnit {
    pub plant_code: String,
    pub code: String,
    pub level: OrgLevel,
    pub name: super::LocalizedName,
}

/// The most units one search answers.
pub const MAX_UNIT_RESULTS: i64 = 50;

/// Units of the master's copy whose code starts with `term` or whose name
/// (ja / en / id) contains it, case-insensitive (the unit picker of the
/// approver selector, ADR-0038 §3.1). `term` is 1..=64 characters,
/// trimmed; anything else is `FG-WEB-0006 param=search`. The read pool.
pub async fn search_units(db: &Db, term: &str) -> Result<Vec<FoundUnit>, ForgeError> {
    const QUERY: &str = "org_master.search_units";
    let term = term.trim();
    let len = term.chars().count();
    if len == 0 || len > 64 || term.chars().any(char::is_control) {
        return Err(ForgeError::new(ErrorCode::RequestInvalid)
            .with_context("reason", "param_invalid")
            .with_context("param", "search")
            .with_context("rule", "length"));
    }
    let mut pattern = String::with_capacity(term.len());
    for c in term.to_lowercase().chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    type Row = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT plant_code, code, level, name_ja, name_en, name_id \
                 FROM forge.org_units_master \
                 WHERE (valid_from IS NULL OR valid_from <= current_date) \
                   AND (lower(code) LIKE $1 || '%' ESCAPE '\\' \
                     OR lower(name_ja) LIKE '%' || $1 || '%' ESCAPE '\\' \
                     OR lower(coalesce(name_en, '')) LIKE '%' || $1 || '%' ESCAPE '\\' \
                     OR lower(coalesce(name_id, '')) LIKE '%' || $1 || '%' ESCAPE '\\') \
                 ORDER BY (lower(code) = $1) DESC, level DESC, code LIMIT $2",
            )
            .bind(&pattern)
            .bind(MAX_UNIT_RESULTS)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| crate::read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(plant_code, code, level, ja, en, id)| {
            Ok(FoundUnit {
                plant_code,
                level: OrgLevel::parse(&level).ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "stored_unit_level_invalid")
                })?,
                code,
                name: super::LocalizedName { ja, en, id },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::org_master::LocalizedName;

    fn row(code: &str, level: &str, parent: Option<&str>) -> ChainRow {
        ChainRow {
            code: code.to_owned(),
            level: level.to_owned(),
            parent_code: parent.map(str::to_owned),
            name: LocalizedName {
                ja: code.to_owned(),
                en: None,
                id: None,
            },
            valid: true,
        }
    }

    #[test]
    fn full_chain_walks_to_the_top() {
        let rows = [
            row("T1", "team", Some("S1")),
            row("S1", "section", Some("D1")),
            row("D1", "department", Some("V1")),
            row("V1", "division", None),
        ];
        let chain = full_chain(&rows).expect("valid");
        let codes: Vec<&str> = chain.iter().map(|u| u.code.as_str()).collect();
        assert_eq!(codes, ["T1", "S1", "D1", "V1"]);
    }

    #[test]
    fn full_chain_stops_above_a_break_and_refuses_a_broken_path() {
        // A division under a division above the department: kept up to D1.
        let rows = [
            row("S1", "section", Some("D1")),
            row("D1", "department", Some("V1")),
            row("V1", "department", None),
        ];
        let codes: Vec<String> = full_chain(&rows)
            .expect("valid up to D1")
            .into_iter()
            .map(|u| u.code)
            .collect();
        assert_eq!(codes, ["S1", "D1"]);
        // A parent at the same level below the department: unknown.
        let broken = [row("S1", "section", Some("S2")), row("S2", "section", None)];
        assert_eq!(full_chain(&broken), Err(UnknownReason::BrokenHierarchy));
        assert_eq!(full_chain(&[]), Err(UnknownReason::NotInMaster));
    }
}
