//! Approvals of merge requests and approval rules (M3 R5, ADR-0024 §8;
//! plan 11 §4.1 R5). The SQL only: what counts is forge-review's
//! (`evaluate_merge()` at evaluation time).
//!
//! - Reads: [`approval_facts_on`] (inside a merge decision's transaction:
//!   the live approvals with each approver's facts **as they are now** —
//!   active, kind, role on the project, direct groups — and the project's
//!   rules), [`approval_facts`] (the same on the read pool, the approval
//!   state route), [`approval_rules`], [`range_pushers`] (the pushers of a range's commits
//!   from `push_commits`, ADR-0024 §8.3) and [`owners`] (the
//!   users and groups a CODEOWNERS file names).
//! - Commands (`commands.rs`): [`approve_mr_decided`] /
//!   [`unapprove_mr_decided`], decided inside their transaction like every
//!   review command; [`revoke_on`], the revocation the version job (a new
//!   head) and the edit command (a new target) run in their own
//!   transaction (one `merge_request.unapproved` per approval).
//! - Rules (`rules.rs`): [`put_approval_rule_mr_decided`],
//!   [`remove_approval_rule_mr_decided`] (`approval_rule.put` /
//!   `.removed`, audit only).

use std::collections::{BTreeMap, BTreeSet};

use forge_core::ids::{ApprovalId, ApprovalRuleId, GroupId, MergeRequestId, ProjectId, UserId};
use forge_core::{ObjectId, RefName, RefPattern, RepoId, Role, UnapproveReason, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db};

mod commands;
mod rules;

pub use commands::{approve_mr_decided, unapprove_mr_decided, ApproveOutcome, Classify};
pub(crate) use commands::{carry_some_on, revoke_on};
pub use rules::{
    put_approval_rule_mr_decided, remove_approval_rule_mr_decided, NewApprovalRule, RuleOutcome,
    MAX_RULES, MAX_RULE_APPROVERS,
};

/// One approval as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApproval {
    pub id: ApprovalId,
    pub merge_request: MergeRequestId,
    pub approver: UserId,
    /// The head and target the approver saw (ADR-0024 §8.1).
    pub head_sha: ObjectId,
    pub target_branch: RefName,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub revoked_reason: Option<UnapproveReason>,
}

/// A live approval and its approver as they are now (ADR-0024 §8.6: the
/// approver's validity is read again at every evaluation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproverFacts {
    pub approval: StoredApproval,
    pub username: String,
    /// Active (not disabled or locked) now.
    pub active: bool,
    pub kind: UserKind,
    /// The native role on the project now (`None`: none).
    pub role: Option<Role>,
    /// The groups the approver is a direct member of now.
    pub groups: BTreeSet<GroupId>,
}

/// One approval rule of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApprovalRule {
    pub id: ApprovalRuleId,
    pub name: String,
    /// `None`: every target.
    pub target_pattern: Option<RefPattern>,
    pub required_approvals: i32,
    /// Who may approve for it (both empty: any eligible person).
    pub users: Vec<UserId>,
    pub groups: Vec<GroupId>,
    /// UTC, RFC 3339 to the microsecond: the token a rule write's
    /// `expected_updated_at` compares (U2 follow-up, FG-REVIEW-0018).
    pub updated_at: String,
}

/// An approver user of a rule, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedUser {
    pub id: UserId,
    pub username: String,
    pub display_name: String,
}

/// An approver group of a rule, named (its full path is what a rule write
/// names it by).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedGroup {
    pub id: GroupId,
    pub full_path: String,
    pub display_name: String,
}

/// The names of approvers (U2 follow-up): for the rule answers only, never
/// for a merge decision. An id without a name (a group whose namespace row
/// is missing) is absent from the map.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApproverNames {
    pub users: BTreeMap<UserId, NamedUser>,
    pub groups: BTreeMap<GroupId, NamedGroup>,
}

