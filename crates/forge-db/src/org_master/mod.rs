//! The copy of the PFS organisation master (ADR-0038 §1, step A):
//! `forge.org_units_master` and `forge.org_positions_master`, replaced by
//! the worker task `org_master_sync` ([`replace`]) and read to give the
//! codes of a person's org claims (`forge.user_org_profiles`) a level, a
//! parent and names.
//!
//! - [`replace`]: one transaction; upsert of every listed row (each row's
//!   `synced_at` becomes the sync's time), delete of the rows no longer
//!   listed, and one `org.master_synced` audit row (`Actor::System`) when
//!   anything was added, changed or removed. The caller validated the lists
//!   (forge-integ `org_master`); the table CHECKs are the backstop. Nothing
//!   is written when it fails: the previous copy stays.
//! - [`derive`]: a person's **primary** unit and the section and department
//!   above it, found by walking `parent_code` and reading `level` — never by
//!   parsing a code. Fail closed (ADR-0038 §4): a missing, invalid or stale
//!   snapshot, a missing or stale master, a primary code without a plant,
//!   a code not in the master (closed, or not yet valid) or a broken parent
//!   chain is [`Derived::Unknown`] with the reason.
//! - [`unit_paths`] / [`primary_paths`]: the names shown next to codes (the
//!   self profile, the people pickers, the approval list). Display only:
//!   they use the copy whatever its age and leave out what they cannot
//!   resolve (the caller then shows the code).
//! - [`health`]: doctor's `org.master`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use forge_core::ids::UserId;
use forge_errors::{ErrorCode, ForgeError};

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit action of a sync that changed the copy.
pub const SYNCED_ACTION: &str = "org.master_synced";
/// The audit resource type of the copy.
const RESOURCE: &str = "org_master";
/// The longest parent chain walked (division → department → section → team
/// is 4; a longer chain is a cycle or a broken master).
pub const MAX_DEPTH: i32 = 8;
/// At most this many users per [`primary_paths`] call (a page's worth).
pub const MAX_USERS: usize = 1000;
/// The longest wait for the tables' lock in [`replace`].
const REPLACE_LOCK_WAIT_MS: u64 = 3_000;

/// An org unit's level (the registry's `level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UnitLevel {
    Team,
    Section,
    Department,
    Division,
}

impl UnitLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Section => "section",
            Self::Department => "department",
            Self::Division => "division",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "team" => Some(Self::Team),
            "section" => Some(Self::Section),
            "department" => Some(Self::Department),
            "division" => Some(Self::Division),
            _ => None,
        }
    }
}

/// A name in the three UI languages (`ja` always; `en` / `id` when PFS
/// has them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalizedName {
    pub ja: String,
    pub en: Option<String>,
    pub id: Option<String>,
}

/// One org unit of the registry (validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterUnit {
    /// `^[A-Z0-9]{1,4}$`.
    pub plant_code: String,
    /// `^[A-Z0-9][A-Z0-9_-]{0,19}$`.
    pub code: String,
    pub level: UnitLevel,
    /// The parent unit in the same plant (never the unit itself).
    pub parent_code: Option<String>,
    pub name: LocalizedName,
    pub cost_center: Option<String>,
    /// The head's staff id (personal data: never on the read pool).
    pub head_staff_id: Option<String>,
    /// `YYYY-MM-DD`.
    pub valid_from: Option<String>,
    pub version: i32,
}

/// One position of the registry (validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterPosition {
    pub plant_code: String,
    /// `^[a-z][a-z0-9_]{0,63}$`.
    pub code: String,
    pub name: LocalizedName,
    /// 0..=999.
    pub rank: i32,
    pub sort_order: i32,
    pub active: bool,
    pub version: i32,
}

/// What a sync did to one table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub added: u64,
    pub changed: u64,
    pub removed: u64,
    /// Rows after the sync.
    pub total: u64,
}

impl Counts {
    pub fn touched(&self) -> bool {
        self.added + self.changed + self.removed > 0
    }
}

/// What [`replace`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Synced {
    pub units: Counts,
    pub positions: Counts,
}

/// One unit as the display and the derivation need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgRef {
    pub code: String,
    pub level: UnitLevel,
    pub name: LocalizedName,
}

