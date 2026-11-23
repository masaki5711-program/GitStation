//! Approvers chosen by organisation (ADR-0038 §3, step C): the selector of
//! §3.1, a person's organisation as recorded with an approval (the
//! snapshot), and the pure decisions on them — which unit a selector names
//! for a requester (with `escalate: parent`) and whether a person matches.
//!
//! Shared by forge-db (which stores selectors and snapshots as jsonb),
//! forge-review (merge request approval rules) and forge-deploy
//! (environment approvers). Pure: no I/O, no clock.
//!
//! Fail closed (ADR-0038 §4): a requester whose organisation is unknown
//! makes a selector unresolvable (no one matches it); an approver whose
//! snapshot was unknown at approval time matches no selector; a person
//! without a rank never meets `min_rank`.

use std::collections::BTreeSet;

use serde_json::{json, Map, Value};

/// The most named users, groups and selectors of one approver list
/// (ADR-0038 §3.4).
pub const MAX_NAMED_USERS: usize = 20;
pub const MAX_GROUPS: usize = 10;
pub const MAX_SELECTORS: usize = 5;
/// The most position codes of one selector (a person holds at most 4).
pub const MAX_POSITIONS: usize = 10;
/// The longest delegation (ADR-0038 §3.5).
pub const MAX_DELEGATION_DAYS: i64 = 30;

/// Whose unit a selector names (§3.1 `relation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Relation {
    /// The requester's section (from the primary unit).
    RequesterSection,
    /// The requester's department.
    RequesterDepartment,
    /// A named unit (`unit`), in the requester's plant.
    Unit,
}

impl Relation {
    pub const ALL: [Relation; 3] = [
        Relation::RequesterSection,
        Relation::RequesterDepartment,
        Relation::Unit,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RequesterSection => "requester_section",
            Self::RequesterDepartment => "requester_department",
            Self::Unit => "unit",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == raw)
    }
}

/// What happens when no one matches (§3.1 `escalate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Escalate {
    None,
    /// Choose again in the unit one level up (once).
    Parent,
}

impl Escalate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Parent => "parent",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "none" => Some(Self::None),
            "parent" => Some(Self::Parent),
            _ => None,
        }
    }
}

/// Who in the unit qualifies (§3.1: `min_rank` or `positions`, or anyone).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Qualifier {
    /// Anyone whose primary unit is in the unit.
    Anyone,
    /// A position rank of at least this (0..=999).
    MinRank(i32),
    /// Holds one of these position codes.
    Positions(BTreeSet<String>),
}

/// One selector (§3.1), validated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApproverSelector {
    pub relation: Relation,
    /// The unit code (`relation = unit` only).
    pub unit: Option<String>,
    /// `unit` / `requester_department`: the unit's descendants too.
    pub include_descendants: bool,
    pub qualifier: Qualifier,
    pub escalate: Escalate,
}

/// Why a selector as sent is refused (`FG-WEB-0006 param=selectors`).
pub type SelectorError = &'static str;

