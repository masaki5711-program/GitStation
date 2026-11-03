//! A protection rule's merge rule (M3 R6, ADR-0024 §12): `PUT|DELETE
//! /api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule`, decided as
//! a change of that rule (`ProtectedRefManage` with the rule's level as
//! both `from` and `to`: people only, Maintainer, never above one's own
//! role), then again on the locked facts.
//!
//! The ask's checks here (`FG-WEB-0006` unless noted): at most 20
//! approvals and 20 checks, every check `<namespace>/<name>`, never `ext/`
//! (`FG-REVIEW-0016`), and no work item link condition until I1
//! (ADR-0024 §11: `rule=unavailable_until_i1`).

use forge_auth::{Action, Authorized, LoadedProtection, RefProtectionCtx};
use forge_core::merge_conditions::{MAX_REQUIRED_APPROVALS, MAX_REQUIRED_CHECKS};
use forge_core::{CheckName, MergeRule};
use forge_db::protected_refs::{self, MergeRuleOutcome};
use forge_errors::{ErrorCode, ForgeError};

use crate::{
    account, caller, change_context, loaded, protection_level, require, shape_mismatch, Hosting,
    WriteRequest,
};

/// A merge rule as asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergeRuleAsk {
    pub required_approvals: u32,
    pub required_checks: Vec<String>,
    pub require_resolved_discussions: bool,
    pub direct_push_forbidden: bool,
    /// Refused while `true` (no work items before I1).
    pub require_work_item_link: bool,
}

fn param_invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

impl MergeRuleAsk {
    /// The merge rule, or why it cannot be one.
    pub fn to_rule(&self) -> Result<MergeRule, ForgeError> {
        if self.require_work_item_link {
            return Err(param_invalid(
                "require_work_item_link",
                "unavailable_until_i1",
            ));
        }
        if self.required_approvals > MAX_REQUIRED_APPROVALS {
            return Err(param_invalid("required_approvals", "range"));
        }
        if self.required_checks.len() > MAX_REQUIRED_CHECKS {
            return Err(param_invalid("required_checks", "too_many"));
        }
        let mut required_checks = Vec::with_capacity(self.required_checks.len());
        for raw in &self.required_checks {
            let name =
                CheckName::parse(raw).map_err(|e| param_invalid("required_checks", e.as_str()))?;
            if !name.namespace().may_be_required() {
                return Err(ForgeError::new(ErrorCode::RequiredCheckNamespaceNotAllowed)
                    .with_context("reason", "namespace_not_allowed")
                    .with_context("namespace", name.namespace().as_str())
                    .with_context("check", raw.clone())); // clone: the log names the refused check
            }
            required_checks.push(name);
        }
        required_checks.sort();
        required_checks.dedup();
        Ok(MergeRule {
            required_approvals: self.required_approvals,
            required_checks,
            require_resolved_discussions: self.require_resolved_discussions,
            direct_push_forbidden: self.direct_push_forbidden,
        })
    }
}

/// The rule a merge-rule proof was decided for (its level as `from` and
/// `to`).
fn rule_of(
    proof: &Authorized<RefProtectionCtx>,
    command: &'static str,
) -> Result<forge_core::ProtectedRefId, ForgeError> {
    let wanted = proof.ctx().protection();
    match (wanted.rule, wanted.from, wanted.to) {
        (Some(rule), from, to) if from == to => Ok(rule),
        _ => Err(shape_mismatch(command)),
    }
}