/// A unit and the section and department above it (each `None` when the
/// chain has no unit of that level above or at the unit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgPath {
    pub unit: OrgRef,
    pub section: Option<OrgRef>,
    pub department: Option<OrgRef>,
}

/// Why a person's organisation could not be derived (ADR-0038 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// No snapshot (never signed in through PFS).
    NoSnapshot,
    /// The snapshot is marked invalid (`FG-AUTH-0028`).
    InvalidSnapshot,
    /// The snapshot was confirmed longer ago than `org.max_profile_age_hours`.
    StaleSnapshot,
    /// The master was never synced.
    MasterMissing,
    /// The master is older than `org.master_max_age_hours` (`FG-INT-0028`).
    MasterStale,
    /// The snapshot has no primary unit.
    NoPrimary,
    /// The snapshot has no plant (the master is keyed by plant).
    NoPlant,
    /// The primary code is not in the master (closed, or never there).
    NotInMaster,
    /// The unit's version starts later (`valid_from` in the future).
    NotYetValid,
    /// A parent the chain names is not in the master (or not yet valid).
    MissingParent,
    /// A parent at the same or a lower level, or a chain longer than
    /// [`MAX_DEPTH`] (a cycle).
    BrokenHierarchy,
}

impl UnknownReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSnapshot => "no_snapshot",
            Self::InvalidSnapshot => "invalid_snapshot",
            Self::StaleSnapshot => "stale_snapshot",
            Self::MasterMissing => "master_missing",
            Self::MasterStale => "master_stale",
            Self::NoPrimary => "no_primary",
            Self::NoPlant => "no_plant",
            Self::NotInMaster => "not_in_master",
            Self::NotYetValid => "not_yet_valid",
            Self::MissingParent => "missing_parent",
            Self::BrokenHierarchy => "broken_hierarchy",
        }
    }
}

/// A person's derived organisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedOrg {
    pub plant_code: String,
    pub primary: OrgPath,
    pub position_rank: Option<i32>,
    pub positions: Vec<String>,
    /// The snapshot's digest (ADR-0038 §3.1 keeps it with an approval).
    pub digest: String,
}

/// The result of [`derive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derived {
    Known(Box<DerivedOrg>),
    Unknown(UnknownReason),
}

/// How old the facts may be ([`derive`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxAges {
    /// `org.max_profile_age_hours`.
    pub profile: Duration,
    /// `org.master_max_age_hours`.
    pub master: Duration,
}

/// doctor's view of the copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MasterHealth {
    pub units: i64,
    pub positions: i64,
    /// The last successful sync (RFC 3339, UTC) and its age in seconds
    /// (`None`: never synced).
    pub last_synced_at: Option<String>,
    pub age_secs: Option<i64>,
}

// ------------------------------------------------------------------ replace

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
fn diff<R: PartialEq>(stored: Vec<R>, new: &[R], key: impl Fn(&R) -> (String, String)) -> Counts {
    let old: HashMap<(String, String), R> = stored.into_iter().map(|r| (key(&r), r)).collect();
    let mut counts = Counts::default();
    let mut seen = HashSet::new();
    for row in new {
        let k = key(row);
        match old.get(&k) {
            None => counts.added += 1,
            Some(o) if o != row => counts.changed += 1,
            Some(_) => {}
        }
        seen.insert(k);
    }
    let removed = old.keys().filter(|k| !seen.contains(*k)).count();
    counts.removed = u64::try_from(removed).unwrap_or(u64::MAX);
    counts.total = u64::try_from(new.len()).unwrap_or(u64::MAX);
    counts
}

fn column<T: Clone, R>(rows: &[R], f: impl Fn(&R) -> T) -> Vec<T> {
    rows.iter().map(f).collect()
}

/// A duplicate key in the lists is a caller bug (the parser refuses it).
fn refuse_duplicates<R>(
    rows: &[R],
    key: impl Fn(&R) -> (String, String),
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

/// Replace the copy with `units` and `positions` (see the module
/// documentation). `ctx` is the worker's (`Actor::System`). An empty unit
/// list is refused (`FG-SYS-0001 reason=org_master_empty`): the parser
/// refuses it first, so a registry outage never wipes the copy.
pub async fn replace(
    db: &Db,
    ctx: &ChangeContext,
    units: &[MasterUnit],
    positions: &[MasterPosition],
) -> Result<Synced, ForgeError> {
    if units.is_empty() {
        return Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "org_master_empty"));
    }
    let units: Vec<UnitRow> = units.iter().map(unit_row).collect();
    let positions: Vec<PositionRow> = positions.iter().map(position_row).collect();
    refuse_duplicates(&units, |r| (r.0.clone(), r.1.clone()), "units")?;
    refuse_duplicates(&positions, |r| (r.0.clone(), r.1.clone()), "positions")?;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = replace_steps(&mut tx, &units, &positions).await;
    tx.settle(SYNCED_ACTION, outcome).await
}

