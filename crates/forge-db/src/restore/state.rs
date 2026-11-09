//! The authorization state copy (M3 O1, ADR-0019 Amendment plan 11 P1,
//! step 2): what a restore must not widen. `forge worker` reads
//! [`AuthzState`] every `backup.state_copy_interval_secs` and stores it,
//! encrypted, **outside** the backup (forge-cli `state_copy`); `forge restore
//! finish` re-applies it to the restored database with [`reapply`].
//!
//! **Narrowing only.** The copy is newer than the backup, but a copy is a
//! file an attacker might replace, so re-applying it can only take rights
//! away, never give them (fail closed):
//!
//! | what | re-applied when the copy is narrower |
//! |---|---|
//! | users | `state = disabled`; `is_admin = false` |
//! | group / project members | a membership the copy lacks (its group / project is in the copy) is removed; a lower role is set |
//! | protected refs | a rule the copy has is created; a higher `push_min_role` is set |
//! | merge rules of a protected ref | created when missing; `required_approvals` = the larger, `required_checks` = the union (at most 20, the copy's first), each `require_*` / `direct_push_forbidden` = OR |
//! | approval rules | a rule the copy has is created; `required_approvals` = the larger |
//! | merge settings | `reset_approvals_on_push`, `committer_approvals_excluded`, `codeowners_enforced` (its suspension cleared) = OR |
//!
//! A widening after the backup (a member added, a rule removed) is not
//! re-applied: the people repeat it. Each change is one transaction per
//! user / group / project with one `restore.reapplied` audit row on it.

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ids::{ProjectId, UserId};
use forge_errors::ForgeError;
use serde::{Deserialize, Serialize};

