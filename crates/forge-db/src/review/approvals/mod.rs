//! Approvals of merge requests and approval rules (M3 R5, ADR-0024 §8;
//! plan 11 §4.1 R5). The SQL only: what counts is forge-review's
//! (`evaluate_merge()` at evaluation time).
//!
//! - Reads: [`approval_facts_on`] (inside a merge decision's transaction:
//!   the live approvals with each approver's facts **as they are now** —
//!   active, kind, role on the project, direct groups — and the project's
//!   rules), [`approval_facts`] (the same on the read pool, the approval
//!   state route), [`approval_rules`], [`push_coverage`] (the push records
//!   that carried a range's commits, ADR-0024 §8.3) and [`owners`] (the
//!   users and groups a CODEOWNERS file names).
//! - Commands (`commands.rs`): [`approve_mr_decided`] /
//!   [`unapprove_mr_decided`], decided inside their transaction like every
//!   review command; [`revoke_on`], the revocation the version job (a new
//!   head) and the edit command (a new target) run in their own
//!   transaction (one `merge_request.unapproved` per approval).
//! - Rules (`rules.rs`): [`put_approval_rule_mr_decided`],
//!   [`remove_approval_rule_mr_decided`] (`approval_rule.put` /
//!   `.removed`, audit only).

use std::collections::BTreeSet;

use forge_core::ids::{ApprovalId, ApprovalRuleId, GroupId, MergeRequestId, ProjectId, UserId};
use forge_core::{ObjectId, RefName, RefPattern, RepoId, Role, UnapproveReason, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::{query_error, read_query_error, Db};

mod commands;
mod rules;

pub(crate) use commands::revoke_on;
pub use commands::{approve_mr_decided, unapprove_mr_decided, ApproveOutcome, Classify};
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
}

/// The approval facts of one merge request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApprovalFacts {
    pub approvals: Vec<ApproverFacts>,
    pub rules: Vec<StoredApprovalRule>,
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

/// One approver's facts now.
async fn approver_on(
    conn: &mut PgConnection,
    project: ProjectId,
    approval: StoredApproval,
) -> Result<ApproverFacts, ForgeError> {
    let user = approval.approver;
    let row: Option<(String, bool, String)> =
        sqlx::query_as("SELECT username, state = 'active', kind FROM forge.users WHERE id = $1")
            .bind(user.get())
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| query_error("review.approver", e))?;
    let Some((username, active, kind)) = row else {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "approver_missing")
            .with_context("user_id", user.get()));
    };
    let kind = UserKind::parse(&kind).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_user_kind")
            .with_context("user_id", user.get())
    })?;
    let role = crate::roles::project_role_on(&mut *conn, project, user).await?;
    let groups: Vec<i64> =
        sqlx::query_scalar("SELECT group_id FROM forge.group_members WHERE user_id = $1")
            .bind(user.get())
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| query_error("review.approver_groups", e))?;
    Ok(ApproverFacts {
        approval,
        username,
        active,
        kind,
        role,
        groups: groups.into_iter().map(GroupId::new).collect(),
    })
}

/// The approval facts of merge request `mr` of `project` on `conn` (the
/// caller holds the merge request's row lock inside its transaction).
pub(crate) async fn approval_facts_on(
    conn: &mut PgConnection,
    project: ProjectId,
    mr: MergeRequestId,
) -> Result<ApprovalFacts, ForgeError> {
    let live = live_approvals_on(&mut *conn, mr).await?;
    let mut approvals = Vec::with_capacity(live.len());
    for approval in live {
        approvals.push(approver_on(&mut *conn, project, approval).await?);
    }
    let rules = rules::rules_on(&mut *conn, project).await?;
    Ok(ApprovalFacts { approvals, rules })
}

/// [`approval_facts_on`] in one transaction on the read pool (the approval
/// state route; the read pool's role reads only).
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

/// One ref update of a push record that may have carried a range's
/// commits (ADR-0024 §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushedEntry {
    /// The audit row (the evidence shown with `committer`).
    pub audit_id: i64,
    /// Who pushed (the replayed pusher for a runbook replay).
    pub pusher: Option<UserId>,
    /// The ref's new commit (`None` for a delete).
    pub new: Option<ObjectId>,
    /// The new commits the record lists for the ref.
    pub commits: Vec<ObjectId>,
    /// The record does not list every new commit of the ref.
    pub commits_truncated: bool,
}