async fn replace_steps(
    tx: &mut ChangeTx,
    units: &[UnitRow],
    positions: &[PositionRow],
) -> Result<Settle<Synced>, ForgeError> {
    tx.step(|c| async move {
        sqlx::query(&format!(
            "SET LOCAL lock_timeout = '{REPLACE_LOCK_WAIT_MS}ms'"
        ))
        .execute(&mut *c)
        .await
        .map_err(|e| query_error("org_master.replace.lock_timeout", e))?;
        // One sync at a time (two workers); readers are not blocked.
        sqlx::query(
            "LOCK TABLE forge.org_units_master, forge.org_positions_master IN EXCLUSIVE MODE",
        )
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| query_error("org_master.replace.lock", e))
    })
    .await?;
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
    let synced = Synced {
        units: diff(stored_units, units, |r| (r.0.clone(), r.1.clone())),
        positions: diff(stored_positions, positions, |r| (r.0.clone(), r.1.clone())),
    };
    tx.step(|c| async move {
        sqlx::query(
            "INSERT INTO forge.org_units_master (plant_code, code, level, parent_code, name_ja, \
             name_en, name_id, cost_center, head_staff_id, valid_from, version, synced_at) \
             SELECT p, c, l, pc, nj, ne, ni, cc, h, vf::date, v, now() \
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], \
                         $7::text[], $8::text[], $9::text[], $10::text[], $11::int4[]) \
                  AS n(p, c, l, pc, nj, ne, ni, cc, h, vf, v) \
             ON CONFLICT (plant_code, code) DO UPDATE SET level = EXCLUDED.level, \
             parent_code = EXCLUDED.parent_code, name_ja = EXCLUDED.name_ja, \
             name_en = EXCLUDED.name_en, name_id = EXCLUDED.name_id, \
             cost_center = EXCLUDED.cost_center, head_staff_id = EXCLUDED.head_staff_id, \
             valid_from = EXCLUDED.valid_from, version = EXCLUDED.version, synced_at = now()",
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
            "INSERT INTO forge.org_positions_master (plant_code, code, name_ja, name_en, name_id, \
             rank, sort_order, active, version, synced_at) \
             SELECT p, c, nj, ne, ni, r, s, a, v, now() \
             FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], \
                         $6::int4[], $7::int4[], $8::bool[], $9::int4[]) \
                  AS n(p, c, nj, ne, ni, r, s, a, v) \
             ON CONFLICT (plant_code, code) DO UPDATE SET name_ja = EXCLUDED.name_ja, \
             name_en = EXCLUDED.name_en, name_id = EXCLUDED.name_id, rank = EXCLUDED.rank, \
             sort_order = EXCLUDED.sort_order, active = EXCLUDED.active, \
             version = EXCLUDED.version, synced_at = now()",
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
    if !synced.units.touched() && !synced.positions.touched() {
        return Ok(Settle::CommitDerived(synced));
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
        }),
    };
    Ok(Settle::Commit(
        ChangeRecord {
            audit: entry,
            affected: Vec::new(),
            events: Vec::new(),
        },
        synced,
    ))
}

// ------------------------------------------------------------------- health

