//! The merge conditions of M3 R6 (ADR-0024 §7, §8, §9, §10, §12): a
//! protected branch's merge rule ([`MergeRule`]: the minimum approvals,
//! the required check names, resolved discussions, the direct-push ban),
//! the names a required check may have ([`CheckName`]), and the audit
//! details of a project's merge settings ([`MergeSettingsChange`]) and of a
//! protected ref's merge rule ([`MergeRuleChange`]). Shared by forge-db
//! (which stores them), forge-auth (the direct-push ban), forge-hosting
//! and forge-review (which decide with them).

use crate::ids::ProtectedRefId;
use crate::merge::MergeMethod;
use crate::RefPattern;

/// The most required checks one merge rule names.
pub const MAX_REQUIRED_CHECKS: usize = 20;
/// The most approvals a merge rule may require (as an approval rule).
pub const MAX_REQUIRED_APPROVALS: u32 = 20;
/// The longest a CODEOWNERS enforcement may be suspended (ADR-0024 §9:
/// at most 24 hours), in minutes.
pub const MAX_CODEOWNERS_SUSPENSION_MINUTES: u32 = 24 * 60;

/// A check run namespace (ADR-0024 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CheckNamespace {
    /// `ci/`: the CI service account.
    Ci,
    /// `forge/`: forge's own `system` worker.
    Forge,
    /// `kanflow/`: the kanflow service account.
    Kanflow,
    /// `ext/`: a person's external tool — never a required check
    /// (`FG-REVIEW-0016` RequiredCheckNamespaceNotAllowed).
    Ext,
}

impl CheckNamespace {
    pub const ALL: [CheckNamespace; 4] = [Self::Ci, Self::Forge, Self::Kanflow, Self::Ext];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Forge => "forge",
            Self::Kanflow => "kanflow",
            Self::Ext => "ext",
        }
    }

    /// Whether a check of this namespace may be required: only the ones a
    /// service account or forge owns (ADR-0024 §10), never `ext/` — a name
    /// a person can write must not satisfy a merge condition.
    pub fn may_be_required(self) -> bool {
        match self {
            Self::Ci | Self::Forge | Self::Kanflow => true,
            Self::Ext => false,
        }
    }
}

/// Why a string is no check name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckNameError {
    /// Not `<namespace>/<name>` with the allowed characters.
    Format,
    /// The namespace is none of `ci`, `forge`, `kanflow`, `ext`.
    UnknownNamespace,
}

impl CheckNameError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::UnknownNamespace => "unknown_namespace",
        }
    }
}

/// A check run's name `<namespace>/<name>` (ADR-0024 §10): the namespace
/// one of [`CheckNamespace`], the name 1–100 bytes of lowercase ASCII
/// letters, digits, `.`, `_`, `-` and `/`, starting with a letter or a
/// digit (C1 writes check runs under the same rule).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckName {
    raw: String,
    namespace: CheckNamespace,
}

impl CheckName {
    /// The longest name after the namespace.
    pub const MAX_NAME_BYTES: usize = 100;

    pub fn parse(raw: &str) -> Result<Self, CheckNameError> {
        let (ns, name) = raw.split_once('/').ok_or(CheckNameError::Format)?;
        let first_ok = name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let rest_ok = name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-' | b'/')
        });
        if !first_ok
            || !rest_ok
            || name.len() > Self::MAX_NAME_BYTES
            || name.ends_with('/')
            || name.contains("//")
        {
            return Err(CheckNameError::Format);
        }
        let namespace = CheckNamespace::ALL
            .into_iter()
            .find(|n| n.as_str() == ns)
            .ok_or(CheckNameError::UnknownNamespace)?;
        Ok(Self {
            raw: raw.to_owned(),
            namespace,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn namespace(&self) -> CheckNamespace {
        self.namespace
    }
}

impl std::fmt::Display for CheckName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// The merge rule of a protected branch (`protected_ref_merge_rules`, M3
/// R6, ADR-0024 §7, §8.6, §12). Several rules may match one branch: they
/// combine to the strictest ([`MergeRule::strictest`]), like the minimum
/// push roles (adding a rule never loosens a branch).
///
/// M3 I1d: [`MergeRule::require_work_item_link`] is the work item link
/// condition (`work_item_link_missing`), configurable since I1b brought the
/// links (ADR-0024 §11 and its I1d note).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeRule {
    /// The minimum counted approvals of a merge into the branch (0: none),
    /// on top of the approval rules (ADR-0024 §8.6: a minimum).
    pub required_approvals: u32,
    /// Every check that must have a `completed` / `success` run on the
    /// merge request's head (sorted, unique; never `ext/`).
    pub required_checks: Vec<CheckName>,
    /// Every discussion of the merge request must be resolved.
    pub require_resolved_discussions: bool,
    /// Pushes may not create or move the branch: it changes through merge
    /// requests only (`MergeRequestMerge` is not affected, ADR-0024 §4.3).
    pub direct_push_forbidden: bool,
    /// M3 I1d: the merge request must have an open link to a work item of
    /// its project's current integration binding (`work_item_link_missing`
    /// otherwise; a project without a binding can never satisfy it).
    pub require_work_item_link: bool,
}