/// The names of `users` and `groups` now (read pool, two bounded
/// statements: at most `MAX_RULES × MAX_RULE_APPROVERS` ids).
pub async fn approver_names(
    db: &Db,
    users: &[UserId],
    groups: &[GroupId],
) -> Result<ApproverNames, ForgeError> {
    const QUERY: &str = "review.approver_names";
    let user_ids: Vec<i64> = users.iter().map(|u| u.get()).collect();
    let group_ids: Vec<i64> = groups.iter().map(|g| g.get()).collect();
    db.within(QUERY, async {
        let named_users: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT id, username, display_name FROM forge.users WHERE id = ANY($1)")
                .bind(&user_ids)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        let named_groups: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT g.id, n.full_path, g.display_name FROM forge.groups g \
             JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = g.id \
             WHERE g.id = ANY($1)",
        )
        .bind(&group_ids)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(ApproverNames {
            users: named_users
                .into_iter()
                .map(|(id, username, display_name)| {
                    let id = UserId::new(id);
                    (
                        id,
                        NamedUser {
                            id,
                            username,
                            display_name,
                        },
                    )
                })
                .collect(),
            groups: named_groups
                .into_iter()
                .map(|(id, full_path, display_name)| {
                    let id = GroupId::new(id);
                    (
                        id,
                        NamedGroup {
                            id,
                            full_path,
                            display_name,
                        },
                    )
                })
                .collect(),
        })
    })
    .await
}

/// The approval facts of one merge request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalFacts {
    pub approvals: Vec<ApproverFacts>,
    pub rules: Vec<StoredApprovalRule>,
    /// M3 R6: the project's merge settings (whether committers' approvals
    /// are excluded, whether CODEOWNERS is enforced now).
    pub settings: crate::review::settings::StoredMergeSettings,
    /// M3 R6: the project's protection rules (the target's merge rule
    /// carries the minimum approvals, ADR-0024 §8.6).
    pub protection: forge_core::ProtectionRules,
}

/// The most live approvals one merge request holds (`FG-REVIEW-0015`,
/// checked under its row lock by the approval): the revocation of a new
/// head writes one event per approval, within one change's 100 events.
/// More on read is `FG-SYS-0001 reason=approvals_over_cap`, never a
/// silently short list.
pub const MAX_APPROVALS: i64 = 50;

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

pub(crate) fn approval_columns() -> String {
    format!(
        "id, merge_request_id, approver_id, head_sha, target_branch, \
         to_char(created_at AT TIME ZONE 'UTC', {TS}) AS created_at, \
         to_char(revoked_at AT TIME ZONE 'UTC', {TS}) AS revoked_at, revoked_reason"
    )
}

#[derive(sqlx::FromRow)]
pub(crate) struct ApprovalRow {
    id: i64,
    merge_request_id: i64,
    approver_id: i64,
    head_sha: String,
    target_branch: String,
    created_at: String,
    revoked_at: Option<String>,
    revoked_reason: Option<String>,
}

fn stored_invalid(reason: &'static str, id: i64) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("approval_id", id)
}

impl ApprovalRow {
    pub(crate) fn parse(self) -> Result<StoredApproval, ForgeError> {
        let id = self.id;
        Ok(StoredApproval {
            id: ApprovalId::new(id),
            merge_request: MergeRequestId::new(self.merge_request_id),
            approver: UserId::new(self.approver_id),
            head_sha: ObjectId::parse(&self.head_sha)
                .map_err(|_| stored_invalid("stored_approval_sha_invalid", id))?,
            target_branch: RefName::parse(&self.target_branch)
                .map_err(|_| stored_invalid("stored_approval_branch_invalid", id))?,
            created_at: self.created_at,
            revoked_at: self.revoked_at,
            revoked_reason: self
                .revoked_reason
                .map(|r| {
                    UnapproveReason::parse(&r)
                        .ok_or_else(|| stored_invalid("stored_revoked_reason_invalid", id))
                })
                .transpose()?,
        })
    }
}