/// doctor's counts (the read pool).
pub async fn health(db: &Db) -> Result<MasterHealth, ForgeError> {
    let row: (i64, i64, Option<String>, Option<i64>) = db
        .within("org_master.health", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "SELECT (SELECT count(code) FROM forge.org_units_master), \
                 (SELECT count(code) FROM forge.org_positions_master), \
                 to_char(s.at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
                 extract(epoch FROM now() - s.at)::bigint \
                 FROM (SELECT max(synced_at) AS at FROM forge.org_units_master) s",
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

// -------------------------------------------------------------------- paths

/// One step of a parent chain as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChainRow {
    pub(crate) code: String,
    pub(crate) level: String,
    pub(crate) parent_code: Option<String>,
    pub(crate) name: LocalizedName,
    /// `valid_from` is not in the future.
    pub(crate) valid: bool,
}

type ChainSql = (
    String,
    String,
    i32,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    bool,
);

/// The parent chains of `starts` ((plant, code) pairs), each from the unit
/// up, at most [`MAX_DEPTH`] + 1 rows (the read pool).
async fn chains(
    db: &Db,
    starts: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<ChainRow>>, ForgeError> {
    if starts.is_empty() {
        return Ok(HashMap::new());
    }
    let plants: Vec<String> = column(starts, |s| s.0.clone());
    let codes: Vec<String> = column(starts, |s| s.1.clone());
    let rows: Vec<ChainSql> = db
        .within("org_master.chains", async {
            let mut conn = db.acquire().await?;
            sqlx::query_as(
                "WITH RECURSIVE chain (sp, sc, depth, code, level, parent_code, name_ja, \
                 name_en, name_id, valid) AS ( \
                   SELECT m.plant_code, m.code, 0, m.code, m.level, m.parent_code, m.name_ja, \
                          m.name_en, m.name_id, (m.valid_from IS NULL OR m.valid_from <= current_date) \
                   FROM (SELECT DISTINCT p, c FROM unnest($1::text[], $2::text[]) AS s(p, c)) s \
                   JOIN forge.org_units_master m ON m.plant_code = s.p AND m.code = s.c \
                   UNION ALL \
                   SELECT ch.sp, ch.sc, ch.depth + 1, m.code, m.level, m.parent_code, m.name_ja, \
                          m.name_en, m.name_id, (m.valid_from IS NULL OR m.valid_from <= current_date) \
                   FROM chain ch JOIN forge.org_units_master m \
                     ON m.plant_code = ch.sp AND m.code = ch.parent_code \
                   WHERE ch.depth < $3 \
                 ) SELECT sp, sc, depth, code, level, parent_code, name_ja, name_en, name_id, \
                   valid FROM chain ORDER BY sp, sc, depth",
            )
            .bind(&plants)
            .bind(&codes)
            .bind(MAX_DEPTH)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| read_query_error("org_master.chains", e))
        })
        .await?;
    let mut out: HashMap<(String, String), Vec<ChainRow>> = HashMap::new();
    for (sp, sc, _, code, level, parent_code, ja, en, id, valid) in rows {
        out.entry((sp, sc)).or_default().push(ChainRow {
            code,
            level,
            parent_code,
            name: LocalizedName { ja, en, id },
            valid,
        });
    }
    Ok(out)
}

/// The path of a unit from its parent chain (the unit first). Pure: the
/// level rules of ADR-0038 §1 — the section is the first `section` at or
/// above the unit, the department the first `department`; every parent
/// sits at a strictly higher level; a parent named but absent (or not yet
/// valid) is [`UnknownReason::MissingParent`].
pub(crate) fn path_of(chain: &[ChainRow]) -> Result<OrgPath, UnknownReason> {
    let Some(first) = chain.first() else {
        return Err(UnknownReason::NotInMaster);
    };
    if !first.valid {
        return Err(UnknownReason::NotYetValid);
    }
    let as_ref = |row: &ChainRow| -> Result<OrgRef, UnknownReason> {
        Ok(OrgRef {
            code: row.code.clone(), // clone: the path owns its rows
            level: UnitLevel::parse(&row.level).ok_or(UnknownReason::BrokenHierarchy)?,
            name: row.name.clone(),
        })
    };
    let unit = as_ref(first)?;
    let mut section = None;
    let mut department = None;
    let mut below = unit.level;
    for (i, row) in chain.iter().enumerate() {
        let r = as_ref(row)?;
        if i > 0 {
            if !row.valid {
                return Err(UnknownReason::MissingParent);
            }
            if r.level <= below {
                return Err(UnknownReason::BrokenHierarchy);
            }
            below = r.level;
        }
        match r.level {
            UnitLevel::Section => {
                if section.is_none() {
                    section = Some(r);
                }
            }
            UnitLevel::Department => {
                department = Some(r);
                break;
            }
            UnitLevel::Division => break,
            UnitLevel::Team => {}
        }
        let next = chain.get(i + 1);
        match (&row.parent_code, next) {
            (None, _) => break,
            (Some(parent), Some(n)) if &n.code == parent => {}
            (Some(_), Some(_)) => return Err(UnknownReason::BrokenHierarchy),
            (Some(_), None) => {
                // The chain stopped: the parent is absent, or the walk hit
                // MAX_DEPTH (a cycle).
                let depth = i32::try_from(i).unwrap_or(i32::MAX);
                return Err(if depth >= MAX_DEPTH {
                    UnknownReason::BrokenHierarchy
                } else {
                    UnknownReason::MissingParent
                });
            }
        }
    }
    Ok(OrgPath {
        unit,
        section,
        department,
    })
}

/// The paths of `codes` in `plant` that resolve (display: the self profile;
/// codes that do not resolve are left out). Without a plant nothing
/// resolves (the master is keyed by plant).
pub async fn unit_paths(
    db: &Db,
    plant: Option<&str>,
    codes: &[String],
) -> Result<HashMap<String, OrgPath>, ForgeError> {
    let Some(plant) = plant else {
        return Ok(HashMap::new());
    };
    let starts: Vec<(String, String)> = codes
        .iter()
        .map(|c| (plant.to_owned(), c.clone())) // clone: the key owns it
        .collect();
    let found = chains(db, &starts).await?;
    Ok(found
        .into_iter()
        .filter_map(|((_, code), chain)| path_of(&chain).ok().map(|p| (code, p)))
        .collect())
}

/// The primary unit's path of each of `users` whose snapshot names a
/// primary unit and a plant that resolve in the copy (display: the people
/// pickers and the approval list; at most [`MAX_USERS`]). The snapshot's
/// plant is read on the write pool (the read pool never sees the plants);
/// only the names leave this function.
pub async fn primary_paths(
    db: &Db,
    users: &[UserId],
) -> Result<HashMap<UserId, OrgPath>, ForgeError> {
    if users.is_empty() {
        return Ok(HashMap::new());
    }
    if users.len() > MAX_USERS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "too_many_users")
            .with_context("count", users.len()));
    }
    let ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let rows: Vec<(i64, String, String)> = db
        .within("org_master.primary_units", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.primary_units.acquire", e))?;
            sqlx::query_as(
                "SELECT p.user_id, p.plant_code, u ->> 'code' FROM forge.user_org_profiles p \
                 CROSS JOIN LATERAL jsonb_array_elements(p.org_units) u \
                 WHERE p.user_id = ANY($1) AND p.plant_code IS NOT NULL \
                   AND u ->> 'primary' = 'true'",
            )
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("org_master.primary_units", e))
        })
        .await?;
    let starts: Vec<(String, String)> = rows.iter().map(|r| (r.1.clone(), r.2.clone())).collect(); // clone: keys
    let found = chains(db, &starts).await?;
    let mut out = HashMap::new();
    for (user, plant, code) in rows {
        if let Some(path) = found.get(&(plant, code)).and_then(|c| path_of(c).ok()) {
            out.insert(UserId::new(user), path);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------- derive

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
/// [`UnknownReason`] for the order of the checks). The write pool for the
/// snapshot (the plant and the positions are not on the read pool).
pub async fn derive(db: &Db, user: UserId, ages: MaxAges) -> Result<Derived, ForgeError> {
    let row: Option<ProfileSql> = db
        .within("org_master.derive", async {
            let mut conn = db
                .write_pool()
                .acquire()
                .await
                .map_err(|e| query_error("org_master.derive.acquire", e))?;
            sqlx::query_as(
                "SELECT p.org_units::text, p.plant_code, p.positions, p.position_rank, p.digest, \
                 p.invalid_since IS NOT NULL, \
                 extract(epoch FROM now() - p.confirmed_at)::bigint, \
                 (SELECT extract(epoch FROM now() - max(synced_at))::bigint \
                  FROM forge.org_units_master) \
                 FROM forge.user_org_profiles p WHERE p.user_id = $1",
            )
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
    let older = |age: i64, max: Duration| u64::try_from(age).unwrap_or(0) > max.as_secs();
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
    let units: Vec<crate::org_profiles::OrgUnit> =
        serde_json::from_str(&org_units).map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "stored_org_units_invalid")
                .with_context("detail", e)
        })?;
    let Some(primary) = units.into_iter().find(|u| u.primary) else {
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

#[cfg(test)]
mod tests;