use super::{audit, record};
use crate::change::{begin_read_snapshot, ChangeContext, ChangeTx, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit action of a re-applied narrowing (resource user / group /
/// project; `details.kind` says what).
pub const ACTION_REAPPLIED: &str = "restore.reapplied";

/// The format of [`AuthzState`] (a reader refuses another).
pub const FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFlags {
    pub id: i64,
    pub disabled: bool,
    pub is_admin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    /// The group id or the project id.
    pub scope: i64,
    pub user: i64,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRule {
    pub required_approvals: i32,
    pub required_checks: Vec<String>,
    pub require_resolved_discussions: bool,
    pub direct_push_forbidden: bool,
    pub require_work_item_link: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedRef {
    pub project: i64,
    pub pattern: String,
    pub push_min_role: String,
    pub merge_rule: Option<MergeRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRule {
    pub project: i64,
    pub name: String,
    pub target_pattern: Option<String>,
    pub required_approvals: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeSettings {
    pub project: i64,
    pub reset_approvals_on_push: bool,
    pub committer_approvals_excluded: bool,
    pub codeowners_enforced: bool,
}

/// The authorization state at `taken_at_micros` (the database clock, as
/// [`super::clock`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthzState {
    pub format: u32,
    pub taken_at_micros: i64,
    pub taken_at: String,
    pub users: Vec<UserFlags>,
    pub groups: Vec<i64>,
    pub projects: Vec<i64>,
    pub group_members: Vec<Membership>,
    pub project_members: Vec<Membership>,
    pub protected_refs: Vec<ProtectedRef>,
    pub approval_rules: Vec<ApprovalRule>,
    pub merge_settings: Vec<MergeSettings>,
}

/// A protected ref joined with its merge rule (`None`s without one): id or
/// project, pattern, push role, approvals, checks, discussions, direct push,
/// work item link.
type RefRow = (
    i64,
    String,
    String,
    Option<i32>,
    Option<Vec<String>>,
    Option<bool>,
    Option<bool>,
    Option<bool>,
);

type MergeRuleRow = (
    i64,
    String,
    String,
    Option<(i32, Vec<String>, bool, bool, bool)>,
);

/// Read the state in one `REPEATABLE READ READ ONLY` snapshot of the read
/// pool (every column it reads is in the read role's ledger).
pub async fn read(db: &Db) -> Result<AuthzState, ForgeError> {
    const QUERY: &str = "restore.state.read";
    let mut tx = begin_read_snapshot(db, QUERY).await?;
    let read = db
        .within(QUERY, async {
            let q = |e| read_query_error(QUERY, e);
            let (taken_at_micros, taken_at): (i64, String) = sqlx::query_as(
                "SELECT (extract(epoch FROM now()) * 1000000)::bigint, \
                 to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')",
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(q)?;
            let users: Vec<(i64, String, bool)> =
                sqlx::query_as("SELECT id, state, is_admin FROM forge.users ORDER BY id")
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(q)?;
            let groups: Vec<i64> = sqlx::query_scalar("SELECT id FROM forge.groups ORDER BY id")
                .fetch_all(&mut *tx)
                .await
                .map_err(q)?;
            let projects: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM forge.projects ORDER BY id")
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(q)?;
            let group_members: Vec<(i64, i64, String)> = sqlx::query_as(
                "SELECT group_id, user_id, role FROM forge.group_members ORDER BY 1, 2",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(q)?;
            let project_members: Vec<(i64, i64, String)> = sqlx::query_as(
                "SELECT project_id, user_id, role FROM forge.project_members ORDER BY 1, 2",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(q)?;
            let refs: Vec<RefRow> = sqlx::query_as(
                "SELECT p.project_id, p.pattern, p.push_min_role, m.required_approvals, \
                     m.required_checks, m.require_resolved_discussions, m.direct_push_forbidden, \
                     m.require_work_item_link \
                     FROM forge.protected_refs p \
                     LEFT JOIN forge.protected_ref_merge_rules m ON m.protected_ref_id = p.id \
                     ORDER BY p.project_id, p.id",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(q)?;
            let approval_rules: Vec<(i64, String, Option<String>, i32)> = sqlx::query_as(
                "SELECT project_id, name, target_pattern, required_approvals \
                 FROM forge.approval_rules ORDER BY project_id, name",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(q)?;
            let merge_settings: Vec<(i64, bool, bool, bool)> = sqlx::query_as(
                "SELECT project_id, reset_approvals_on_push, committer_approvals_excluded, \
                 codeowners_enforced FROM forge.merge_settings ORDER BY project_id",
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(q)?;
            let membership =
                |(scope, user, role): (i64, i64, String)| Membership { scope, user, role };
            Ok(AuthzState {
                format: FORMAT,
                taken_at_micros,
                taken_at,
                users: users
                    .into_iter()
                    .map(|(id, state, is_admin)| UserFlags {
                        id,
                        disabled: state == "disabled",
                        is_admin,
                    })
                    .collect(),
                groups,
                projects,
                group_members: group_members.into_iter().map(membership).collect(),
                project_members: project_members.into_iter().map(membership).collect(),
                protected_refs: refs
                    .into_iter()
                    .map(
                        |(project, pattern, push_min_role, a, c, d, f, w)| ProtectedRef {
                            project,
                            pattern,
                            push_min_role,
                            merge_rule: match (a, c, d, f, w) {
                                (Some(a), Some(c), Some(d), Some(f), Some(w)) => Some(MergeRule {
                                    required_approvals: a,
                                    required_checks: c,
                                    require_resolved_discussions: d,
                                    direct_push_forbidden: f,
                                    require_work_item_link: w,
                                }),
                                _ => None,
                            },
                        },
                    )
                    .collect(),
                approval_rules: approval_rules
                    .into_iter()
                    .map(
                        |(project, name, target_pattern, required_approvals)| ApprovalRule {
                            project,
                            name,
                            target_pattern,
                            required_approvals,
                        },
                    )
                    .collect(),
                merge_settings: merge_settings
                    .into_iter()
                    .map(|(project, r, c, o)| MergeSettings {
                        project,
                        reset_approvals_on_push: r,
                        committer_approvals_excluded: c,
                        codeowners_enforced: o,
                    })
                    .collect(),
            })
        })
        .await;
    // Read only: ending it can lose nothing, but a failed rollback is said.
    if let Err(e) = tx.rollback().await {
        let err = read_query_error(QUERY, e).with_context("operation", "rollback");
        tracing::warn!(
            target: "forge_db",
            error_code = %err.code(),
            context = ?err.context(),
            "forge.restore.state_read_rollback_failed"
        );
    }
    read
}

/// The rank of a role or a `push_min_role` (unknown: the highest, so an
/// unknown value is never lowered to).
fn rank(role: &str) -> u8 {
    match role {
        "guest" => 0,
        "reporter" => 1,
        "developer" => 2,
        "maintainer" => 3,
        "owner" => 4,
        _ => 5,
    }
}

/// What [`reapply`] changed (counts of rows).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reapplied {
    pub users_disabled: i64,
    pub admin_flags_removed: i64,
    pub members_removed: i64,
    pub roles_lowered: i64,
    pub protected_refs_created: i64,
    pub push_roles_raised: i64,
    pub merge_rules_tightened: i64,
    pub approval_rules_tightened: i64,
    pub merge_settings_tightened: i64,
}

impl Reapplied {
    pub fn total(&self) -> i64 {
        self.users_disabled
            + self.admin_flags_removed
            + self.members_removed
            + self.roles_lowered
            + self.protected_refs_created
            + self.push_roles_raised
            + self.merge_rules_tightened
            + self.approval_rules_tightened
            + self.merge_settings_tightened
    }
}

/// Re-apply the narrowings of `copy` to the database (module docs). The
/// current state is read first (read pool); each user / group / project
/// that needs a change is changed in its own transaction, which reads its
/// rows again under lock, so a concurrent writer (there should be none:
/// forge serve and worker are stopped) cannot be overwritten.
pub async fn reapply(
    db: &Db,
    ctx: &ChangeContext,
    copy: &AuthzState,
) -> Result<Reapplied, ForgeError> {
    let now = read(db).await?;
    let mut done = Reapplied::default();

    // Users.
    let current: BTreeMap<i64, &UserFlags> = now.users.iter().map(|u| (u.id, u)).collect();
    for want in &copy.users {
        let Some(have) = current.get(&want.id) else {
            continue;
        };
        let disable = want.disabled && !have.disabled;
        let unadmin = !want.is_admin && have.is_admin;
        if disable || unadmin {
            let (d, a) = narrow_user(db, ctx, UserId::new(want.id), disable, unadmin).await?;
            done.users_disabled += d;
            done.admin_flags_removed += a;
        }
    }

    // Memberships.
    for (table, scopes, want, have) in [
        (
            Scope::Group,
            &copy.groups,
            &copy.group_members,
            &now.group_members,
        ),
        (
            Scope::Project,
            &copy.projects,
            &copy.project_members,
            &now.project_members,
        ),
    ] {
        let known: BTreeSet<i64> = scopes.iter().copied().collect();
        let want: BTreeMap<(i64, i64), &str> = want
            .iter()
            .map(|m| ((m.scope, m.user), m.role.as_str()))
            .collect();
        let mut per_scope: BTreeMap<i64, Vec<MemberChange>> = BTreeMap::new();
        for m in have {
            if !known.contains(&m.scope) {
                continue;
            }
            match want.get(&(m.scope, m.user)) {
                None => per_scope.entry(m.scope).or_default().push(MemberChange {
                    user: m.user,
                    to: None,
                }),
                Some(role) if rank(role) < rank(&m.role) => {
                    per_scope.entry(m.scope).or_default().push(MemberChange {
                        user: m.user,
                        to: Some((*role).to_owned()),
                    })
                }
                Some(_) => {}
            }
        }
        for (scope, changes) in per_scope {
            let (removed, lowered) = narrow_members(db, ctx, table, scope, &changes).await?;
            done.members_removed += removed;
            done.roles_lowered += lowered;
        }
    }

    // Project rules.
    let projects: BTreeSet<i64> = now.projects.iter().copied().collect();
    let mut wanted: BTreeMap<i64, ProjectRules> = BTreeMap::new();
    for r in &copy.protected_refs {
        wanted.entry(r.project).or_default().refs.push(r.clone());
    }
    for r in &copy.approval_rules {
        wanted
            .entry(r.project)
            .or_default()
            .approvals
            .push(r.clone());
    }
    for s in &copy.merge_settings {
        wanted.entry(s.project).or_default().settings = Some(s.clone());
    }
    for (project, rules) in wanted {
        if !projects.contains(&project) {
            continue;
        }
        let n = narrow_project(db, ctx, ProjectId::new(project), &rules).await?;
        done.protected_refs_created += n.protected_refs_created;
        done.push_roles_raised += n.push_roles_raised;
        done.merge_rules_tightened += n.merge_rules_tightened;
        done.approval_rules_tightened += n.approval_rules_tightened;
        done.merge_settings_tightened += n.merge_settings_tightened;
    }
    Ok(done)
}

#[derive(Debug, Clone, Copy)]
enum Scope {
    Group,
    Project,
}

impl Scope {
    fn resource(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
struct MemberChange {
    user: i64,
    /// `None`: remove; `Some(role)`: lower to it.
    to: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ProjectRules {
    refs: Vec<ProtectedRef>,
    approvals: Vec<ApprovalRule>,
    settings: Option<MergeSettings>,
}

async fn narrow_user(
    db: &Db,
    ctx: &ChangeContext,
    user: UserId,
    disable: bool,
    unadmin: bool,
) -> Result<(i64, i64), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        tx.step(crate::users::admin::lock_admin_set).await?;
        let disabled: u64 = if disable {
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.users SET state = 'disabled', updated_at = now() \
                     WHERE id = $1 AND state = 'active'",
                )
                .bind(user.get())
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error("restore.reapply.disable_user", e))
            })
            .await?
        } else {
            0
        };
        let unadmined: u64 = if unadmin {
            tx.step(|c| async move {
                sqlx::query(
                    "UPDATE forge.users SET is_admin = false, updated_at = now() \
                     WHERE id = $1 AND is_admin",
                )
                .bind(user.get())
                .execute(c)
                .await
                .map(|r| r.rows_affected())
                .map_err(|e| query_error("restore.reapply.unadmin_user", e))
            })
            .await?
        } else {
            0
        };
        if disabled + unadmined == 0 {
            return Ok(Settle::Discard((0, 0)));
        }
        let entry = audit(
            ACTION_REAPPLIED,
            "user",
            Some(user.get()),
            None,
            serde_json::json!({
                "kind": "user",
                "disabled": disabled > 0,
                "admin_flag_removed": unadmined > 0,
            }),
        );
        let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
        Ok(Settle::Commit(record(entry), (n(disabled), n(unadmined))))
    }
    .await;
    tx.settle(ACTION_REAPPLIED, outcome).await
}

async fn narrow_members(
    db: &Db,
    ctx: &ChangeContext,
    scope: Scope,
    id: i64,
    changes: &[MemberChange],
) -> Result<(i64, i64), ForgeError> {
    let (delete, update) = match scope {
        Scope::Group => (
            "DELETE FROM forge.group_members WHERE group_id = $1 AND user_id = $2",
            "UPDATE forge.group_members SET role = $3, updated_at = now() \
             WHERE group_id = $1 AND user_id = $2 AND role <> $3",
        ),
        Scope::Project => (
            "DELETE FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
            "UPDATE forge.project_members SET role = $3, updated_at = now() \
             WHERE project_id = $1 AND user_id = $2 AND role <> $3",
        ),
    };
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (mut removed, mut lowered) = (Vec::new(), Vec::new());
        for change in changes {
            let user = change.user;
            match &change.to {
                None => {
                    let n = tx
                        .step(|c| async move {
                            sqlx::query(delete)
                                .bind(id)
                                .bind(user)
                                .execute(c)
                                .await
                                .map(|r| r.rows_affected())
                                .map_err(|e| query_error("restore.reapply.remove_member", e))
                        })
                        .await?;
                    if n > 0 {
                        removed.push(user);
                    }
                }
                Some(role) => {
                    let role = role.clone(); // clone: moved into the step
                    let n = tx
                        .step(|c| async move {
                            sqlx::query(update)
                                .bind(id)
                                .bind(user)
                                .bind(role)
                                .execute(c)
                                .await
                                .map(|r| r.rows_affected())
                                .map_err(|e| query_error("restore.reapply.lower_role", e))
                        })
                        .await?;
                    if n > 0 {
                        lowered.push(user);
                    }
                }
            }
        }
        if removed.is_empty() && lowered.is_empty() {
            return Ok(Settle::Discard((0, 0)));
        }
        let project = match scope {
            Scope::Project => Some(ProjectId::new(id)),
            Scope::Group => None,
        };
        let entry = audit(
            ACTION_REAPPLIED,
            scope.resource(),
            Some(id),
            project,
            serde_json::json!({
                "kind": "members",
                "removed_user_ids": removed,
                "lowered_user_ids": lowered,
            }),
        );
        let n = |v: &Vec<i64>| i64::try_from(v.len()).unwrap_or(i64::MAX);
        Ok(Settle::Commit(record(entry), (n(&removed), n(&lowered))))
    }
    .await;
    tx.settle(ACTION_REAPPLIED, outcome).await
}

/// The project's protected refs, merge rules, approval rules and merge
/// settings tightened to `want` (module docs), in one change.
async fn narrow_project(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    want: &ProjectRules,
) -> Result<Reapplied, ForgeError> {
    let p = project.get();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let mut done = Reapplied::default();
        // The project row first (the order every rule writer takes).
        tx.step(|c| async move {
            sqlx::query("SELECT 1 FROM forge.projects WHERE id = $1 FOR UPDATE")
                .bind(p)
                .execute(c)
                .await
                .map_err(|e| query_error("restore.reapply.lock_project", e))
        })
        .await?;
        let have: Vec<MergeRuleRow> = {
            let rows: Vec<RefRow> = tx
                .step(|c| async move {
                    sqlx::query_as(
                        "SELECT p.id, lower(p.pattern), p.push_min_role, m.required_approvals, \
                         m.required_checks, m.require_resolved_discussions, \
                         m.direct_push_forbidden, m.require_work_item_link \
                         FROM forge.protected_refs p \
                         LEFT JOIN forge.protected_ref_merge_rules m ON m.protected_ref_id = p.id \
                         WHERE p.project_id = $1 FOR UPDATE OF p",
                    )
                    .bind(p)
                    .fetch_all(c)
                    .await
                    .map_err(|e| query_error("restore.reapply.protected_refs", e))
                })
                .await?;
            rows.into_iter()
                .map(|(id, pat, role, a, c, d, f, w)| {
                    let rule = match (a, c, d, f, w) {
                        (Some(a), Some(c), Some(d), Some(f), Some(w)) => Some((a, c, d, f, w)),
                        _ => None,
                    };
                    (id, pat, role, rule)
                })
                .collect()
        };
        let by_pattern: BTreeMap<&str, &MergeRuleRow> =
            have.iter().map(|r| (r.1.as_str(), r)).collect();
        for want_ref in &want.refs {
            let key = want_ref.pattern.to_lowercase();
            let (ref_id, current_rule) = match by_pattern.get(key.as_str()) {
                Some((id, _, role, rule)) => {
                    if rank(&want_ref.push_min_role) > rank(role) {
                        let (id, to) = (*id, want_ref.push_min_role.clone()); // clone: moved into the step
                        tx.step(|c| async move {
                            sqlx::query(
                                "UPDATE forge.protected_refs SET push_min_role = $2, \
                                 updated_at = now() WHERE id = $1",
                            )
                            .bind(id)
                            .bind(to)
                            .execute(c)
                            .await
                            .map_err(|e| query_error("restore.reapply.raise_push_role", e))
                        })
                        .await?;
                        done.push_roles_raised += 1;
                    }
                    (*id, rule.clone()) // clone: compared below
                }
                None => {
                    let (pattern, role) =
                        (want_ref.pattern.clone(), want_ref.push_min_role.clone()); // clone: moved into the step
                    let id: i64 = tx
                        .step(|c| async move {
                            sqlx::query_scalar(
                                "INSERT INTO forge.protected_refs (project_id, pattern, \
                                 push_min_role) VALUES ($1, $2, $3) RETURNING id",
                            )
                            .bind(p)
                            .bind(pattern)
                            .bind(role)
                            .fetch_one(c)
                            .await
                            .map_err(|e| query_error("restore.reapply.create_protected_ref", e))
                        })
                        .await?;
                    done.protected_refs_created += 1;
                    (id, None)
                }
            };
            let Some(want_rule) = &want_ref.merge_rule else {
                continue;
            };
            let tightened = match current_rule {
                None => Some(want_rule.clone()), // clone: written below
                Some((a, checks, d, f, w)) => {
                    let mut union: Vec<String> = want_rule.required_checks.clone(); // clone: extended below
                    for c in &checks {
                        if union.len() < 20 && !union.contains(c) {
                            union.push(c.clone()); // clone: one more check name
                        }
                    }
                    let next = MergeRule {
                        required_approvals: a.max(want_rule.required_approvals),
                        required_checks: union,
                        require_resolved_discussions: d || want_rule.require_resolved_discussions,
                        direct_push_forbidden: f || want_rule.direct_push_forbidden,
                        require_work_item_link: w || want_rule.require_work_item_link,
                    };
                    let unchanged = next.required_approvals == a
                        && next.required_checks.len() == checks.len()
                        && next.required_checks.iter().all(|c| checks.contains(c))
                        && next.require_resolved_discussions == d
                        && next.direct_push_forbidden == f
                        && next.require_work_item_link == w;
                    (!unchanged).then_some(next)
                }
            };
            if let Some(rule) = tightened {
                tx.step(|c| async move {
                    sqlx::query(
                        "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, \
                         project_id, required_approvals, required_checks, \
                         require_resolved_discussions, direct_push_forbidden, \
                         require_work_item_link) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                         ON CONFLICT (protected_ref_id) DO UPDATE SET \
                         required_approvals = EXCLUDED.required_approvals, \
                         required_checks = EXCLUDED.required_checks, \
                         require_resolved_discussions = EXCLUDED.require_resolved_discussions, \
                         direct_push_forbidden = EXCLUDED.direct_push_forbidden, \
                         require_work_item_link = EXCLUDED.require_work_item_link, \
                         updated_at = now()",
                    )
                    .bind(ref_id)
                    .bind(p)
                    .bind(rule.required_approvals)
                    .bind(rule.required_checks)
                    .bind(rule.require_resolved_discussions)
                    .bind(rule.direct_push_forbidden)
                    .bind(rule.require_work_item_link)
                    .execute(c)
                    .await
                    .map_err(|e| query_error("restore.reapply.merge_rule", e))
                })
                .await?;
                done.merge_rules_tightened += 1;
            }
        }
        for want_rule in &want.approvals {
            let (name, pattern, required) = (
                want_rule.name.clone(),           // clone: moved into the step
                want_rule.target_pattern.clone(), // clone: moved into the step
                want_rule.required_approvals,
            );
            let n = tx
                .step(|c| async move {
                    sqlx::query(
                        "INSERT INTO forge.approval_rules (project_id, name, target_pattern, \
                         required_approvals) VALUES ($1, $2, $3, $4) \
                         ON CONFLICT (project_id, name) DO UPDATE SET \
                         required_approvals = EXCLUDED.required_approvals, updated_at = now() \
                         WHERE forge.approval_rules.required_approvals < \
                         EXCLUDED.required_approvals",
                    )
                    .bind(p)
                    .bind(name)
                    .bind(pattern)
                    .bind(required)
                    .execute(c)
                    .await
                    .map(|r| r.rows_affected())
                    .map_err(|e| query_error("restore.reapply.approval_rule", e))
                })
                .await?;
            done.approval_rules_tightened += i64::try_from(n).unwrap_or(i64::MAX);
        }
        if let Some(s) = &want.settings {
            let (r, c2, o) = (
                s.reset_approvals_on_push,
                s.committer_approvals_excluded,
                s.codeowners_enforced,
            );
            let n = tx
                .step(|c| async move {
                    sqlx::query(
                        "UPDATE forge.merge_settings SET \
                         reset_approvals_on_push = reset_approvals_on_push OR $2, \
                         committer_approvals_excluded = committer_approvals_excluded OR $3, \
                         codeowners_enforced = codeowners_enforced OR $4, \
                         codeowners_suspended_at = CASE WHEN $4 THEN NULL \
                             ELSE codeowners_suspended_at END, \
                         codeowners_enforced_off_until = CASE WHEN $4 THEN NULL \
                             ELSE codeowners_enforced_off_until END, \
                         updated_at = now() \
                         WHERE project_id = $1 AND ((NOT reset_approvals_on_push AND $2) \
                             OR (NOT committer_approvals_excluded AND $3) \
                             OR (NOT codeowners_enforced AND $4))",
                    )
                    .bind(p)
                    .bind(r)
                    .bind(c2)
                    .bind(o)
                    .execute(c)
                    .await
                    .map(|r| r.rows_affected())
                    .map_err(|e| query_error("restore.reapply.merge_settings", e))
                })
                .await?;
            done.merge_settings_tightened += i64::try_from(n).unwrap_or(i64::MAX);
        }
        if done.total() == 0 {
            return Ok(Settle::Discard(done));
        }
        let entry = audit(
            ACTION_REAPPLIED,
            "project",
            Some(p),
            Some(project),
            serde_json::json!({
                "kind": "project_rules",
                "protected_refs_created": done.protected_refs_created,
                "push_roles_raised": done.push_roles_raised,
                "merge_rules_tightened": done.merge_rules_tightened,
                "approval_rules_tightened": done.approval_rules_tightened,
                "merge_settings_tightened": done.merge_settings_tightened,
            }),
        );
        Ok(Settle::Commit(record(entry), done))
    }
    .await;
    tx.settle(ACTION_REAPPLIED, outcome).await
}
