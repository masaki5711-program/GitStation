//! The approval half of `evaluate_merge()` (M3 R5, ADR-0024 §7, §8, §9): a
//! pure function from the facts read for one decision to how each approval
//! counts and what blocks the merge. It reads neither the database nor git.
//!
//! An approval **counts** only when, now: it is for the merge request's
//! current head and target (`stale`), its approver is active
//! (`inactive`), a person (`not_human`), Developer or above on the project
//! (`role_insufficient`), not the author (`author`), and not a committer of
//! the range (`committer`; `committers_unknown` when that cannot be shown —
//! fail closed) while the project's setting "committers' approvals do not
//! count" is on (the default; M3 R6 makes it a project setting).
//!
//! Requirements, each blocking on its own:
//! - every approval rule whose `target_pattern` is absent or matches the
//!   target needs `required_approvals` counted approvals from its approvers
//!   (any person when it names none) — `approvals_missing`;
//! - M3 R6: the target's protection needs at least its merge rule's
//!   minimum counted approvals from anyone eligible — `approvals_missing`
//!   with the rule [`PROTECTED_BRANCH_RULE`] (ADR-0024 §8.6: a minimum);
//! - every changed path's last-matching CODEOWNERS rule needs one counted
//!   approval by one of its owners (a user, or a direct member of a group)
//!   — `codeowner_approval_missing` (once per rule);
//! - ADR-0038 §3 (step C): a rule's approvers mix users, groups and
//!   organisation selectors (any one of them qualifies). A selector matches
//!   on the snapshot recorded **with the approval** (the approver's
//!   organisation at approval time) against the unit it names for the
//!   merge request's author now ([`SelectorTargets`]); an approval without
//!   a known snapshot, or a selector that names no unit, matches nothing
//!   (fail closed). The exclusions above (stale, inactive, not a person,
//!   role, author, committer) come first, whatever a selector says.
//! - ADR-0038 §3.5: an approval given on someone's behalf counts as theirs
//!   (their user, groups and recorded snapshot), once the delegator also
//!   passes the exclusions (active, a person, not the author, not a
//!   committer); each person counts once per requirement, however many
//!   approvals are theirs.
//! - an unusable CODEOWNERS file — `codeowners_invalid`;
//! - (both CODEOWNERS requirements only while enforcement is on: M3 R6's
//!   time-boxed suspension, ADR-0024 §9, drops them — never the
//!   configuration rule);
//! - a changed configuration path ([`is_config_path`]) needs one counted
//!   approval of a Maintainer or higher — `config_change_needs_maintainer_approval`;
//! - changed paths that could not be read in full — `changed_paths_unknown`
//!   (CODEOWNERS and the configuration rule are then not evaluated: never
//!   "no owners").

use std::collections::{BTreeMap, BTreeSet};

use forge_core::approver::{matches, ApproverSelector, PersonOrg, Target, Unresolved};
use forge_core::ids::{ApprovalId, GroupId, UserId};
use forge_core::{ObjectId, RefName, Role, UncountedReason, UserKind};
use forge_db::review::{ApprovalFacts, ApproverFacts, StoredApprovalRule};

use super::codeowners::{Codeowners, CodeownersError, Owner};
use crate::merges::BlockReason;

/// Who pushed the range's commits (ADR-0024 §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committers {
    /// The pushers, each with the audit row of one push that carried a
    /// range commit (the evidence shown with `committer`).
    Known(BTreeMap<UserId, i64>),
    /// Cannot be shown (`reason`): no approval counts.
    Unknown(&'static str),
}

/// Every path the merge changes on the target (the target tip against the
/// trial merge's tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedPathsFact {
    Known(Vec<String>),
    /// The trial merge conflicts: nothing can land, and the conflict blocks
    /// the merge on its own (the mergeability check, the job's trial merge,
    /// FG-REVIEW-0006). CODEOWNERS and the configuration rule are not
    /// evaluated and add no reason.
    Conflict,
    /// `too_many_paths`, `timeout`, `git_failed`, `non_utf8_path`,
    /// `target_missing`.
    Unknown(&'static str),
}

/// The name of the approval requirement of a protected branch's minimum
/// (M3 R6): `@` never starts an approval rule's name, so it cannot collide.
pub const PROTECTED_BRANCH_RULE: &str = "@protected_branch";

/// What an owner resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerRef {
    User(UserId),
    Group(GroupId),
}

/// The target's CODEOWNERS file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeownersFact {
    Absent,
    /// Parsed, every owner resolved, and matched against the changed paths
    /// (`matched`: line → how many changed paths it owns; computed by the
    /// loader within a time budget, R5 review).
    Valid {
        path: &'static str,
        file: Codeowners,
        owners: BTreeMap<Owner, OwnerRef>,
        matched: BTreeMap<u32, usize>,
    },
    Invalid(CodeownersError),
}

