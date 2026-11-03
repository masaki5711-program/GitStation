//! A project's approval rules (M3 R5, ADR-0024 §8.6): put (create or
//! replace by name) and remove, decided inside their transaction after the
//! project row and the caller are locked. A change is audited on the
//! project (`approval_rule.put` / `approval_rule.removed`, no event) and
//! never touches approvals: it only changes what `evaluate_merge()` reads.
//!
//! U2 follow-up: a put names the rule it read (`expected_updated_at`;
//! `None`: create only) and a rule otherwise as locked is
//! `FG-REVIEW-0018` — nothing is replaced, re-created or overwritten
//! silently. `updated_at` is stamped with `clock_timestamp()` under the
//! project row lock (as the merge settings, ADR-0024 U2) and read to the
//! microsecond.

use std::collections::BTreeMap;

use forge_core::ids::{ApprovalRuleId, GroupId, ProjectId, UserId};
use forge_core::{ApprovalRuleChange, AuditDetails, RefPattern};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{NamedGroup, NamedUser, StoredApprovalRule};
use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::query_error;
use crate::review::commands::{facts, MergeRequestFacts};
use crate::Db;

/// The most rules one project has (`FG-WEB-0006 rule=too_many_rules`).
pub const MAX_RULES: usize = 20;
/// The most users and groups one rule names.
pub const MAX_RULE_APPROVERS: usize = 50;

/// A rule as asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApprovalRule {
    pub name: String,
    pub target_pattern: Option<RefPattern>,
    pub required_approvals: i32,
    pub users: Vec<UserId>,
    pub groups: Vec<GroupId>,
}

/// What a put or a remove did.
#[must_use = "Unchanged wrote nothing; callers report it differently"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOutcome {
    Created,
    Replaced,
    Removed,
    /// The rule was already exactly so: nothing written.
    Unchanged,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i64,
    name: String,
    target_pattern: Option<String>,
    required_approvals: i32,
    updated_at: String,
    user_id: Option<i64>,
    username: Option<String>,
    user_display_name: Option<String>,
    group_id: Option<i64>,
    group_path: Option<String>,
    group_display_name: Option<String>,
}

fn invalid_rule(id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "stored_approval_rule_invalid")
        .with_context("approval_rule_id", id)
}

/// The rules of `project` with their approvers (named), by name: one
/// statement of at most `MAX_RULES × MAX_RULE_APPROVERS` rows.
pub(super) async fn rules_on(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Vec<StoredApprovalRule>, ForgeError> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT r.id, r.name, r.target_pattern, r.required_approvals, \
         to_char(r.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') \
         AS updated_at, \
         a.user_id, u.username, u.display_name AS user_display_name, \
         a.group_id, n.full_path AS group_path, g.display_name AS group_display_name \
         FROM forge.approval_rules r \
         LEFT JOIN forge.approval_rule_approvers a ON a.rule_id = r.id \
         LEFT JOIN forge.users u ON u.id = a.user_id \
         LEFT JOIN forge.groups g ON g.id = a.group_id \
         LEFT JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = a.group_id \
         WHERE r.project_id = $1 ORDER BY r.name, a.id",
    )
    .bind(project.get())
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approval_rules", e))?;
    let mut by_name: BTreeMap<String, StoredApprovalRule> = BTreeMap::new();
    for row in rows {
        let rule = match by_name.get_mut(&row.name) {
            Some(rule) => rule,
            None => {
                let pattern = row
                    .target_pattern
                    .as_deref()
                    .map(RefPattern::parse)
                    .transpose()
                    .map_err(|_| invalid_rule(row.id))?;
                by_name
                    .entry(row.name.clone())
                    .or_insert(StoredApprovalRule {
                        // clone: the map key and the rule both keep the name
                        id: ApprovalRuleId::new(row.id),
                        name: row.name.clone(), // clone: as above
                        target_pattern: pattern,
                        required_approvals: row.required_approvals,
                        users: Vec::new(),
                        groups: Vec::new(),
                        named_users: Vec::new(),
                        named_groups: Vec::new(),
                        updated_at: row.updated_at.clone(), // clone: as above
                    })
            }
        };
        if let Some(u) = row.user_id {
            let (Some(username), Some(display_name)) = (row.username, row.user_display_name) else {
                return Err(invalid_rule(row.id).with_context("field", "user"));
            };
            rule.users.push(UserId::new(u));
            rule.named_users.push(NamedUser {
                id: UserId::new(u),
                username,
                display_name,
            });
        }
        if let Some(g) = row.group_id {
            let (Some(full_path), Some(display_name)) = (row.group_path, row.group_display_name)
            else {
                return Err(invalid_rule(row.id).with_context("field", "group"));
            };
            rule.groups.push(GroupId::new(g));
            rule.named_groups.push(NamedGroup {
                id: GroupId::new(g),
                full_path,
                display_name,
            });
        }
    }
    Ok(by_name.into_values().collect())
}

