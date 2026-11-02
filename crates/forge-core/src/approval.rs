//! Approvals of merge requests (M3 R5, ADR-0024 §6, §8): why an approval
//! ended, why an approval does not count, and the audit details of an
//! approval rule change. Shared by forge-db (which stores them) and
//! forge-review (which decides with them).

use crate::ids::{ApprovalRuleId, GroupId, UserId};
use crate::RefPattern;

/// Why an approval ended (`merge_request.unapproved` `reason`, the row's
/// `revoked_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnapproveReason {
    /// The approver withdrew it.
    Unapproved,
    /// A new head was recorded (ADR-0024 §8.2: new commits clear the
    /// approvals by default, plan 11 Q4).
    NewCommits,
    /// The target branch changed (ADR-0024 §8.1).
    TargetChanged,
}

impl UnapproveReason {
    pub const ALL: [UnapproveReason; 3] = [Self::Unapproved, Self::NewCommits, Self::TargetChanged];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unapproved => "unapproved",
            Self::NewCommits => "new_commits",
            Self::TargetChanged => "target_changed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == raw)
    }
}

/// Why an approval is not counted (ADR-0024 §6 `uncounted_reason`, §8.3,
/// §8.6), as decided at evaluation time — never at approval time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UncountedReason {
    /// The approval is for another head or target than the merge
    /// request's current ones.
    Stale,
    /// The approver is no longer active (disabled or locked).
    Inactive,
    /// The approver is a service account or a bot.
    NotHuman,
    /// The approver's role on the project is below Developer now.
    RoleInsufficient,
    /// The approver is the merge request's author.
    Author,
    /// The approver pushed commits of the merge request's range (ADR-0024
    /// §8.3; the project setting "committers' approvals do not count" is
    /// on by default).
    Committer,
    /// Who pushed the range's commits cannot be shown (push records
    /// missing or cut, the range too long): the approval is not counted
    /// (fail closed).
    CommittersUnknown,
    /// Valid, but no applicable rule, CODEOWNERS entry or configuration
    /// requirement lists the approver (informational: such an approval
    /// counts for nothing).
    NotInRule,
}

impl UncountedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::Inactive => "inactive",
            Self::NotHuman => "not_human",
            Self::RoleInsufficient => "role_insufficient",
            Self::Author => "author",
            Self::Committer => "committer",
            Self::CommittersUnknown => "committers_unknown",
            Self::NotInRule => "not_in_rule",
        }
    }
}

/// The audit details of an approval rule change (`approval_rule.put` /
/// `approval_rule.removed`, audit only: no domain event). Ids, the name,
/// the pattern and the counts only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRuleChange {
    /// Created, or replaced (`created: false`).
    Put {
        rule: ApprovalRuleId,
        name: String,
        target_pattern: Option<RefPattern>,
        required_approvals: i32,
        users: Vec<UserId>,
        groups: Vec<GroupId>,
        created: bool,
    },
    Removed {
        rule: ApprovalRuleId,
        name: String,
    },
}

impl ApprovalRuleChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Put { .. } => "approval_rule.put",
            Self::Removed { .. } => "approval_rule.removed",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Put {
                rule,
                name,
                target_pattern,
                required_approvals,
                users,
                groups,
                created,
            } => serde_json::json!({
                "approval_rule_id": rule.get(),
                "name": name,
                "target_pattern": target_pattern.as_ref().map(RefPattern::as_str),
                "required_approvals": required_approvals,
                "user_ids": users.iter().map(|u| u.get()).collect::<Vec<_>>(),
                "group_ids": groups.iter().map(|g| g.get()).collect::<Vec<_>>(),
                "created": created,
            }),
            Self::Removed { rule, name } => serde_json::json!({
                "approval_rule_id": rule.get(),
                "name": name,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unapprove_reasons_round_trip() {
        for r in UnapproveReason::ALL {
            assert_eq!(UnapproveReason::parse(r.as_str()), Some(r));
        }
        assert_eq!(UnapproveReason::parse("other"), None);
    }
}
