//! The approval half of `evaluate_merge()` (M3 R5, ADR-0024 §7, §8, §9): a
//! pure function from the facts read for one decision to how each approval
//! counts and what blocks the merge. It reads neither the database nor git.
//!
//! An approval **counts** only when, now: it is for the merge request's
//! current head and target (`stale`), its approver is active
//! (`inactive`), a person (`not_human`), Developer or above on the project
//! (`role_insufficient`), not the author (`author`), and not a committer of
//! the range (`committer`; `committers_unknown` when that cannot be shown —
//! fail closed). The setting "committers' approvals do not count" is on for
//! every project until R6 adds the project settings.
//!
//! Requirements, each blocking on its own:
//! - every approval rule whose `target_pattern` is absent or matches the
//!   target needs `required_approvals` counted approvals from its approvers
//!   (any person when it names none) — `approvals_missing`;
//! - every changed path's last-matching CODEOWNERS rule needs one counted
//!   approval by one of its owners (a user, or a direct member of a group)
//!   — `codeowner_approval_missing` (once per rule);
//! - an unusable CODEOWNERS file — `codeowners_invalid`;
//! - a changed configuration path ([`is_config_path`]) needs one counted
//!   approval of a Maintainer or higher — `config_change_needs_maintainer_approval`;
//! - changed paths that could not be read in full — `changed_paths_unknown`
//!   (CODEOWNERS and the configuration rule are then not evaluated: never
//!   "no owners").

use std::collections::BTreeMap;

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

/// Every path changed between the merge base and the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangedPathsFact {
    Known(Vec<String>),
    /// `too_many_paths`, `timeout`, `git_failed`, `non_utf8_path`,
    /// `no_merge_base`, `target_missing`.
    Unknown(&'static str),
}

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
    /// Parsed, every owner resolved.
    Valid {
        path: &'static str,
        file: Codeowners,
        owners: BTreeMap<Owner, OwnerRef>,
    },
    Invalid(CodeownersError),
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
    pub blocks: Vec<BlockReason>,
}

/// The configuration paths of ADR-0017 §2.7 (`.forge/**`, `.forge-ci.yml`,
/// the root `CODEOWNERS`; `.forge/CODEOWNERS` is under `.forge/`). The
/// protected paths and the files `.forge-ci.yml` includes have no input in
/// M3 (both M4): until then they are empty sets
/// (`is_config_path_holds_no_protected_paths_or_ci_includes_until_m4`).
pub fn is_config_path(path: &str) -> bool {
    path.starts_with(".forge/") || path == ".forge-ci.yml" || path == "CODEOWNERS"
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
    } else {
        match &inputs.committers {
            Committers::Unknown(_) => Some(UncountedReason::CommittersUnknown),
            Committers::Known(pushers) => match pushers.get(&a.approval.approver) {
                Some(audit) => return (Some(UncountedReason::Committer), Some(*audit)),
                None => None,
            },
        }
    };
    (reason, None)
}

fn in_rule(rule: &StoredApprovalRule, a: &ApproverFacts) -> bool {
    (rule.users.is_empty() && rule.groups.is_empty())
        || rule.users.contains(&a.approval.approver)
        || rule.groups.iter().any(|g| a.groups.contains(g))
}

fn owns(owners: &[Owner], resolved: &BTreeMap<Owner, OwnerRef>, a: &ApproverFacts) -> bool {
    owners.iter().any(|o| match resolved.get(o) {
        Some(OwnerRef::User(u)) => *u == a.approval.approver,
        Some(OwnerRef::Group(g)) => a.groups.contains(g),
        None => false,
    })
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
        let counted = valid.iter().filter(|a| in_rule(rule, a)).count();
        let counted = u32::try_from(counted).unwrap_or(u32::MAX);
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
    // CODEOWNERS.
    let mut codeowners = Vec::new();
    let mut invalid = None;
    if let (CodeownersFact::Valid { file, owners, .. }, ChangedPathsFact::Known(paths)) =
        (&inputs.codeowners, &inputs.changed)
    {
        let mut by_line: BTreeMap<u32, (usize, &super::codeowners::Rule)> = BTreeMap::new();
        for path in paths {
            if let Some(rule) = file.rule_for(path) {
                by_line.entry(rule.line).or_insert((0, rule)).0 += 1;
            }
        }
        for (count, rule) in by_line.into_values() {
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
    if let CodeownersFact::Invalid(e) = &inputs.codeowners {
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
        ChangedPathsFact::Known(_) | ChangedPathsFact::Unknown(_) => None,
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
                    || inputs
                        .facts
                        .rules
                        .iter()
                        .zip(&rules)
                        .any(|(rule, status)| status.applies && in_rule(rule, a))
                    || match &inputs.codeowners {
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
            }
        })
        .collect();
    ApprovalReport {
        approvals,
        rules,
        codeowners,
        config_change,
        blocks,
    }
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod tests;