/// An org unit code (`^[A-Z0-9][A-Z0-9_-]{0,19}$`, as the master's CHECK).
pub fn unit_code_ok(code: &str) -> bool {
    let mut bytes = code.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        && code.len() <= 20
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// A position code (`^[a-z][a-z0-9_]{0,63}$`).
pub fn position_code_ok(code: &str) -> bool {
    let mut bytes = code.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && code.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl ApproverSelector {
    /// Parse and validate one selector object: unknown keys, a missing or
    /// unknown `relation`, a `unit` with another relation (or none with
    /// `unit`), `include_descendants` on `requester_section`, both
    /// `min_rank` and `positions`, a rank outside 0..=999, a bad code, and
    /// `head_only: true` (not available until PFS fills the heads,
    /// ADR-0038 §8.4) are refused.
    pub fn from_value(value: &Value) -> Result<Self, SelectorError> {
        let obj = value.as_object().ok_or("not_an_object")?;
        const KEYS: [&str; 7] = [
            "relation",
            "unit",
            "include_descendants",
            "min_rank",
            "positions",
            "head_only",
            "escalate",
        ];
        if obj.keys().any(|k| !KEYS.contains(&k.as_str())) {
            return Err("unknown_field");
        }
        let relation = obj
            .get("relation")
            .and_then(Value::as_str)
            .and_then(Relation::parse)
            .ok_or("relation")?;
        let unit = match obj.get("unit") {
            None | Some(Value::Null) => None,
            Some(Value::String(u)) if unit_code_ok(u) => Some(u.clone()), // clone: the selector owns it
            Some(_) => return Err("unit_format"),
        };
        match (relation, &unit) {
            (Relation::Unit, None) => return Err("unit_required"),
            (Relation::RequesterSection | Relation::RequesterDepartment, Some(_)) => {
                return Err("unit_not_allowed")
            }
            _ => {}
        }
        let include_descendants = match obj.get("include_descendants") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("include_descendants"),
        };
        if include_descendants && relation == Relation::RequesterSection {
            return Err("include_descendants");
        }
        let min_rank = match obj.get("min_rank") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let n = v.as_i64().ok_or("min_rank")?;
                if !(0..=999).contains(&n) {
                    return Err("min_rank");
                }
                Some(i32::try_from(n).map_err(|_| "min_rank")?)
            }
        };
        let positions = match obj.get("positions") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                if items.is_empty() || items.len() > MAX_POSITIONS {
                    return Err("positions");
                }
                let mut set = BTreeSet::new();
                for item in items {
                    let code = item.as_str().filter(|c| position_code_ok(c));
                    set.insert(code.ok_or("positions")?.to_owned());
                }
                Some(set)
            }
            Some(_) => return Err("positions"),
        };
        let qualifier = match (min_rank, positions) {
            (Some(_), Some(_)) => return Err("min_rank_or_positions"),
            (Some(n), None) => Qualifier::MinRank(n),
            (None, Some(p)) => Qualifier::Positions(p),
            (None, None) => Qualifier::Anyone,
        };
        match obj.get("head_only") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => return Err("head_only_unavailable"),
            Some(_) => return Err("head_only"),
        }
        let escalate = match obj.get("escalate") {
            None | Some(Value::Null) => Escalate::None,
            Some(v) => v.as_str().and_then(Escalate::parse).ok_or("escalate")?,
        };
        Ok(Self {
            relation,
            unit,
            include_descendants,
            qualifier,
            escalate,
        })
    }

    /// The canonical stored form (every key, `null` for the absent ones):
    /// what the database's CHECK and [`Self::from_value`] both accept.
    pub fn to_value(&self) -> Value {
        let (min_rank, positions) = match &self.qualifier {
            Qualifier::Anyone => (Value::Null, Value::Null),
            Qualifier::MinRank(n) => (json!(n), Value::Null),
            Qualifier::Positions(p) => (Value::Null, json!(p.iter().collect::<Vec<_>>())),
        };
        let mut m = Map::new();
        m.insert("relation".into(), json!(self.relation.as_str()));
        m.insert("unit".into(), json!(self.unit));
        m.insert(
            "include_descendants".into(),
            json!(self.include_descendants),
        );
        m.insert("min_rank".into(), min_rank);
        m.insert("positions".into(), positions);
        m.insert("head_only".into(), json!(false));
        m.insert("escalate".into(), json!(self.escalate.as_str()));
        Value::Object(m)
    }
}

/// An org unit's level (the registry's `level`; ordered from the bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OrgLevel {
    Team,
    Section,
    Department,
    Division,
}

impl OrgLevel {
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

/// One unit of a person's chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChainUnit {
    pub code: String,
    pub level: OrgLevel,
}

/// A person's organisation as derived from a valid, fresh snapshot and a
/// fresh master (ADR-0038 §1): the primary unit and every unit above it,
/// each at a strictly higher level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrgSnapshot {
    pub plant: String,
    /// The primary unit first, then its parents (never empty).
    pub chain: Vec<ChainUnit>,
    pub rank: Option<i32>,
    pub positions: Vec<String>,
    /// The claims snapshot's digest (kept with an approval for audit).
    pub digest: String,
}