fn param_invalid(param: &'static str, rule: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("reason", "param_invalid")
        .with_context("param", param)
        .with_context("rule", rule)
}

/// A rule write the database refused for a value the request named (an
/// unknown user or group, a CHECK): `FG-WEB-0006`.
fn write_error(query: &'static str) -> impl FnOnce(sqlx::Error) -> ForgeError {
    move |e| {
        let constraint = e
            .as_database_error()
            .and_then(|d| d.constraint())
            .map(str::to_owned);
        match constraint.as_deref() {
            Some("approval_rule_approvers_user_id_fkey") => param_invalid("users", "unknown_user"),
            Some("approval_rule_approvers_group_id_fkey") => {
                param_invalid("groups", "unknown_group")
            }
            Some("approval_rules_name") => param_invalid("name", "format"),
            Some("approval_rules_required") => param_invalid("required_approvals", "range"),
            Some(_) | None => query_error(query, e),
        }
    }
}

fn audit(project: ProjectId, change: ApprovalRuleChange) -> ChangeRecord {
    let details = AuditDetails::ApprovalRule(change);
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

/// The rule as locked is the one the caller read (U2 follow-up): the same
/// `updated_at`, or no rule for `None` (create only). `FG-REVIEW-0018`
/// otherwise — `rule_exists`, `rule_gone` or `stale_rule`.
fn unchanged_since(
    project: ProjectId,
    name: &str,
    expected: Option<&str>,
    current: Option<&StoredApprovalRule>,
) -> Result<(), ForgeError> {
    let actual = current.map(|r| r.updated_at.as_str());
    if actual == expected {
        return Ok(());
    }
    let reason = match (expected, actual) {
        (None, Some(_)) => "rule_exists",
        (Some(_), None) => "rule_gone",
        _ => "stale_rule",
    };
    Err(ForgeError::new(ErrorCode::ApprovalRuleChanged)
        .with_context("action", "ApprovalRuleManage")
        .with_context("reason", reason)
        .with_context("project_id", project.get())
        .with_context("approval_rule", name.to_owned())
        .with_context("expected", expected.unwrap_or("none").to_owned())
        .with_context("actual", actual.unwrap_or("none").to_owned()))
}

fn same(stored: &StoredApprovalRule, new: &NewApprovalRule) -> bool {
    let sorted = |v: &[i64]| {
        let mut v = v.to_vec();
        v.sort_unstable();
        v
    };
    stored.target_pattern.as_ref().map(RefPattern::as_str)
        == new.target_pattern.as_ref().map(RefPattern::as_str)
        && stored.required_approvals == new.required_approvals
        && sorted(&stored.users.iter().map(|u| u.get()).collect::<Vec<_>>())
            == sorted(&new.users.iter().map(|u| u.get()).collect::<Vec<_>>())
        && sorted(&stored.groups.iter().map(|g| g.get()).collect::<Vec<_>>())
            == sorted(&new.groups.iter().map(|g| g.get()).collect::<Vec<_>>())
}

/// Create rule `new.name` of `project`, or replace it (its approvers
/// entirely), after `decide` on the locked project facts. More than
/// [`MAX_RULES`] rules is `FG-WEB-0006 rule=too_many_rules`; an unknown
/// user or group `rule=unknown_user|unknown_group`. `expected_updated_at`
/// is the rule's `updated_at` the caller read (`None`: no such rule, create
/// only); another rule as locked is `FG-REVIEW-0018` (module docs), decided
/// after `decide` and before anything is compared or written.
pub async fn put_approval_rule_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    new: &NewApprovalRule,
    expected_updated_at: Option<&str>,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(RuleOutcome, StoredApprovalRule, P), ForgeError> {
    const ACTION: &str = "approval_rule.put";
    if new.users.len() + new.groups.len() > MAX_RULE_APPROVERS {
        return Err(param_invalid("users", "too_many_approvers"));
    }
    // R5 review: duplicates are dropped before the comparison and the
    // inserts (the same user named twice is one approver).
    let mut new = new.clone(); // clone: deduplicated locally
    new.users.sort_unstable_by_key(|u| u.get());
    new.users.dedup();
    new.groups.sort_unstable_by_key(|g| g.get());
    new.groups.dedup();
    let new = &new;
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, None, viewer).await?;
        let decided = decide(&facts)?;
        let current = tx
            .step(|c| async move { rules_on(c, project).await })
            .await?;
        let existing = current.iter().find(|r| r.name == new.name);
        unchanged_since(project, &new.name, expected_updated_at, existing)?;
        if let Some(existing) = existing {
            if same(existing, new) {
                return Ok(Settle::Discard((
                    RuleOutcome::Unchanged,
                    existing.clone(),
                    decided,
                ))); // clone: the answer
            }
        } else if current.len() >= MAX_RULES {
            return Err(param_invalid("name", "too_many_rules").with_context("limit", MAX_RULES));
        }
        let existing_id = existing.map(|r| r.id);
        let id: i64 = match existing_id {
            Some(id) => {
                tx.step(|c| async move {
                    sqlx::query("DELETE FROM forge.approval_rule_approvers WHERE rule_id = $1")
                        .bind(id.get())
                        .execute(&mut *c)
                        .await
                        .map_err(|e| query_error("review.approval_rule_clear", e))?;
                    sqlx::query_scalar(
                        "UPDATE forge.approval_rules SET target_pattern = $2, \
                         required_approvals = $3, updated_at = clock_timestamp() \
                         WHERE id = $1 RETURNING id",
                    )
                    .bind(id.get())
                    .bind(new.target_pattern.as_ref().map(RefPattern::as_str))
                    .bind(new.required_approvals)
                    .fetch_one(c)
                    .await
                    .map_err(write_error("review.approval_rule_update"))
                })
                .await?
            }
            None => {
                tx.step(|c| async move {
                    sqlx::query_scalar(
                        "INSERT INTO forge.approval_rules (project_id, name, target_pattern, \
                         required_approvals, created_at, updated_at) \
                         VALUES ($1, $2, $3, $4, clock_timestamp(), clock_timestamp()) \
                         RETURNING id",
                    )
                    .bind(project.get())
                    .bind(&new.name)
                    .bind(new.target_pattern.as_ref().map(RefPattern::as_str))
                    .bind(new.required_approvals)
                    .fetch_one(c)
                    .await
                    .map_err(write_error("review.approval_rule_insert"))
                })
                .await?
            }
        };
        tx.step(|c| async move {
            for user in &new.users {
                sqlx::query(
                    "INSERT INTO forge.approval_rule_approvers (rule_id, project_id, user_id) \
                     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(project.get())
                .bind(user.get())
                .execute(&mut *c)
                .await
                .map_err(write_error("review.approval_rule_user"))?;
            }
            for group in &new.groups {
                sqlx::query(
                    "INSERT INTO forge.approval_rule_approvers (rule_id, project_id, group_id) \
                     VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                )
                .bind(id)
                .bind(project.get())
                .bind(group.get())
                .execute(&mut *c)
                .await
                .map_err(write_error("review.approval_rule_group"))?;
            }
            Ok(())
        })
        .await?;
        let stored = tx
            .step(|c| async move { rules_on(c, project).await })
            .await?
            .into_iter()
            .find(|r| r.id.get() == id)
            .ok_or_else(|| invalid_rule(id))?;
        let record = audit(
            project,
            ApprovalRuleChange::Put {
                rule: stored.id,
                name: stored.name.clone(), // clone: the details own their values
                target_pattern: stored.target_pattern.clone(),
                required_approvals: stored.required_approvals,
                users: stored.users.clone(),
                groups: stored.groups.clone(),
                created: existing_id.is_none(),
            },
        );
        let outcome = if existing_id.is_none() {
            RuleOutcome::Created
        } else {
            RuleOutcome::Replaced
        };
        Ok(Settle::Commit(record, (outcome, stored, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}

/// Remove rule `name` of `project` after `decide`; `FG-REPO-0005
/// resource=approval_rule` when there is none.
pub async fn remove_approval_rule_mr_decided<P>(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    viewer: UserId,
    name: &str,
    decide: impl FnOnce(&MergeRequestFacts) -> Result<P, ForgeError>,
) -> Result<(RuleOutcome, P), ForgeError> {
    const ACTION: &str = "approval_rule.removed";
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let facts = facts(&mut tx, project, None, viewer).await?;
        let decided = decide(&facts)?;
        let current = tx
            .step(|c| async move { rules_on(c, project).await })
            .await?;
        let rule = current
            .into_iter()
            .find(|r| r.name == name)
            .ok_or_else(|| {
                ForgeError::new(ErrorCode::NotFound)
                    .with_context("resource", "approval_rule")
                    .with_context("reason", "absent")
            })?;
        let id = rule.id;
        tx.step(|c| async move {
            sqlx::query("DELETE FROM forge.approval_rule_approvers WHERE rule_id = $1")
                .bind(id.get())
                .execute(&mut *c)
                .await
                .map_err(|e| query_error("review.approval_rule_remove", e))?;
            sqlx::query("DELETE FROM forge.approval_rules WHERE id = $1")
                .bind(id.get())
                .execute(c)
                .await
                .map(drop)
                .map_err(|e| query_error("review.approval_rule_remove", e))
        })
        .await?;
        let record = audit(
            project,
            ApprovalRuleChange::Removed {
                rule: rule.id,
                name: rule.name,
            },
        );
        Ok(Settle::Commit(record, (RuleOutcome::Removed, decided)))
    }
    .await;
    tx.settle(ACTION, outcome).await
}
