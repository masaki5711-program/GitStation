//! Resolving approver selectors for one requester (ADR-0038 §3.1, §3.3,
//! step C): the facts both approval paths need — merge request rules
//! (forge-review) and deploy environment approvers (forge-deploy) — read
//! the same way.
//!
//! For each selector: the unit it names for the requester
//! ([`forge_core::approver::resolve`]), escalated one level up when
//! `escalate: parent` and **no eligible person** is in the base unit, and
//! the eligible people of the final unit (active people with at least
//! `min_role` on the project, never one of `exclude` — the author, the
//! requester). "Could not tell" (a plant with more than
//! [`super::MAX_PLANT_PEOPLE`] people) never escalates and lists no one
//! (`candidates: None`).
//!
//! Facts only: whether an approval counts is decided on the snapshot
//! recorded with it (forge-review's `evaluate`, `authorize()` for a
//! deployment), never on this list.

use std::collections::{BTreeMap, BTreeSet};

use forge_core::approver::{
    base_target, matches, resolve, ApproverSelector, OrgSnapshot, PersonOrg, Relation, Target,
    Unresolved,
};
use forge_core::ids::{ProjectId, UserId};
use forge_core::{Role, UserKind};
use forge_errors::ForgeError;

use super::{person_orgs, plant_people, unit_chain};
use crate::review::{people_facts, PersonFacts};
use crate::Db;

/// One selector resolved for a requester.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelector {
    pub selector: ApproverSelector,
    pub target: Result<Target, Unresolved>,
    /// The eligible people of the final unit, by user id (`None`: could
    /// not tell). Empty with a resolved target: "no one can approve".
    pub candidates: Option<Vec<UserId>>,
}

/// The requester's organisation and each selector resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub requester: PersonOrg,
    pub selectors: Vec<ResolvedSelector>,
    /// The facts of every candidate (names for display).
    pub people: BTreeMap<UserId, PersonFacts>,
}

impl Resolution {
    /// The target of `selector` (`None`: not resolved here).
    pub fn target(&self, selector: &ApproverSelector) -> Option<&Result<Target, Unresolved>> {
        self.selectors
            .iter()
            .find(|r| &r.selector == selector)
            .map(|r| &r.target)
    }
}

fn eligible(p: &PersonFacts, min_role: Role) -> bool {
    p.active && p.kind == UserKind::Human && p.role.is_some_and(|r| r >= min_role)
}

/// Resolve `selectors` for `requester` on `project` (module docs).
pub async fn resolve_selectors(
    db: &Db,
    project: ProjectId,
    requester: Option<UserId>,
    selectors: &[ApproverSelector],
    min_role: Role,
    exclude: &[UserId],
) -> Result<Resolution, ForgeError> {
    let requester_org = match requester {
        Some(r) => person_orgs(db, &[r])
            .await?
            .remove(&r)
            .unwrap_or_else(|| PersonOrg::Unknown("no_snapshot".to_owned())),
        // An automatic deployment has no requester: no unit to name.
        None => PersonOrg::Unknown("no_requester".to_owned()),
    };
    let mut out = Resolution {
        requester: requester_org.clone(), // clone: the answer keeps it
        selectors: Vec::with_capacity(selectors.len()),
        people: BTreeMap::new(),
    };
    if selectors.is_empty() {
        return Ok(out);
    }
    let Some(snapshot) = requester_org.known() else {
        for s in selectors {
            out.selectors.push(ResolvedSelector {
                selector: s.clone(), // clone: the answer owns it
                target: resolve(s, &requester_org, None, None),
                candidates: Some(Vec::new()),
            });
        }
        return Ok(out);
    };
    let plant = snapshot.plant.clone(); // clone: borrowed below
    let population: Option<Vec<(UserId, OrgSnapshot)>> = plant_people(db, &plant).await?;
    // Every chain a `relation = unit` selector names, once.
    let mut chains = BTreeMap::new();
    for s in selectors {
        if let (Relation::Unit, Some(unit)) = (s.relation, s.unit.as_deref()) {
            if !chains.contains_key(unit) {
                chains.insert(unit.to_owned(), unit_chain(db, &plant, unit).await?);
            }
        }
    }
    let chain_of = |s: &ApproverSelector| {
        s.unit
            .as_deref()
            .and_then(|u| chains.get(u))
            .and_then(|c| c.as_deref())
    };
    // The people who could be in any unit a selector names (its base or
    // its parent), with their facts.
    let in_any: BTreeSet<UserId> = match &population {
        None => BTreeSet::new(),
        Some(people) => people
            .iter()
            .filter(|(u, snap)| {
                !exclude.contains(u)
                    && selectors.iter().any(|s| {
                        let base = base_target(s, &requester_org, chain_of(s));
                        let escalated = resolve(s, &requester_org, chain_of(s), Some(false));
                        [base.map(|(t, _)| t), escalated]
                            .iter()
                            .any(|t| t.as_ref().is_ok_and(|t| matches(&s.qualifier, t, snap)))
                    })
            })
            .map(|(u, _)| *u)
            .collect(),
    };
    let ids: Vec<UserId> = in_any.into_iter().collect();
    for chunk in ids.chunks(crate::review::approvals::MAX_PEOPLE) {
        out.people.extend(people_facts(db, project, chunk).await?);
    }
    let eligible_in = |t: &Target, s: &ApproverSelector| -> Option<Vec<UserId>> {
        population.as_ref().map(|people| {
            people
                .iter()
                .filter(|(u, snap)| {
                    !exclude.contains(u)
                        && matches(&s.qualifier, t, snap)
                        && out.people.get(u).is_some_and(|p| eligible(p, min_role))
                })
                .map(|(u, _)| *u)
                .collect()
        })
    };
    let mut resolved = Vec::with_capacity(selectors.len());
    for s in selectors {
        let chain = chain_of(s);
        let base = base_target(s, &requester_org, chain).map(|(t, _)| t);
        let has = match &base {
            Ok(t) => eligible_in(t, s).map(|c| !c.is_empty()),
            Err(_) => Some(false),
        };
        let target = resolve(s, &requester_org, chain, has);
        let candidates = match &target {
            Ok(t) => eligible_in(t, s),
            Err(_) => Some(Vec::new()),
        };
        resolved.push(ResolvedSelector {
            selector: s.clone(), // clone: the answer owns it
            target,
            candidates,
        });
    }
    out.selectors = resolved;
    Ok(out)
}