impl OrgSnapshot {
    /// The first unit of `level` at or above the primary unit.
    pub fn at_level(&self, level: OrgLevel) -> Option<&ChainUnit> {
        self.chain.iter().find(|u| u.level == level)
    }

    pub fn primary(&self) -> Option<&ChainUnit> {
        self.chain.first()
    }

    /// The unit directly above `code` in the chain.
    pub fn parent_of(&self, code: &str) -> Option<&ChainUnit> {
        let i = self.chain.iter().position(|u| u.code == code)?;
        self.chain.get(i + 1)
    }
}

/// A person's organisation as recorded or read: known, or why not (the
/// reasons of ADR-0038 §4, `no_snapshot`, `stale_snapshot`, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PersonOrg {
    Known(OrgSnapshot),
    Unknown(String),
}

impl PersonOrg {
    pub fn known(&self) -> Option<&OrgSnapshot> {
        match self {
            Self::Known(s) => Some(s),
            Self::Unknown(_) => None,
        }
    }

    /// The stored form (`merge_request_approvals.approver_org`,
    /// `deployment_approvals.approver_org`): codes, levels, the rank, the
    /// positions and the digest only — never names (ADR-0038 §6).
    pub fn to_value(&self) -> Value {
        match self {
            Self::Known(s) => json!({
                "plant": s.plant,
                "chain": s.chain.iter().map(|u| json!({
                    "code": u.code, "level": u.level.as_str(),
                })).collect::<Vec<_>>(),
                "rank": s.rank,
                "positions": s.positions,
                "digest": s.digest,
            }),
            Self::Unknown(reason) => json!({ "unknown": reason }),
        }
    }

