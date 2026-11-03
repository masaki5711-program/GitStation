//! The merge rule of a protection rule (M3 R6, ADR-0024 §7, §8.6, §12;
//! `protected_ref_merge_rules`): the minimum approvals, the required
//! check names, resolved discussions and the direct-push ban of the
//! branches the rule protects. Managed with `ProtectedRefManage` (the
//! same decision as the rule itself, `above_own_role` included): put and
//! remove, each one `ChangeTx` that locks the project, the rule and the
//! caller (`protection_facts`), decides with `decide`, and writes one
//! audit row on the project (`protected_ref.merge_rule_put` /
//! `.merge_rule_removed`, audit only). A rule's deletion deletes its merge
//! rule in the same change ([`delete_with_rule`]).

use forge_core::ids::{ProjectId, ProtectedRefId};
use forge_core::merge_conditions::{MAX_REQUIRED_APPROVALS, MAX_REQUIRED_CHECKS};
use forge_core::{AuditDetails, CheckName, MergeRule, MergeRuleChange};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{absent_rule, parse_row, protection_facts, rule_invalid, ProtectionFacts, RuleState};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, Db};

/// What a put or a remove did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeRuleOutcome {
    Created,
    Replaced,
    Removed,
    /// The merge rule was already exactly so: nothing written.
    Unchanged,
}

fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::ProtectedRefDataInvalid)
        .with_context("reason", reason)
        .with_context("protected_ref_id", id)
}

/// A stored merge rule's columns (all `None`: the rule has none). A row
/// this build cannot read is `FG-DB-0020` (never skipped: skipping a
/// required check would loosen the branch).
pub(super) fn parse_merge(
    id: i64,
    approvals: Option<i32>,
    checks: Option<Vec<String>>,
    discussions: Option<bool>,
    no_push: Option<bool>,
) -> Result<Option<MergeRule>, ForgeError> {
    match (approvals, checks, discussions, no_push) {
        (None, None, None, None) => Ok(None),
        (Some(approvals), Some(checks), Some(discussions), Some(no_push)) => {
            let required_approvals = u32::try_from(approvals)
                .ok()
                .filter(|n| *n <= MAX_REQUIRED_APPROVALS)
                .ok_or_else(|| stored_invalid("stored_merge_rule_approvals_invalid", id))?;
            let mut required_checks = checks
                .iter()
                .map(|c| {
                    CheckName::parse(c)
                        .ok()
                        .filter(|n| n.namespace().may_be_required())
                        .ok_or_else(|| stored_invalid("stored_merge_rule_check_invalid", id))
                })
                .collect::<Result<Vec<_>, _>>()?;
            required_checks.sort();
            required_checks.dedup();
            Ok(Some(MergeRule {
                required_approvals,
                required_checks,
                require_resolved_discussions: discussions,
                direct_push_forbidden: no_push,
            }))
        }
        _ => Err(stored_invalid("stored_merge_rule_invalid", id)),
    }
}

/// Delete the merge rule of `rule` (inside the change deleting the rule).
pub(super) async fn delete_with_rule(
    conn: &mut PgConnection,
    rule: ProtectedRefId,
) -> Result<(), ForgeError> {
    sqlx::query("DELETE FROM forge.protected_ref_merge_rules WHERE protected_ref_id = $1")
        .bind(rule.get())
        .execute(conn)
        .await
        .map(drop)
        .map_err(|e| query_error("protected_refs.merge_rule_delete", e))
}