/// The unit each selector names for the merge request's author now
/// (ADR-0038 §3.1; `escalate: parent` already applied), by selector. A
/// selector absent from it names no unit (fail closed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectorTargets(pub Vec<(ApproverSelector, Result<Target, Unresolved>)>);

impl SelectorTargets {
    pub fn get(&self, selector: &ApproverSelector) -> Option<&Result<Target, Unresolved>> {
        self.0.iter().find(|(s, _)| s == selector).map(|(_, t)| t)
    }
}

/// The inputs of the approval half of one decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalInputs {
    pub author: UserId,
    /// The merge request's current head and target.
    pub head: ObjectId,
    pub target: RefName,
    pub facts: ApprovalFacts,
    pub committers: Committers,
    pub changed: ChangedPathsFact,
    pub codeowners: CodeownersFact,
    /// M3 R6: the project's "committers' approvals do not count" (default
    /// on).
    pub committer_approvals_excluded: bool,
    /// M3 R6: CODEOWNERS enforced now (off only during a live suspension).
    pub codeowners_enforced: bool,
    /// M3 R6: the target's protection's minimum counted approvals (0: none).
    pub minimum: u32,
    /// ADR-0038 §3.1: the units the rules' selectors name for the author.
    pub targets: SelectorTargets,
}

impl ApprovalInputs {
    /// No approval, no rule, no CODEOWNERS, nothing changed (tests).
    #[cfg(test)]
    pub(crate) fn nothing_required() -> Self {
        Self {
            author: UserId::new(1),
            head: ObjectId::parse(&"a".repeat(40)).expect("oid"),
            target: RefName::parse("refs/heads/main").expect("ref"),
            facts: ApprovalFacts::default(),
            committers: Committers::Known(BTreeMap::new()),
            changed: ChangedPathsFact::Known(Vec::new()),
            codeowners: CodeownersFact::Absent,
            committer_approvals_excluded: true,
            codeowners_enforced: true,
            minimum: 0,
            targets: SelectorTargets::default(),
        }
    }
}

/// How one approval counts now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStatus {
    pub approval: ApprovalId,
    pub approver: UserId,
    pub username: String,
    pub counted: bool,
    pub uncounted_reason: Option<UncountedReason>,
    /// `committer`: the audit row of a push by the approver that carried a
    /// range commit.
    pub committer_push: Option<i64>,
    /// ADR-0038 §3.5: the person the approval was given for.
    pub on_behalf_of: Option<(UserId, String)>,
}

/// One approval rule's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleStatus {
    pub name: String,
    pub applies: bool,
    pub required: u32,
    pub counted: u32,
}

/// One CODEOWNERS rule some changed path needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeownerStatus {
    pub line: u32,
    pub pattern: String,
    pub owners: Vec<String>,
    /// How many changed paths it owns.
    pub paths: usize,
    pub satisfied: bool,
}

/// The evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalReport {
    pub approvals: Vec<ApprovalStatus>,
    pub rules: Vec<RuleStatus>,
    pub codeowners: Vec<CodeownerStatus>,
    /// `Some(satisfied)` when a configuration path changed.
    pub config_change: Option<bool>,
    /// M3 R6: whether CODEOWNERS was enforced in this evaluation.
    pub codeowners_enforced: bool,
    pub blocks: Vec<BlockReason>,
}

/// The configuration paths of ADR-0017 §2.7 (`.forge/**`, `.forge-ci.yml`,
/// the root `CODEOWNERS`; `.forge/CODEOWNERS` is under `.forge/`). The
/// protected paths and the files `.forge-ci.yml` includes have no input in
/// M3 (both M4): until then they are empty sets
/// (`is_config_path_holds_no_protected_paths_or_ci_includes_until_m4`).
/// ASCII case is ignored (R5 review: `.Forge/ci.yml` on a case-insensitive
/// checkout is the same file).
pub fn is_config_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.starts_with(".forge/") || path == ".forge-ci.yml" || path == "codeowners"
}