/// A live approval and the head a person gave it on (M3 R6 review round
/// 2): a carried approval's `carried_from` chain followed back to the
/// approval without one. The carry limits of ADR-0024 §8.2 are measured
/// from this head — cumulative over every carried push, never per push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedHead {
    pub approval: ApprovalId,
    /// `None`: the chain is longer than [`MAX_CARRY_HOPS`] (or leaves the
    /// merge request) — never carried (fail closed: the approval ends).
    pub approved_head: Option<ObjectId>,
}

/// The most `carried_from` hops followed back to the approval a person
/// gave (one per carried push; the recursion's bound). A longer chain is
/// never carried again.
pub const MAX_CARRY_HOPS: i32 = 1_000;

/// The [`ApprovedHead`] of every live approval of `mr`, oldest first.
/// More than [`MAX_APPROVALS`] is `FG-SYS-0001 reason=approvals_over_cap`.
pub(crate) async fn approved_heads_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
) -> Result<Vec<ApprovedHead>, ForgeError> {
    approved_heads_capped_on(conn, mr, MAX_CARRY_HOPS).await
}

/// [`approved_heads_on`] following at most `max_hops` hops.
pub(crate) async fn approved_heads_capped_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
    max_hops: i32,
) -> Result<Vec<ApprovedHead>, ForgeError> {
    // What holds a chain together: carried_from is a foreign key to an
    // approval, fixed at insert (the approvals guard refuses any change to
    // it), and forge-db sets it only in `carry_on`, to the id of a live
    // approval of the same merge request it just ended (no caller supplies
    // it). The database does not enforce "older" or "same merge request",
    // so the walk stays on this merge request's rows and stops after
    // `max_hops` hops: a chain that does not reach an approval without
    // carried_from by then has no approved head (never carried).
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "WITH RECURSIVE chain (live_id, carried_from, head_sha, hops) AS ( \
             SELECT id, carried_from, head_sha, 0 FROM forge.merge_request_approvals \
             WHERE merge_request_id = $1 AND revoked_at IS NULL \
           UNION ALL \
             SELECT c.live_id, a.carried_from, a.head_sha, c.hops + 1 \
             FROM chain c JOIN forge.merge_request_approvals a \
               ON a.id = c.carried_from AND a.merge_request_id = $1 \
             WHERE c.hops < $3 \
         ) \
         SELECT live_id, min(head_sha) FILTER (WHERE carried_from IS NULL) \
         FROM chain GROUP BY live_id ORDER BY live_id LIMIT $2",
    )
    .bind(mr.get())
    .bind(MAX_APPROVALS + 1)
    .bind(max_hops)
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approved_heads", e))?;
    if rows.len() as i64 > MAX_APPROVALS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "approvals_over_cap")
            .with_context("merge_request_id", mr.get()));
    }
    rows.into_iter()
        .map(|(id, head)| {
            Ok(ApprovedHead {
                approval: ApprovalId::new(id),
                approved_head: head
                    .map(|h| ObjectId::parse(&h))
                    .transpose()
                    .map_err(|_| stored_invalid("stored_approval_sha_invalid", id))?,
            })
        })
        .collect()
}

/// [`approved_heads_on`] on the read pool (the version job's carry
/// decision, before the merge request is locked; forge-db reads them again
/// under the lock).
pub async fn approved_heads(db: &Db, mr: MergeRequestId) -> Result<Vec<ApprovedHead>, ForgeError> {
    const QUERY: &str = "review.approved_heads";
    db.within(QUERY, async {
        let mut tx = db
            .pool()
            .executor()
            .begin()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        let heads = approved_heads_on(&mut tx, mr).await?;
        tx.commit().await.map_err(|e| read_query_error(QUERY, e))?;
        Ok(heads)
    })
    .await
}