impl MergeRule {
    /// Whether the rule asks for nothing (a rule row with every field off).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The strictest combination of `rules`: the highest minimum, every
    /// required check (sorted, unique), and each switch on when any rule
    /// has it on.
    pub fn strictest<'a>(rules: impl IntoIterator<Item = &'a MergeRule>) -> MergeRule {
        let mut out = MergeRule::default();
        for rule in rules {
            out.required_approvals = out.required_approvals.max(rule.required_approvals);
            out.required_checks
                .extend(rule.required_checks.iter().cloned()); // clone: the combination owns its names
            out.require_resolved_discussions |= rule.require_resolved_discussions;
            out.direct_push_forbidden |= rule.direct_push_forbidden;
            out.require_work_item_link |= rule.require_work_item_link;
        }
        out.required_checks.sort();
        out.required_checks.dedup();
        out
    }
}

/// The audit details of a protected ref's merge rule change
/// (`protected_ref.merge_rule_put` / `protected_ref.merge_rule_removed`,
/// audit only, on the project).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRuleChange {
    Put {
        rule: ProtectedRefId,
        pattern: RefPattern,
        merge_rule: MergeRule,
        created: bool,
    },
    Removed {
        rule: ProtectedRefId,
        pattern: RefPattern,
    },
}

fn merge_rule_json(rule: &MergeRule) -> serde_json::Value {
    serde_json::json!({
        "required_approvals": rule.required_approvals,
        "required_checks": rule.required_checks.iter().map(CheckName::as_str).collect::<Vec<_>>(),
        "require_resolved_discussions": rule.require_resolved_discussions,
        "direct_push_forbidden": rule.direct_push_forbidden,
        "require_work_item_link": rule.require_work_item_link,
    })
}

impl MergeRuleChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Put { .. } => "protected_ref.merge_rule_put",
            Self::Removed { .. } => "protected_ref.merge_rule_removed",
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Put {
                rule,
                pattern,
                merge_rule,
                created,
            } => serde_json::json!({
                "protected_ref_id": rule.get(),
                "pattern": pattern.as_str(),
                "merge_rule": merge_rule_json(merge_rule),
                "created": created,
            }),
            Self::Removed { rule, pattern } => serde_json::json!({
                "protected_ref_id": rule.get(),
                "pattern": pattern.as_str(),
            }),
        }
    }
}

/// A project's merge settings as written (M3 R6, ADR-0024 §4.1, §8.2,
/// §8.3): the methods allowed (never empty) and the default among them,
/// whether the source branch is deleted by default, whether new commits
/// clear the approvals, and whether committers' approvals are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSettingsValues {
    pub allowed_methods: Vec<MergeMethod>,
    pub default_method: MergeMethod,
    pub delete_source_default: bool,
    pub reset_approvals_on_push: bool,
    pub committer_approvals_excluded: bool,
}

impl Default for MergeSettingsValues {
    /// ADR-0024 §4.1, §8.2, §8.3 and plan 11 Q3/Q4: merge commit and
    /// squash, merge commit by default, the source kept, new commits clear
    /// the approvals, committers' approvals do not count.
    fn default() -> Self {
        Self {
            allowed_methods: vec![MergeMethod::MergeCommit, MergeMethod::Squash],
            default_method: MergeMethod::MergeCommit,
            delete_source_default: false,
            reset_approvals_on_push: true,
            committer_approvals_excluded: true,
        }
    }
}

/// The audit details of a project's merge settings change (M3 R6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeSettingsChange {
    /// `merge_settings.updated` (audit only): the values written.
    Updated { values: MergeSettingsValues },
    /// `merge_settings.codeowners_enforcement_changed` (an event of the
    /// maintainer class): CODEOWNERS enforcement suspended until
    /// `until` (UTC, RFC 3339; at most 24 hours), or enforced again by a
    /// person (`until: None`).
    CodeownersEnforcementChanged {
        enforced: bool,
        until: Option<String>,
    },
    /// `merge_settings.codeowners_enforcement_restored` (an event of the
    /// maintainer class, actor `system`): a suspension expired and the
    /// worker recorded it (ADR-0024 §9: for the record only — the
    /// evaluation counts an expired suspension as enforced by itself).
    CodeownersEnforcementRestored { expired_at: String },
}