impl Hosting {
    /// `PUT …/protected-refs/{rule_id}/merge-rule` (`ProtectedRefManage`):
    /// create or replace the rule's merge rule.
    pub async fn put_merge_rule(
        &self,
        request: &WriteRequest,
        proof: &Authorized<RefProtectionCtx>,
        ask: &MergeRuleAsk,
    ) -> Result<(MergeRuleOutcome, MergeRule), ForgeError> {
        require(proof.action(), Action::ProtectedRefManage)?;
        let rule = rule_of(proof, "merge_rule_put")?;
        let merge = ask.to_rule()?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project = proof.id().0;
        let (outcome, ()) = protected_refs::put_merge_rule_decided(
            &self.db,
            &ctx,
            project,
            rule,
            user,
            &merge,
            |facts| {
                let level = protection_level(facts.rule);
                let fresh = RefProtectionCtx::new(
                    loaded(&facts.project),
                    LoadedProtection {
                        rule: Some(rule),
                        from: level,
                        to: level,
                    },
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok((outcome, merge))
    }

    /// `DELETE …/protected-refs/{rule_id}/merge-rule`
    /// (`ProtectedRefManage`).
    pub async fn remove_merge_rule(
        &self,
        request: &WriteRequest,
        proof: &Authorized<RefProtectionCtx>,
    ) -> Result<MergeRuleOutcome, ForgeError> {
        require(proof.action(), Action::ProtectedRefManage)?;
        let rule = rule_of(proof, "merge_rule_remove")?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project = proof.id().0;
        let (outcome, ()) = protected_refs::remove_merge_rule_decided(
            &self.db,
            &ctx,
            project,
            rule,
            user,
            |facts| {
                let level = protection_level(facts.rule);
                let fresh = RefProtectionCtx::new(
                    loaded(&facts.project),
                    LoadedProtection {
                        rule: Some(rule),
                        from: level,
                        to: level,
                    },
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FG-REVIEW-0016 (ADR-0024 §10): an `ext/` check is never required.
    #[test]
    fn an_ext_check_cannot_be_required() {
        let ask = MergeRuleAsk {
            required_checks: vec!["ci/gates".to_owned(), "ext/sonar".to_owned()],
            ..MergeRuleAsk::default()
        };
        let err = ask.to_rule().expect_err("ext");
        assert_eq!(err.code(), ErrorCode::RequiredCheckNamespaceNotAllowed);
        assert!(err
            .context()
            .iter()
            .any(|(k, v)| *k == "namespace" && v == "ext"));
    }

    #[test]
    fn a_merge_rule_ask_is_checked() {
        let ok = MergeRuleAsk {
            required_approvals: 2,
            required_checks: vec![
                "kanflow/linked".to_owned(),
                "ci/gates".to_owned(),
                "ci/gates".to_owned(),
            ],
            require_resolved_discussions: true,
            direct_push_forbidden: true,
            require_work_item_link: false,
        }
        .to_rule()
        .expect("rule");
        assert_eq!(
            ok.required_checks
                .iter()
                .map(CheckName::as_str)
                .collect::<Vec<_>>(),
            ["ci/gates", "kanflow/linked"]
        );
        let rule_of = |ask: MergeRuleAsk| {
            let e = ask.to_rule().expect_err("refused");
            assert_eq!(e.code(), ErrorCode::RequestInvalid);
            e.context()
                .iter()
                .find(|(k, _)| *k == "rule")
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        // ADR-0024 §11: no work item link before I1.
        assert_eq!(
            rule_of(MergeRuleAsk {
                require_work_item_link: true,
                ..MergeRuleAsk::default()
            }),
            "unavailable_until_i1"
        );
        assert_eq!(
            rule_of(MergeRuleAsk {
                required_approvals: 21,
                ..MergeRuleAsk::default()
            }),
            "range"
        );
        assert_eq!(
            rule_of(MergeRuleAsk {
                required_checks: (0..21).map(|i| format!("ci/c{i}")).collect(),
                ..MergeRuleAsk::default()
            }),
            "too_many"
        );
        assert_eq!(
            rule_of(MergeRuleAsk {
                required_checks: vec!["gh/actions".to_owned()],
                ..MergeRuleAsk::default()
            }),
            "unknown_namespace"
        );
        assert_eq!(
            rule_of(MergeRuleAsk {
                required_checks: vec!["ci/Gates".to_owned()],
                ..MergeRuleAsk::default()
            }),
            "format"
        );
    }
}