fn audit(project: ProjectId, change: MergeRuleChange) -> ChangeRecord {
    let details = AuditDetails::ProtectedRefMergeRule(change);
    ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: details.to_value(),
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

/// The merge rule of `rule` as locked (`None`: none).
async fn merge_rule_on(
    conn: &mut PgConnection,
    rule: ProtectedRefId,
) -> Result<Option<MergeRule>, ForgeError> {
    type Row = (i32, Vec<String>, bool, bool);
    let row: Option<Row> = sqlx::query_as(
        "SELECT required_approvals, required_checks, require_resolved_discussions, \
         direct_push_forbidden FROM forge.protected_ref_merge_rules \
         WHERE protected_ref_id = $1 FOR UPDATE",
    )
    .bind(rule.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("protected_refs.merge_rule_lock", e))?;
    match row {
        None => Ok(None),
        Some((a, c, d, p)) => parse_merge(rule.get(), Some(a), Some(c), Some(d), Some(p)),
    }
}

/// The shape checks forge-hosting made, held again here (the table's
/// CHECKs are the last backstop): at most 20 approvals and 20 checks,
/// never an `ext/` check (`FG-REVIEW-0016`).
fn check(merge: &MergeRule) -> Result<(), ForgeError> {
    if merge.required_approvals > MAX_REQUIRED_APPROVALS {
        return Err(rule_invalid("merge_rule_required_approvals"));
    }
    if merge.required_checks.len() > MAX_REQUIRED_CHECKS {
        return Err(rule_invalid("merge_rule_too_many_checks"));
    }
    if let Some(name) = merge
        .required_checks
        .iter()
        .find(|c| !c.namespace().may_be_required())
    {
        return Err(ForgeError::new(ErrorCode::RequiredCheckNamespaceNotAllowed)
            .with_context("reason", "namespace_not_allowed")
            .with_context("namespace", name.namespace().as_str())
            .with_context("check", name.as_str().to_owned()));
    }
    Ok(())
}

/// Put `merge` as the merge rule of rule `rule` of `project` for `viewer`
/// after `decide` (forge-hosting: `ProtectedRefManage` with the rule's
/// level as both `from` and `to`). After it: `FG-REPO-0005
/// resource=protected_ref` for an absent rule, `FG-DB-0020` for a rule this
/// build cannot read, `FG-REPO-0013 merge_rule_on_tag` for a tag pattern
/// (merge requests target branches only), the shape checks.
pub async fn put_merge_rule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    rule: ProtectedRefId,
    viewer: forge_core::UserId,
    merge: &MergeRule,
    decide: impl FnOnce(&ProtectionFacts) -> Result<P, ForgeError>,
) -> Result<(MergeRuleOutcome, P), ForgeError> {
    const ACTION: &str = "protected_ref.merge_rule_put";
    let mut merge = merge.clone(); // clone: normalized locally
    merge.required_checks.sort();
    merge.required_checks.dedup();
    let merge = &merge;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (facts, row) = protection_facts(&mut tx, project, Some(rule), viewer).await?;
        let decided = decide(&facts)?;
        let Some(row) = row else {
            return Err(absent_rule());
        };
        if facts.rule == RuleState::Unreadable {
            return Err(stored_invalid("stored_rule_unreadable", rule.get()));
        }
        let stored = parse_row(row)?;
        if !stored.pattern.as_str().starts_with("refs/heads/") {
            return Err(rule_invalid("merge_rule_on_tag"));
        }
        check(merge)?;
        let current = tx.step(|c| merge_rule_on(c, rule)).await?;
        if current.as_ref() == Some(merge) {
            return Ok(Settle::Discard((MergeRuleOutcome::Unchanged, decided)));
        }
        let checks: Vec<&str> = merge
            .required_checks
            .iter()
            .map(CheckName::as_str)
            .collect();
        let approvals = i32::try_from(merge.required_approvals)
            .map_err(|_| rule_invalid("merge_rule_required_approvals"))?;
        tx.step(|c| async move {
            sqlx::query(
                "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
                 required_approvals, required_checks, require_resolved_discussions, \
                 direct_push_forbidden) VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (protected_ref_id) DO UPDATE SET \
                 required_approvals = EXCLUDED.required_approvals, \
                 required_checks = EXCLUDED.required_checks, \
                 require_resolved_discussions = EXCLUDED.require_resolved_discussions, \
                 direct_push_forbidden = EXCLUDED.direct_push_forbidden, updated_at = now()",
            )
            .bind(rule.get())
            .bind(project.get())
            .bind(approvals)
            .bind(&checks)
            .bind(merge.require_resolved_discussions)
            .bind(merge.direct_push_forbidden)
            .execute(c)
            .await
            .map(drop)
            .map_err(|e| query_error("protected_refs.merge_rule_put", e))
        })
        .await?;
        let created = current.is_none();
        let record = audit(
            project,
            MergeRuleChange::Put {
                rule,
                pattern: stored.pattern,
                merge_rule: merge.clone(), // clone: the details own their values
                created,
            },
        );
        let outcome = if created {
            MergeRuleOutcome::Created
        } else {
            MergeRuleOutcome::Replaced
        };
        Ok(Settle::Commit(record, (outcome, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Remove the merge rule of rule `rule` of `project` after `decide`;
/// `FG-REPO-0005 resource=protected_ref` for an absent rule,
/// `resource=merge_rule` when the rule has no merge rule.
pub async fn remove_merge_rule_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    rule: ProtectedRefId,
    viewer: forge_core::UserId,
    decide: impl FnOnce(&ProtectionFacts) -> Result<P, ForgeError>,
) -> Result<(MergeRuleOutcome, P), ForgeError> {
    const ACTION: &str = "protected_ref.merge_rule_removed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (facts, row) = protection_facts(&mut tx, project, Some(rule), viewer).await?;
        let decided = decide(&facts)?;
        let Some((_, pattern, _)) = row else {
            return Err(absent_rule());
        };
        let found: Option<i64> = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "DELETE FROM forge.protected_ref_merge_rules WHERE protected_ref_id = $1 \
                     RETURNING protected_ref_id",
                )
                .bind(rule.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("protected_refs.merge_rule_remove", e))
            })
            .await?;
        if found.is_none() {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_rule")
                .with_context("reason", "absent"));
        }
        let pattern = forge_core::RefPattern::parse(&pattern)
            .map_err(|_| stored_invalid("stored_pattern_invalid", rule.get()))?;
        let record = audit(project, MergeRuleChange::Removed { rule, pattern });
        Ok(Settle::Commit(record, (MergeRuleOutcome::Removed, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