impl MergeSettingsChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Updated { .. } => "merge_settings.updated",
            Self::CodeownersEnforcementChanged { .. } => {
                "merge_settings.codeowners_enforcement_changed"
            }
            Self::CodeownersEnforcementRestored { .. } => {
                "merge_settings.codeowners_enforcement_restored"
            }
        }
    }

    pub fn to_value(&self, project: crate::ProjectId) -> serde_json::Value {
        match self {
            Self::Updated { values } => serde_json::json!({
                "project_id": project.get(),
                "allowed_methods": values
                    .allowed_methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>(),
                "default_method": values.default_method.as_str(),
                "delete_source_default": values.delete_source_default,
                "reset_approvals_on_push": values.reset_approvals_on_push,
                "committer_approvals_excluded": values.committer_approvals_excluded,
            }),
            Self::CodeownersEnforcementChanged { enforced, until } => serde_json::json!({
                "project_id": project.get(),
                "enforced": enforced,
                "until": until,
            }),
            Self::CodeownersEnforcementRestored { expired_at } => serde_json::json!({
                "project_id": project.get(),
                "enforced": true,
                "expired_at": expired_at,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_names_need_a_known_namespace_and_a_plain_name() {
        let ok = CheckName::parse("ci/gates").expect("ok");
        assert_eq!(ok.namespace(), CheckNamespace::Ci);
        assert_eq!(ok.as_str(), "ci/gates");
        assert!(CheckName::parse("kanflow/pbi-linked").is_ok());
        assert!(CheckName::parse("forge/spec-drift").is_ok());
        assert!(CheckName::parse("ci/lint/rust_1.80").is_ok());
        assert_eq!(
            CheckName::parse("ext/sonar")
                .expect("ext parses")
                .namespace(),
            CheckNamespace::Ext
        );
        for bad in [
            "", "ci", "ci/", "ci/Gates", "ci/-x", "ci/a b", "ci/a/", "ci//a",
        ] {
            assert_eq!(CheckName::parse(bad), Err(CheckNameError::Format), "{bad}");
        }
        for unknown in ["gh/actions", "/gates"] {
            assert_eq!(
                CheckName::parse(unknown),
                Err(CheckNameError::UnknownNamespace),
                "{unknown}"
            );
        }
        let long = format!("ci/{}", "a".repeat(CheckName::MAX_NAME_BYTES + 1));
        assert_eq!(CheckName::parse(&long), Err(CheckNameError::Format));
    }

    #[test]
    fn only_owned_namespaces_may_be_required() {
        let required: Vec<&str> = CheckNamespace::ALL
            .into_iter()
            .filter(|n| n.may_be_required())
            .map(CheckNamespace::as_str)
            .collect();
        assert_eq!(required, ["ci", "forge", "kanflow"]);
    }

    #[test]
    fn rules_combine_to_the_strictest() {
        let a = MergeRule {
            required_approvals: 1,
            required_checks: vec![CheckName::parse("ci/gates").expect("name")],
            require_resolved_discussions: false,
            direct_push_forbidden: true,
            require_work_item_link: false,
        };
        let b = MergeRule {
            required_approvals: 2,
            required_checks: vec![
                CheckName::parse("kanflow/linked").expect("name"),
                CheckName::parse("ci/gates").expect("name"),
            ],
            require_resolved_discussions: true,
            direct_push_forbidden: false,
            require_work_item_link: false,
        };
        let both = MergeRule::strictest([&a, &b]);
        assert_eq!(both.required_approvals, 2);
        assert_eq!(
            both.required_checks
                .iter()
                .map(CheckName::as_str)
                .collect::<Vec<_>>(),
            ["ci/gates", "kanflow/linked"]
        );
        assert!(both.require_resolved_discussions && both.direct_push_forbidden);
        assert!(MergeRule::strictest([]).is_empty());
    }

    /// M3 I1d: one rule asking for a work item link is enough (adding a
    /// rule never loosens a branch).
    #[test]
    fn a_work_item_link_required_by_any_rule_is_required() {
        let plain = MergeRule::default();
        let linked = MergeRule {
            require_work_item_link: true,
            ..MergeRule::default()
        };
        assert!(MergeRule::strictest([&plain, &linked]).require_work_item_link);
        assert!(!MergeRule::strictest([&plain]).require_work_item_link);
        assert!(!linked.is_empty());
        let change = MergeRuleChange::Put {
            rule: ProtectedRefId::new(3),
            pattern: RefPattern::parse("refs/heads/main").expect("pattern"),
            merge_rule: linked,
            created: true,
        };
        assert_eq!(
            change.to_value()["merge_rule"]["require_work_item_link"],
            true
        );
    }
}