/// The live approvals of `mr`, oldest first.
pub(crate) async fn live_approvals_on(
    conn: &mut PgConnection,
    mr: MergeRequestId,
) -> Result<Vec<StoredApproval>, ForgeError> {
    let rows: Vec<ApprovalRow> = sqlx::query_as(&format!(
        "SELECT {} FROM forge.merge_request_approvals WHERE merge_request_id = $1 \
         AND revoked_at IS NULL ORDER BY id LIMIT $2",
        approval_columns()
    ))
    .bind(mr.get())
    .bind(MAX_APPROVALS + 1)
    .fetch_all(conn)
    .await
    .map_err(|e| query_error("review.approvals_live", e))?;
    if rows.len() as i64 > MAX_APPROVALS {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "approvals_over_cap")
            .with_context("merge_request_id", mr.get()));
    }
    rows.into_iter().map(ApprovalRow::parse).collect()
}

/// The approvers' facts now: three statements whatever their number (R5
/// review) — the accounts, the roles on the project, the direct groups.
async fn approvers_on(
    conn: &mut PgConnection,
    project: ProjectId,
    live: Vec<StoredApproval>,
) -> Result<Vec<ApproverFacts>, ForgeError> {
    let ids: Vec<i64> = live.iter().map(|a| a.approver.get()).collect();
    let accounts: Vec<(i64, String, bool, String)> = sqlx::query_as(
        "SELECT id, username, state = 'active', kind FROM forge.users WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| query_error("review.approvers", e))?;
    let users: Vec<UserId> = live.iter().map(|a| a.approver).collect();
    let mut roles = crate::roles::project_roles_on(&mut *conn, project, &users).await?;
    let memberships: Vec<(i64, i64)> =
        sqlx::query_as("SELECT user_id, group_id FROM forge.group_members WHERE user_id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("review.approver_groups", e))?;
    let mut out = Vec::with_capacity(live.len());
    for approval in live {
        let user = approval.approver;
        let Some((_, username, active, kind)) = accounts.iter().find(|(id, ..)| *id == user.get())
        else {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "approver_missing")
                .with_context("user_id", user.get()));
        };
        let kind = UserKind::parse(kind).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "unknown_user_kind")
                .with_context("user_id", user.get())
        })?;
        out.push(ApproverFacts {
            approval,
            username: username.clone(), // clone: each approver owns its name
            active: *active,
            kind,
            role: roles.remove(&user).flatten(),
            groups: memberships
                .iter()
                .filter(|(u, _)| *u == user.get())
                .map(|(_, g)| GroupId::new(*g))
                .collect(),
        });
    }
    Ok(out)
}

