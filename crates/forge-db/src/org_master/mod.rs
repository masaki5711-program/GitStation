//! The copy of the PFS organisation master (ADR-0038 §1, step A):
//! `forge.org_units_master`, `forge.org_positions_master` and the one-row
//! `forge.org_master_state` (the last successful sync), replaced by the
//! worker task `org_master_sync` ([`replace`]) and read to give the codes
//! of a person's org claims (`forge.user_org_profiles`) a level, a parent
//! and names.
//!
//! - [`replace`] (`replace.rs`): one transaction under an advisory lock;
//!   the whole lists are checked first (hierarchy, shrink guard), rows are
//!   written only when they differ, the sync's time goes to the state row,
//!   one `org.master_synced` audit row when anything changed. Nothing is
//!   written when it fails: the previous copy stays.
//! - [`derive`] (`read.rs`): a person's **primary** unit and the section
//!   and department above it, found by walking `parent_code` and reading
//!   `level`, never by parsing a code. Fail closed (ADR-0038 §4).
//! - [`unit_paths`] / [`primary_paths`]: the names shown next to codes,
//!   behind the same gates as [`derive`]; past a gate they are still shown,
//!   marked stale (the last known names).
//! - [`health`]: doctor's `org.master`.
//! - [`person_orgs`] / [`plant_people`] / [`unit_chain`] (`people.rs`):
//!   the whole chain of people's primary units for approvals (ADR-0038 §3).

use std::collections::HashMap;
use std::time::Duration;

use forge_errors::ForgeError;

use crate::{read_query_error, Db};

mod people;
mod read;
mod replace;
mod selectors;

pub use people::{
    person_orgs, plant_people, search_units, unit_chain, FoundUnit, MAX_PLANT_PEOPLE,
    MAX_UNIT_RESULTS,
};
pub use read::{derive, health, primary_paths, unit_paths, ShownPath, MAX_UNIT_CODES};
pub use replace::{replace, ReplacePolicy, Replaced, SYNCED_ACTION};
pub use selectors::{resolve_selectors, Resolution, ResolvedSelector};
/// The longest parent chain walked (division → department → section → team
/// is 4; a longer chain is a cycle or a broken master).
pub const MAX_DEPTH: i32 = 8;
/// At most this many users per [`primary_paths`] call (a page's worth).
pub const MAX_USERS: usize = 1000;

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

/// How old the facts may be (ADR-0038 §4; [`Db::with_org_ages`] from the
/// configuration's `org.*_age_hours`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxAges {
    /// `org.max_profile_age_hours`.
    pub profile: Duration,
    /// `org.master_max_age_hours`.
    pub master: Duration,
}

impl Default for MaxAges {
    fn default() -> Self {
        Self {
            profile: Duration::from_secs(24 * 3600),
            master: Duration::from_secs(24 * 3600),
        }
    }
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

// -------------------------------------------------------------------- paths

fn column<T, R>(rows: &[R], f: impl Fn(&R) -> T) -> Vec<T> {
    rows.iter().map(f).collect()
}

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

#[cfg(test)]
mod tests;