fn classify(inputs: &ApprovalInputs, a: &ApproverFacts) -> (Option<UncountedReason>, Option<i64>) {
    let stale = a.approval.head_sha != inputs.head || a.approval.target_branch != inputs.target;
    let reason = if stale {
        Some(UncountedReason::Stale)
    } else if !a.active {
        Some(UncountedReason::Inactive)
    } else if a.kind != UserKind::Human {
        Some(UncountedReason::NotHuman)
    } else if !a.role.is_some_and(|r| r >= Role::Developer) {
        Some(UncountedReason::RoleInsufficient)
    } else if a.approval.approver == inputs.author {
        Some(UncountedReason::Author)
    } else if a.delegator.as_ref().is_some_and(|d| !d.active) {
        // ADR-0038 §3.5: the person it counts for passes the same checks.
        Some(UncountedReason::Inactive)
    } else if a
        .delegator
        .as_ref()
        .is_some_and(|d| d.kind != UserKind::Human)
    {
        Some(UncountedReason::NotHuman)
    } else if a.delegator.as_ref().is_some_and(|d| !d.delegation_holds) {
        // Removed, or given to another person: it no longer counts, even
        // before the clean-up ends it.
        Some(UncountedReason::DelegationEnded)
    } else if a
        .delegator
        .as_ref()
        .is_some_and(|d| !d.role.is_some_and(|r| r >= Role::Developer))
    {
        // Review of ADR-0038 step C: an approval counts for the delegator
        // only while they could approve themself.
        Some(UncountedReason::RoleInsufficient)
    } else if a
        .delegator
        .as_ref()
        .is_some_and(|d| d.user == inputs.author)
    {
        Some(UncountedReason::Author)
    } else if !inputs.committer_approvals_excluded {
        // M3 R6: the project counts committers' approvals.
        None
    } else {
        match &inputs.committers {
            Committers::Unknown(_) => Some(UncountedReason::CommittersUnknown),
            Committers::Known(pushers) => {
                let behalf = a.delegator.as_ref().map(|d| d.user);
                match [Some(a.approval.approver), behalf]
                    .into_iter()
                    .flatten()
                    .find_map(|u| pushers.get(&u))
                {
                    Some(audit) => return (Some(UncountedReason::Committer), Some(*audit)),
                    None => None,
                }
            }
        }
    };
    (reason, None)
}

/// The person an approval counts for (ADR-0038 §3.5): the delegator of an
/// approval given on their behalf, else the approver.
pub fn counts_for(a: &ApproverFacts) -> UserId {
    a.delegator.as_ref().map_or(a.approval.approver, |d| d.user)
}

fn groups_of(a: &ApproverFacts) -> &BTreeSet<GroupId> {
    a.delegator.as_ref().map_or(&a.groups, |d| &d.groups)
}

/// Whether the snapshot recorded with `a` matches `selector` for this
/// merge request (fail closed: no target, no known snapshot — no match).
fn selector_matches(
    targets: &SelectorTargets,
    selector: &ApproverSelector,
    a: &ApproverFacts,
) -> bool {
    let Some(Ok(target)) = targets.get(selector) else {
        return false;
    };
    match &a.approval.approver_org {
        Some(PersonOrg::Known(snapshot)) => matches(&selector.qualifier, target, snapshot),
        Some(PersonOrg::Unknown(_)) | None => false,
    }
}

fn in_rule(rule: &StoredApprovalRule, targets: &SelectorTargets, a: &ApproverFacts) -> bool {
    let who = counts_for(a);
    (rule.users.is_empty() && rule.groups.is_empty() && rule.selectors.is_empty())
        || rule.users.contains(&who)
        || rule.groups.iter().any(|g| groups_of(a).contains(g))
        || rule
            .selectors
            .iter()
            .any(|s| selector_matches(targets, s, a))
}

fn owns(owners: &[Owner], resolved: &BTreeMap<Owner, OwnerRef>, a: &ApproverFacts) -> bool {
    owners.iter().any(|o| match resolved.get(o) {
        Some(OwnerRef::User(u)) => *u == counts_for(a),
        Some(OwnerRef::Group(g)) => groups_of(a).contains(g),
        None => false,
    })
}

/// How many distinct people the approvals count for.
fn people<'a>(approvals: impl Iterator<Item = &'a &'a ApproverFacts>) -> u32 {
    let distinct: BTreeSet<UserId> = approvals.map(|a| counts_for(a)).collect();
    u32::try_from(distinct.len()).unwrap_or(u32::MAX)
}