/// The approval facts of merge request `mr` of `project` on `conn` (the
/// caller holds the merge request's row lock inside its transaction).
pub(crate) async fn approval_facts_on(
    conn: &mut PgConnection,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<ApprovalFacts, ForgeError> {
    let live = live_approvals_on(&mut *conn, mr).await?;
    let approvals = approvers_on(&mut *conn, project, live).await?;
    let rules = rules::rules_on(&mut *conn, project).await?;
    let settings = crate::review::settings::settings_on(&mut *conn, project).await?;
    let protection = crate::protected_refs::rules_on(&mut *conn, project).await?;
    Ok(ApprovalFacts {
        approvals,
        rules,
        settings,
        protection,
    })
}

/// [`approval_facts_on`] in one transaction on the read pool (the approval
/// state route; the read pool's role reads only). Not one snapshot: the
/// transaction runs at the pool's READ COMMITTED, so its five statements
/// (approvals, accounts, roles, groups, rules) may each see a later commit.
/// That is accepted for a display (R5 review): no decision is taken on it —
/// a merge decides on the same facts read again inside its gate
/// transaction under the merge request's lock — and transaction control
/// (an isolation level) belongs to `ChangeTx` alone (ADR-0016 §6, the
/// db-write-boundary gate).
pub async fn approval_facts(
    db: &Db,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<ApprovalFacts, ForgeError> {
    const QUERY: &str = "review.approval_facts";
    db.within(QUERY, async {
        let mut tx = db
            .pool()
            .executor()
            .begin()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        let facts = approval_facts_on(&mut tx, project, mr).await?;
        tx.commit().await.map_err(|e| read_query_error(QUERY, e))?;
        Ok(facts)
    })
    .await
}

/// The approval rules of `project`, by name.
pub async fn approval_rules(
    db: &Db,
    project: ProjectId,
) -> Result<Vec<StoredApprovalRule>, ForgeError> {
    const QUERY: &str = "review.approval_rules";
    db.within(QUERY, async {
        let mut conn = db
            .pool()
            .executor()
            .acquire()
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
        rules::rules_on(&mut conn, project).await
    })
    .await
}

/// One pusher of a commit (M3 R5 round 4: a `push_commits` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitPusher {
    pub commit: ObjectId,
    pub pusher: UserId,
    /// The `repository.pushed` audit row of the push (the evidence shown
    /// with `committer`; `None` when it cannot be found).
    pub audit_id: Option<i64>,
}

/// The pushers of the commits of `range` in repository `repo` of `project`
/// (ADR-0024 §8.3, M3 R5 round 4): one indexed statement on the read pool
/// over `push_commits`, whatever the repository's other history. A range
/// commit with no row is absent from the answer (the caller fails closed).
pub async fn range_pushers(
    db: &Db,
    project: ProjectId,
    repo: RepoId,
    range: &[ObjectId],
) -> Result<Vec<CommitPusher>, ForgeError> {
    const QUERY: &str = "review.range_pushers";
    if range.is_empty() {
        return Ok(Vec::new());
    }
    let shas: Vec<&str> = range.iter().map(ObjectId::as_str).collect();
    let rows: Vec<(String, i64, Option<i64>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.commit_sha, p.pusher_id, \
                   (SELECT min(a.id) FROM forge.audit_log a \
                    WHERE a.request_id = p.request_id AND a.project_id = $1 \
                      AND a.action = 'repository.pushed') \
                 FROM forge.push_commits p \
                 WHERE p.repository_id = $2 AND p.commit_sha = ANY($3)",
            )
            .bind(project.get())
            .bind(repo.get())
            .bind(&shas)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(sha, pusher, audit_id)| {
            Ok(CommitPusher {
                commit: ObjectId::parse(&sha).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "stored_push_commit_invalid")
                })?,
                pusher: UserId::new(pusher),
                audit_id,
            })
        })
        .collect()
}

// The users and groups a CODEOWNERS file names, resolved now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Owners {
    /// username → user, active users only (R5 review: a disabled account
    /// answers like a missing one — no existence oracle).
    pub users: Vec<(String, UserId)>,
    /// group full path → group.
    pub groups: Vec<(String, GroupId)>,
}

/// Resolve `usernames` (active users) and `group_paths` (live groups:
/// the namespaces table holds no deleted or moved-away path — a tombstone
/// is a row of its own table) — one statement each, on the read pool.
/// Names that do not resolve are absent from the answer, whatever the
/// reason (the callers answer missing and invisible alike).
pub async fn owners(
    db: &Db,
    usernames: &[String],
    group_paths: &[String],
) -> Result<Owners, ForgeError> {
    const QUERY: &str = "review.codeowners_owners";
    db.within(QUERY, async {
        let users: Vec<(String, i64)> = sqlx::query_as(
            "SELECT username, id FROM forge.users WHERE username = ANY($1) \
             AND state = 'active'",
        )
        .bind(usernames)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        let groups: Vec<(String, i64)> = sqlx::query_as(
            "SELECT full_path, owner_id FROM forge.namespaces \
             WHERE kind = 'group' AND full_path = ANY($1)",
        )
        .bind(group_paths)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Owners {
            users: users
                .into_iter()
                .map(|(n, id)| (n, UserId::new(id)))
                .collect(),
            groups: groups
                .into_iter()
                .map(|(p, id)| (p, GroupId::new(id)))
                .collect(),
        })
    })
    .await
}
