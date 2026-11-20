//! [`replace`]: one sync of the copy (ADR-0038 §1).
//!
//! Before anything is written the new lists are checked as a whole:
//! every `parent_code` names a unit of the same plant at a strictly higher
//! level (`missing_parent` / `level_order`), and — unless
//! `org.allow_large_shrink` — the copy may not shrink sharply (more than
//! half of the units removed, a plant's units or positions emptied, or no
//! positions at all). Each refusal is `FG-INT-0027` with the counts; the
//! previous copy stays.
//!
//! One sync at a time: a transaction-scoped advisory lock taken with
//! `pg_try_advisory_xact_lock`; a second worker finds it held and skips
//! ([`Replaced::Busy`]). Rows are written only when they differ (`IS
//! DISTINCT FROM`); the sync's time goes to the one-row
//! `forge.org_master_state`, which `derive`, `health` and doctor read.

use std::collections::{HashMap, HashSet};

use forge_errors::{ErrorCode, ForgeError};

use super::{Counts, MasterPosition, MasterUnit, Synced, UnitLevel};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, Db};

/// The audit action of a sync that changed the copy.
pub const SYNCED_ACTION: &str = "org.master_synced";
/// The audit resource type of the copy.
const RESOURCE: &str = "org_master";
/// The longest wait for a row lock in [`replace`].
const REPLACE_LOCK_WAIT_MS: u64 = 3_000;
/// The advisory lock key's text (`hashtextextended(…, 0)`).
const LOCK_KEY: &str = "org_master_sync";

/// What a sync may do besides the normal (`org.allow_large_shrink`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplacePolicy {
    /// Let a sync remove more than half of the units, empty a plant or
    /// leave no positions (an operator's deliberate opt-in).
    pub allow_large_shrink: bool,
}

/// What [`replace`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replaced {
    Synced(Synced),
    /// Another sync holds the lock: nothing was read or written.
    Busy,
}

type UnitRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i32,
);

type PositionRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i32,
    i32,
    bool,
    i32,
);

fn unit_row(u: &MasterUnit) -> UnitRow {
    (
        u.plant_code.clone(), // clone: compared as an owned row
        u.code.clone(),
        u.level.as_str().to_owned(),
        u.parent_code.clone(),
        u.name.ja.clone(),
        u.name.en.clone(),
        u.name.id.clone(),
        u.cost_center.clone(),
        u.head_staff_id.clone(),
        u.valid_from.clone(),
        u.version,
    )
}

fn position_row(p: &MasterPosition) -> PositionRow {
    (
        p.plant_code.clone(), // clone: compared as an owned row
        p.code.clone(),
        p.name.ja.clone(),
        p.name.en.clone(),
        p.name.id.clone(),
        p.rank,
        p.sort_order,
        p.active,
        p.version,
    )
}

/// Added / changed / removed between the stored rows and the new ones,
/// keyed by (plant, code) — the first two fields of each row.
fn diff<R: PartialEq>(stored: &[R], new: &[R], key: impl Fn(&R) -> (String, String)) -> Counts {
    let old: HashMap<(String, String), &R> = stored.iter().map(|r| (key(r), r)).collect();
    let mut counts = Counts::default();
    let mut seen = HashSet::new();
    for row in new {
        let k = key(row);
        match old.get(&k) {
            None => counts.added += 1,
            Some(o) if *o != row => counts.changed += 1,
            Some(_) => {}
        }
        seen.insert(k);
    }
    let removed = old.keys().filter(|k| !seen.contains(*k)).count();
    counts.removed = u64::try_from(removed).unwrap_or(u64::MAX);
    counts.total = u64::try_from(new.len()).unwrap_or(u64::MAX);
    counts
}

fn column<T, R>(rows: &[R], f: impl Fn(&R) -> T) -> Vec<T> {
    rows.iter().map(f).collect()
}

/// `FG-INT-0027` for a refused sync (counts and codes only, never names).
fn refused(list: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::OrgMasterInvalid)
        .with_context("event", "forge.org.master_sync_failed")
        .with_context("list", list)
        .with_context("reason", reason)
}