/// Evaluate `inputs` (module docs).
pub fn evaluate(inputs: &ApprovalInputs) -> ApprovalReport {
    let classified: Vec<(&ApproverFacts, Option<UncountedReason>, Option<i64>)> = inputs
        .facts
        .approvals
        .iter()
        .map(|a| {
            let (reason, push) = classify(inputs, a);
            (a, reason, push)
        })
        .collect();
    let valid: Vec<&ApproverFacts> = classified
        .iter()
        .filter(|(_, r, _)| r.is_none())
        .map(|(a, _, _)| *a)
        .collect();
    let mut blocks = Vec::new();
    // Rules.
    let mut rules = Vec::new();
    for rule in &inputs.facts.rules {
        let applies = rule
            .target_pattern
            .as_ref()
            .is_none_or(|p| p.matches(&inputs.target));
        let counted = people(valid.iter().filter(|a| in_rule(rule, &inputs.targets, a)));
        let required = u32::try_from(rule.required_approvals).unwrap_or(0);
        if applies && counted < required {
            blocks.push(BlockReason::ApprovalsMissing {
                rule: rule.name.clone(), // clone: the reason owns the name
                required,
                counted,
            });
        }
        rules.push(RuleStatus {
            name: rule.name.clone(), // clone: as above
            applies,
            required,
            counted,
        });
    }
    // M3 R6: the protected branch's minimum (anyone eligible counts).
    if inputs.minimum > 0 {
        let counted = people(valid.iter());
        if counted < inputs.minimum {
            blocks.push(BlockReason::ApprovalsMissing {
                rule: PROTECTED_BRANCH_RULE.to_owned(),
                required: inputs.minimum,
                counted,
            });
        }
        rules.push(RuleStatus {
            name: PROTECTED_BRANCH_RULE.to_owned(),
            applies: true,
            required: inputs.minimum,
            counted,
        });
    }
    // CODEOWNERS (M3 R6: not while its enforcement is suspended).
    let mut codeowners = Vec::new();
    let mut invalid = None;
    if let (
        true,
        CodeownersFact::Valid {
            file,
            owners,
            matched,
            ..
        },
        ChangedPathsFact::Known(_),
    ) = (
        inputs.codeowners_enforced,
        &inputs.codeowners,
        &inputs.changed,
    ) {
        let by_line = matched.iter().filter_map(|(line, count)| {
            file.rules
                .iter()
                .find(|r| r.line == *line)
                .map(|r| (*count, r))
        });
        for (count, rule) in by_line {
            let satisfied = valid.iter().any(|a| owns(&rule.owners, owners, a));
            let names: Vec<String> = rule.owners.iter().map(Owner::display).collect();
            if !satisfied {
                blocks.push(BlockReason::CodeownerApprovalMissing {
                    pattern: rule.pattern.clone(), // clone: the reason owns it
                    owners: names.clone(),         // clone: the status keeps them too
                });
            }
            codeowners.push(CodeownerStatus {
                line: rule.line,
                pattern: rule.pattern.clone(), // clone: as above
                owners: names,
                paths: count,
                satisfied,
            });
        }
    }
    if let (true, CodeownersFact::Invalid(e)) = (inputs.codeowners_enforced, &inputs.codeowners) {
        invalid = Some(BlockReason::CodeownersInvalid {
            line: e.line,
            reason: e.reason,
        });
    }
    blocks.extend(invalid);
    // Configuration paths.
    let config_change = match &inputs.changed {
        ChangedPathsFact::Known(paths) if paths.iter().any(|p| is_config_path(p)) => Some(
            valid
                .iter()
                .any(|a| a.role.is_some_and(|r| r >= Role::Maintainer)),
        ),
        ChangedPathsFact::Known(_) | ChangedPathsFact::Conflict | ChangedPathsFact::Unknown(_) => {
            None
        }
    };
    if config_change == Some(false) {
        blocks.push(BlockReason::ConfigChangeNeedsMaintainerApproval);
    }
    if let ChangedPathsFact::Unknown(reason) = &inputs.changed {
        blocks.push(BlockReason::ChangedPathsUnknown { reason });
    }
    // How each approval counts: valid and needed by something (or nothing
    // is required at all).
    let any_requirement =
        rules.iter().any(|r| r.applies) || !codeowners.is_empty() || config_change.is_some();
    let approvals = classified
        .into_iter()
        .map(|(a, reason, push)| {
            let reason = reason.or_else(|| {
                let needed = !any_requirement
                    || inputs.minimum > 0
                    || inputs
                        .facts
                        .rules
                        .iter()
                        .zip(&rules)
                        .any(|(rule, status)| status.applies && in_rule(rule, &inputs.targets, a))
                    || match &inputs.codeowners {
                        CodeownersFact::Valid { .. } if !inputs.codeowners_enforced => false,
                        CodeownersFact::Valid { file, owners, .. } => codeowners.iter().any(|c| {
                            file.rules
                                .iter()
                                .find(|r| r.line == c.line)
                                .is_some_and(|r| owns(&r.owners, owners, a))
                        }),
                        CodeownersFact::Absent | CodeownersFact::Invalid(_) => false,
                    }
                    || (config_change.is_some() && a.role.is_some_and(|r| r >= Role::Maintainer));
                (!needed).then_some(UncountedReason::NotInRule)
            });
            ApprovalStatus {
                approval: a.approval.id,
                approver: a.approval.approver,
                username: a.username.clone(), // clone: the status owns it
                counted: reason.is_none(),
                uncounted_reason: reason,
                committer_push: push,
                on_behalf_of: a.delegator.as_ref().map(|d| (d.user, d.username.clone())), // clone: the status owns it
            }
        })
        .collect();
    ApprovalReport {
        approvals,
        rules,
        codeowners,
        config_change,
        codeowners_enforced: inputs.codeowners_enforced,
        blocks,
    }
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod tests;