    /// The stored form read back; `None` when it is not one (the caller
    /// fails closed and logs).
    pub fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        if let Some(reason) = obj.get("unknown") {
            return Some(Self::Unknown(reason.as_str()?.to_owned()));
        }
        let plant = obj.get("plant")?.as_str()?.to_owned();
        let chain = obj
            .get("chain")?
            .as_array()?
            .iter()
            .map(|u| {
                Some(ChainUnit {
                    code: u.get("code")?.as_str()?.to_owned(),
                    level: OrgLevel::parse(u.get("level")?.as_str()?)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        if chain.is_empty() {
            return None;
        }
        let rank = match obj.get("rank") {
            None | Some(Value::Null) => None,
            Some(v) => Some(i32::try_from(v.as_i64()?).ok()?),
        };
        let positions = obj
            .get("positions")
            .and_then(Value::as_array)
            .map(|p| {
                p.iter()
                    .map(|c| c.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or(Some(Vec::new()))?;
        let digest = obj.get("digest")?.as_str()?.to_owned();
        Some(Self::Known(OrgSnapshot {
            plant,
            chain,
            rank,
            positions,
            digest,
        }))
    }
}

/// The unit a selector resolved to for one requester.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub plant: String,
    pub unit: String,
    /// Only people whose primary unit is `unit` itself.
    pub exact: bool,
    /// Escalated one level up (`escalate: parent`).
    pub escalated: bool,
}

/// Why a selector names no unit for a requester (shown with "no one can
/// approve", ADR-0038 §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Unresolved {
    /// The requester's organisation is unknown (the §4 reason).
    RequesterUnknown(String),
    /// The requester's chain has no section / department.
    RequesterNoSection,
    RequesterNoDepartment,
    /// `relation = unit` names a code the master does not hold (in the
    /// requester's plant).
    UnitNotInMaster,
}

impl Unresolved {
    pub fn as_str(&self) -> &str {
        match self {
            Self::RequesterUnknown(_) => "requester_org_unknown",
            Self::RequesterNoSection => "requester_no_section",
            Self::RequesterNoDepartment => "requester_no_department",
            Self::UnitNotInMaster => "unit_not_in_master",
        }
    }

    /// The §4 reason behind `requester_org_unknown`.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::RequesterUnknown(r) => Some(r),
            Self::RequesterNoSection | Self::RequesterNoDepartment | Self::UnitNotInMaster => None,
        }
    }
}

/// The unit `selector` names for `requester` before any escalation, and
/// the unit one level up (for `escalate: parent`). `unit_chain`: the
/// chain of `selector.unit` in the requester's plant as the master holds
/// it (the unit first; `None` / empty when absent) — read for
/// `relation = unit` only.
pub fn base_target(
    selector: &ApproverSelector,
    requester: &PersonOrg,
    unit_chain: Option<&[ChainUnit]>,
) -> Result<(Target, Option<String>), Unresolved> {
    let r = match requester {
        PersonOrg::Known(r) => r,
        PersonOrg::Unknown(reason) => return Err(Unresolved::RequesterUnknown(reason.clone())), // clone: the reason is shown
    };
    let (unit, exact, parent) = match selector.relation {
        Relation::RequesterSection => {
            let s = r
                .at_level(OrgLevel::Section)
                .ok_or(Unresolved::RequesterNoSection)?;
            (
                s.code.clone(),
                false,
                r.parent_of(&s.code).map(|p| p.code.clone()),
            ) // clone: owned target
        }
        Relation::RequesterDepartment => {
            let d = r
                .at_level(OrgLevel::Department)
                .ok_or(Unresolved::RequesterNoDepartment)?;
            (
                d.code.clone(), // clone: owned target
                !selector.include_descendants,
                r.parent_of(&d.code).map(|p| p.code.clone()),
            )
        }
        Relation::Unit => {
            let chain = unit_chain
                .filter(|c| !c.is_empty())
                .ok_or(Unresolved::UnitNotInMaster)?;
            let code = selector
                .unit
                .as_deref()
                .ok_or(Unresolved::UnitNotInMaster)?;
            if chain[0].code != code {
                return Err(Unresolved::UnitNotInMaster);
            }
            (
                code.to_owned(),
                !selector.include_descendants,
                chain.get(1).map(|p| p.code.clone()), // clone: owned target
            )
        }
    };
    Ok((
        Target {
            plant: r.plant.clone(), // clone: owned target
            unit,
            exact,
            escalated: false,
        },
        parent,
    ))
}

/// The unit a selector names for a requester (§3.1): the base unit, or —
/// `escalate: parent` and `base_has_candidates == Some(false)` — the unit
/// one level up, descendants included. `None` for `base_has_candidates`
/// (could not tell, e.g. too many people to read) never escalates: an
/// escalation widens who may approve, so it needs a known "no one".
pub fn resolve(
    selector: &ApproverSelector,
    requester: &PersonOrg,
    unit_chain: Option<&[ChainUnit]>,
    base_has_candidates: Option<bool>,
) -> Result<Target, Unresolved> {
    let (base, parent) = base_target(selector, requester, unit_chain)?;
    match (selector.escalate, base_has_candidates, parent) {
        (Escalate::Parent, Some(false), Some(parent)) => Ok(Target {
            plant: base.plant,
            unit: parent,
            exact: false,
            escalated: true,
        }),
        _ => Ok(base),
    }
}

/// Whether `person` is in `target` (plant, then the primary unit itself or
/// any unit above it) and meets `qualifier`.
pub fn matches(qualifier: &Qualifier, target: &Target, person: &OrgSnapshot) -> bool {
    if person.plant != target.plant {
        return false;
    }
    let in_unit = if target.exact {
        person.primary().is_some_and(|p| p.code == target.unit)
    } else {
        person.chain.iter().any(|u| u.code == target.unit)
    };
    in_unit
        && match qualifier {
            Qualifier::Anyone => true,
            Qualifier::MinRank(n) => person.rank.is_some_and(|r| r >= *n),
            Qualifier::Positions(ps) => person.positions.iter().any(|p| ps.contains(p)),
        }
}

#[cfg(test)]
#[path = "approver_tests.rs"]
mod tests;