/// A duplicate key in the lists is a caller bug (the parser refuses it).
fn refuse_duplicates<R>(
    rows: &[R],
    key: impl Fn(&R) -> (&str, &str),
    list: &'static str,
) -> Result<(), ForgeError> {
    let mut seen = HashSet::new();
    for r in rows {
        if !seen.insert(key(r)) {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "org_master_duplicate")
                .with_context("list", list));
        }
    }
    Ok(())
}

/// Every parent is a unit of the same plant at a strictly higher level
/// (so the hierarchy has no cycle either). Pure.
pub(crate) fn check_hierarchy(units: &[MasterUnit]) -> Result<(), ForgeError> {
    let levels: HashMap<(&str, &str), UnitLevel> = units
        .iter()
        .map(|u| ((u.plant_code.as_str(), u.code.as_str()), u.level))
        .collect();
    for (index, u) in units.iter().enumerate() {
        let Some(parent) = u.parent_code.as_deref() else {
            continue;
        };
        match levels.get(&(u.plant_code.as_str(), parent)) {
            None => {
                return Err(refused("units", "missing_parent")
                    .with_context("index", index)
                    .with_context("plant", u.plant_code.clone())); // clone: logged
            }
            Some(level) if *level <= u.level => {
                return Err(refused("units", "level_order")
                    .with_context("index", index)
                    .with_context("plant", u.plant_code.clone())); // clone: logged
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn per_plant<R>(rows: &[R], plant: impl Fn(&R) -> &str) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for r in rows {
        *out.entry(plant(r).to_owned()).or_default() += 1;
    }
    out
}

/// The shrink guard (pure): `Err` when the new lists would shrink the copy
/// sharply and the policy does not allow it.
pub(crate) fn check_shrink(
    (stored_units, new_units): (&[UnitRow], &[UnitRow]),
    (stored_positions, new_positions): (&[PositionRow], &[PositionRow]),
    policy: ReplacePolicy,
) -> Result<(), ForgeError> {
    if policy.allow_large_shrink {
        return Ok(());
    }
    let units = diff(stored_units, new_units, |r| (r.0.clone(), r.1.clone())); // clone: keys
    if units.removed > 0
        && units.removed.saturating_mul(2) > u64::try_from(stored_units.len()).unwrap_or(u64::MAX)
    {
        return Err(refused("units", "shrink")
            .with_context("before", stored_units.len())
            .with_context("after", new_units.len())
            .with_context("removed", units.removed));
    }
    let new_unit_plants = per_plant(new_units, |r| r.0.as_str());
    for (plant, before) in per_plant(stored_units, |r| r.0.as_str()) {
        if !new_unit_plants.contains_key(&plant) {
            return Err(refused("units", "plant_emptied")
                .with_context("plant", plant)
                .with_context("before", before));
        }
    }
    if new_positions.is_empty() {
        return Err(
            refused("positions", "positions_empty").with_context("before", stored_positions.len())
        );
    }
    let new_position_plants = per_plant(new_positions, |r| r.0.as_str());
    for (plant, before) in per_plant(stored_positions, |r| r.0.as_str()) {
        if !new_position_plants.contains_key(&plant) {
            return Err(refused("positions", "plant_emptied")
                .with_context("plant", plant)
                .with_context("before", before));
        }
    }
    Ok(())
}

/// Replace the copy with `units` and `positions` (see the module
/// documentation). `ctx` is the worker's (`Actor::System`). An empty unit
/// list is `FG-SYS-0001 reason=org_master_empty` (the parser refuses it
/// first); a broken hierarchy or a sharp shrink is `FG-INT-0027`.
pub async fn replace(
    db: &Db,
    ctx: &ChangeContext,
    units: &[MasterUnit],
    positions: &[MasterPosition],
    policy: ReplacePolicy,
) -> Result<Replaced, ForgeError> {
    if units.is_empty() {
        return Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "org_master_empty"));
    }
    refuse_duplicates(units, |u| (&u.plant_code, &u.code), "units")?;
    refuse_duplicates(positions, |p| (&p.plant_code, &p.code), "positions")?;
    check_hierarchy(units)?;
    let units: Vec<UnitRow> = units.iter().map(unit_row).collect();
    let positions: Vec<PositionRow> = positions.iter().map(position_row).collect();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = replace_steps(&mut tx, &units, &positions, policy).await;
    tx.settle(SYNCED_ACTION, outcome).await
}

async fn replace_steps(
    tx: &mut ChangeTx,
    units: &[UnitRow],
    positions: &[PositionRow],
    policy: ReplacePolicy,
) -> Result<Settle<Replaced>, ForgeError> {
    let locked: bool = tx
        .step(|c| async move {
            sqlx::query(&format!(
                "SET LOCAL lock_timeout = '{REPLACE_LOCK_WAIT_MS}ms'"
            ))
            .execute(&mut *c)
            .await
            .map_err(|e| query_error("org_master.replace.lock_timeout", e))?;
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(LOCK_KEY)
                .fetch_one(c)
                .await
                .map_err(|e| query_error("org_master.replace.lock", e))
        })
        .await?;
    if !locked {
        return Ok(Settle::Discard(Replaced::Busy));
    }
    let stored_units: Vec<UnitRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT plant_code, code, level, parent_code, name_ja, name_en, name_id, \
                 cost_center, head_staff_id, to_char(valid_from, 'YYYY-MM-DD'), version \
                 FROM forge.org_units_master",
            )
            .fetch_all(c)
            .await
            .map_err(|e| query_error("org_master.replace.read_units", e))
        })
        .await?;
    let stored_positions: Vec<PositionRow> = tx
        .step(|c| async move {
            sqlx::query_as(
                "SELECT plant_code, code, name_ja, name_en, name_id, rank, sort_order, active, \
                 version FROM forge.org_positions_master",
            )
            .fetch_all(c)
            .await
            .map_err(|e| query_error("org_master.replace.read_positions", e))
        })
        .await?;
    check_shrink(
        (&stored_units, units),
        (&stored_positions, positions),
        policy,
    )?;
    let synced = Synced {
        units: diff(&stored_units, units, |r| (r.0.clone(), r.1.clone())), // clone: keys
        positions: diff(&stored_positions, positions, |r| (r.0.clone(), r.1.clone())),
    };
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.org_units_master AS m (plant_code, code, level, parent_code, \
             name_ja, name_en, name_id, cost_center, head_staff_id, valid_from, version, \
             changed_at) \
             SELECT p, c, l, pc, nj, ne, ni, cc, h, vf::date, v, now() \
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
                         $7::text[], $8::text[], $9::text[], $10::text[], $11::int4[]) \
                  AS n(p, c, l, pc, nj, ne, ni, cc, h, vf, v) \
             ON CONFLICT (plant_code, code) DO UPDATE SET level = EXCLUDED.level, \
             parent_code = EXCLUDED.parent_code, name_ja = EXCLUDED.name_ja, \
             name_en = EXCLUDED.name_en, name_id = EXCLUDED.name_id, \
             cost_center = EXCLUDED.cost_center, head_staff_id = EXCLUDED.head_staff_id, \
             valid_from = EXCLUDED.valid_from, version = EXCLUDED.version, changed_at = now() \
             WHERE (m.level, m.parent_code, m.name_ja, m.name_en, m.name_id, m.cost_center, \
                    m.head_staff_id, m.valid_from, m.version) IS DISTINCT FROM \
                   (EXCLUDED.level, EXCLUDED.parent_code, EXCLUDED.name_ja, EXCLUDED.name_en, \
                    EXCLUDED.name_id, EXCLUDED.cost_center, EXCLUDED.head_staff_id, \
                    EXCLUDED.valid_from, EXCLUDED.version)",
        )
        .bind(column(units, |r| r.0.clone()))
        .bind(column(units, |r| r.1.clone()))
        .bind(column(units, |r| r.2.clone()))
        .bind(column(units, |r| r.3.clone()))
        .bind(column(units, |r| r.4.clone()))
        .bind(column(units, |r| r.5.clone()))
        .bind(column(units, |r| r.6.clone()))
        .bind(column(units, |r| r.7.clone()))
        .bind(column(units, |r| r.8.clone()))
        .bind(column(units, |r| r.9.clone()))
        .bind(column(units, |r| r.10))
        .execute(&mut *c)
        .await
        .map_err(|e| query_error("org_master.replace.upsert_units", e))?;
        sqlx::query(
            "DELETE FROM forge.org_units_master m WHERE NOT EXISTS (SELECT 1 FROM \
             unnest($1::text[], $2::text[]) AS n(p, c) WHERE n.p = m.plant_code AND n.c = m.code)",
        )
        .bind(column(units, |r| r.0.clone()))
        .bind(column(units, |r| r.1.clone()))
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| query_error("org_master.replace.delete_units", e))
    })
    .await?;
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.org_positions_master AS m (plant_code, code, name_ja, name_en, \
             name_id, rank, sort_order, active, version, changed_at) \
             SELECT p, c, nj, ne, ni, r, s, a, v, now() \
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], \
                         $6::int4[], $7::int4[], $8::bool[], $9::int4[]) \
                  AS n(p, c, nj, ne, ni, r, s, a, v) \
             ON CONFLICT (plant_code, code) DO UPDATE SET name_ja = EXCLUDED.name_ja, \
             name_en = EXCLUDED.name_en, name_id = EXCLUDED.name_id, rank = EXCLUDED.rank, \
             sort_order = EXCLUDED.sort_order, active = EXCLUDED.active, \
             version = EXCLUDED.version, changed_at = now() \
             WHERE (m.name_ja, m.name_en, m.name_id, m.rank, m.sort_order, m.active, m.version) \
                   IS DISTINCT FROM (EXCLUDED.name_ja, EXCLUDED.name_en, EXCLUDED.name_id, \
                   EXCLUDED.rank, EXCLUDED.sort_order, EXCLUDED.active, EXCLUDED.version)",
        )
        .bind(column(positions, |r| r.0.clone()))
        .bind(column(positions, |r| r.1.clone()))
        .bind(column(positions, |r| r.2.clone()))
        .bind(column(positions, |r| r.3.clone()))
        .bind(column(positions, |r| r.4.clone()))
        .bind(column(positions, |r| r.5))
        .bind(column(positions, |r| r.6))
        .bind(column(positions, |r| r.7))
        .bind(column(positions, |r| r.8))
        .execute(&mut *c)
        .await
        .map_err(|e| query_error("org_master.replace.upsert_positions", e))?;
        sqlx::query(
            "DELETE FROM forge.org_positions_master m WHERE NOT EXISTS (SELECT 1 FROM \
             unnest($1::text[], $2::text[]) AS n(p, c) WHERE n.p = m.plant_code AND n.c = m.code)",
        )
        .bind(column(positions, |r| r.0.clone()))
        .bind(column(positions, |r| r.1.clone()))
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| query_error("org_master.replace.delete_positions", e))
    })
    .await?;
    let (unit_total, position_total) = (
        i32::try_from(units.len()).unwrap_or(i32::MAX),
        i32::try_from(positions.len()).unwrap_or(i32::MAX),
    );
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.org_master_state (id, synced_at, units, positions) \
             VALUES (true, now(), $1, $2) ON CONFLICT (id) DO UPDATE \
             SET synced_at = now(), units = EXCLUDED.units, positions = EXCLUDED.positions",
        )
        .bind(unit_total)
        .bind(position_total)
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| query_error("org_master.replace.state", e))
    })
    .await?;
    if !synced.units.touched() && !synced.positions.touched() {
        return Ok(Settle::CommitDerived(Replaced::Synced(synced)));
    }
    let entry = AuditEntry {
        action: SYNCED_ACTION,
        outcome: Outcome::Succeeded,
        resource_type: RESOURCE,
        resource_id: None,
        project_id: None,
        error_code: None,
        // Counts only: never a name or a staff id.
        details: serde_json::json!({
            "units": synced.units,
            "positions": synced.positions,
            "allow_large_shrink": policy.allow_large_shrink,
        }),
    };
    Ok(Settle::Commit(
        ChangeRecord {
            audit: entry,
            affected: Vec::new(),
            events: Vec::new(),
        },
        Replaced::Synced(synced),
    ))
}