/// What the push records say about a range of commits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PushCoverage {
    /// The ref updates that name a range commit (`new` or listed) or whose
    /// list is cut.
    pub entries: Vec<PushedEntry>,
    /// A record whose refs were cut (`refs_truncated`): some ref updates
    /// are not listed at all.
    pub refs_truncated: bool,
    /// More matching entries than the cap: the coverage is incomplete.
    pub over_cap: bool,
}

/// The most entries [`push_coverage`] reads.
pub const MAX_PUSH_ENTRIES: i64 = 2_000;

#[derive(sqlx::FromRow)]
struct EntryRow {
    id: i64,
    pusher: Option<i64>,
    refs_truncated: Option<bool>,
    new: Option<String>,
    commits_truncated: Option<bool>,
    commits: Vec<String>,
}

/// The `repository.pushed` records of repository `repo` of `project` whose
/// ref updates name one of `range` (as `new` or a listed commit), or whose
/// commit list or ref list is cut (ADR-0024 §8.3; the evaluation decides
/// what they mean). One statement on the read pool (the audit log's
/// `(project_id, occurred_at)` index narrows it to the project).
pub async fn push_coverage(
    db: &Db,
    project: ProjectId,
    repo: RepoId,
    range: &[ObjectId],
) -> Result<PushCoverage, ForgeError> {
    const QUERY: &str = "review.push_coverage";
    let range: Vec<&str> = range.iter().map(ObjectId::as_str).collect();
    let rows: Vec<EntryRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT a.id, \
                   COALESCE((a.details -> 'replayed_from' ->> 'user_id')::bigint, a.actor_user_id) \
                     AS pusher, \
                   (a.details ->> 'refs_truncated')::boolean AS refs_truncated, \
                   r.value ->> 'new' AS new, \
                   (r.value ->> 'commits_truncated')::boolean AS commits_truncated, \
                   ARRAY(SELECT jsonb_array_elements_text(COALESCE(r.value -> 'commits', '[]'))) \
                     AS commits \
                 FROM forge.audit_log a \
                 CROSS JOIN LATERAL jsonb_array_elements(COALESCE(a.details -> 'refs', '[]')) r \
                 WHERE a.project_id = $1 AND a.action = 'repository.pushed' \
                   AND a.outcome = 'succeeded' \
                   AND (a.details ->> 'repository_id')::bigint = $2 \
                   AND ((r.value ->> 'new') = ANY($3) \
                     OR COALESCE(r.value -> 'commits', '[]') ?| $3 \
                     OR COALESCE((r.value ->> 'commits_truncated')::boolean, true) \
                     OR COALESCE((a.details ->> 'refs_truncated')::boolean, false)) \
                 ORDER BY a.id LIMIT $4",
            )
            .bind(project.get())
            .bind(repo.get())
            .bind(&range)
            .bind(MAX_PUSH_ENTRIES + 1)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let over_cap = rows.len() as i64 > MAX_PUSH_ENTRIES;
    let mut coverage = PushCoverage {
        over_cap,
        ..PushCoverage::default()
    };
    for row in rows.into_iter().take(MAX_PUSH_ENTRIES as usize) {
        coverage.refs_truncated |= row.refs_truncated.unwrap_or(false);
        let new = row
            .new
            .as_deref()
            .and_then(|n| ObjectId::parse(n).ok())
            .filter(|n| !n.is_zero());
        coverage.entries.push(PushedEntry {
            audit_id: row.id,
            pusher: row.pusher.map(UserId::new),
            new,
            commits: row
                .commits
                .iter()
                .filter_map(|c| ObjectId::parse(c).ok())
                .collect(),
            commits_truncated: row.commits_truncated.unwrap_or(true),
        });
    }
    Ok(coverage)
}

/// The users and groups a CODEOWNERS file names, resolved now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Owners {
    /// username → user (active or not: an inactive owner simply never
    /// approves validly).
    pub users: Vec<(String, UserId)>,
    /// group full path → group.
    pub groups: Vec<(String, GroupId)>,
}

/// Resolve `usernames` and `group_paths` (one statement each, on the read
/// pool). Names that do not resolve are absent from the answer.
pub async fn owners(
    db: &Db,
    usernames: &[String],
    group_paths: &[String],
) -> Result<Owners, ForgeError> {
    const QUERY: &str = "review.codeowners_owners";
    db.within(QUERY, async {
        let users: Vec<(String, i64)> =
            sqlx::query_as("SELECT username, id FROM forge.users WHERE username = ANY($1)")
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
