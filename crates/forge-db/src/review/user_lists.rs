//! The merge requests of one person's own work across projects (plan 15
//! MR 6): the SQL of forge-review's `Review::user_merge_requests` and
//! `…_counts` (`GET /api/v1/user/merge-requests?scope=` and `…/counts`).
//!
//! **Candidates only.** Each statement narrows the rows a scope can hold;
//! it never decides access. forge-review decides every row again with
//! `forge_auth` (`MergeRequestRead`, and `MergeRequestApprove` for
//! `approvable`) on the project as loaded for the caller, so a merge
//! request in a project the caller cannot see is never listed or counted.
//!
//! - `authored`: open merge requests whose author is the caller (index
//!   `merge_requests_author_state`).
//! - `approvable`: open, not draft, with a head, not the caller's own, in a
//!   project where the caller holds a membership (directly, through an
//!   ancestor group — the role chain of ADR-0010 — or as the owner of the
//!   personal namespace), and without the caller's live approval of the
//!   current head and target (the pair `evaluate_merge()` counts).
//! - `attention`: the caller's open merge requests with a head where the
//!   head is known to need the author: the mergeability check found a
//!   conflict for it (`cannot_be_merged`), a conflict with another open
//!   merge request is predicted for it (both heads current, the reads of
//!   [`super::conflict_predictions`]), or a check run on it completed
//!   without success. forge-review keeps a failed run only when the
//!   target's merge rule requires its name.
//!
//! One statement per page on the read pool under [`Db::within`]; newest
//! first by the merge request's id (`before`: the previous page's last id).

use forge_core::ids::UserId;
use forge_errors::ForgeError;

use super::{Row, StoredMergeRequest, COLUMNS, MAX_PAGE};
use crate::roles::CAP;
use crate::{read_query_error, Db};

/// Which of the caller's lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserListScope {
    Authored,
    Approvable,
    Attention,
}

impl UserListScope {
    pub const ALL: [UserListScope; 3] = [Self::Authored, Self::Approvable, Self::Attention];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Approvable => "approvable",
            Self::Attention => "attention",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == raw)
    }
}

/// One candidate row with the facts `attention` is decided on (read for
/// every scope: the answer shows them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserListRow {
    pub merge_request: StoredMergeRequest,
    /// The mergeability check found a conflict for the current head.
    pub conflict: bool,
    /// A conflict with another open merge request is predicted for the
    /// current head.
    pub conflict_predicted: bool,
    /// The names of the check runs on the current head that completed
    /// without success (at most [`MAX_FAILED_CHECKS`], by name).
    pub failed_checks: Vec<String>,
}

/// The most failed check names read per row (a merge rule requires at most
/// 20 names).
pub const MAX_FAILED_CHECKS: i64 = 100;

/// The facts of [`UserListRow`], as columns of a `SELECT … FROM
/// forge.merge_requests` (unaliased: [`COLUMNS`] qualifies by the table).
const FACTS: &str = "\
     EXISTS (SELECT 1 FROM forge.merge_request_mergeability g \
       WHERE g.merge_request_id = merge_requests.id AND g.status = 'cannot_be_merged' \
         AND g.checked_head_sha = merge_requests.head_sha) AS conflict, \
     (EXISTS (SELECT 1 FROM forge.merge_request_conflict_pairs p \
        JOIN forge.merge_requests o ON o.id = p.high_mr_id \
        WHERE p.low_mr_id = merge_requests.id AND p.low_head_sha = merge_requests.head_sha \
          AND o.head_sha = p.high_head_sha AND p.outcome = 'conflict' \
          AND p.target_branch = merge_requests.target_branch AND o.state = 'open' \
          AND o.project_id = merge_requests.project_id AND o.target_branch = p.target_branch) \
      OR EXISTS (SELECT 1 FROM forge.merge_request_conflict_pairs p \
        JOIN forge.merge_requests o ON o.id = p.low_mr_id \
        WHERE p.high_mr_id = merge_requests.id AND p.high_head_sha = merge_requests.head_sha \
          AND o.head_sha = p.low_head_sha AND p.outcome = 'conflict' \
          AND p.target_branch = merge_requests.target_branch AND o.state = 'open' \
          AND o.project_id = merge_requests.project_id AND o.target_branch = p.target_branch)) \
       AS conflict_predicted, \
     ARRAY(SELECT c.name::text FROM forge.check_runs c \
       WHERE c.project_id = merge_requests.project_id AND c.head_sha = merge_requests.head_sha \
         AND c.status = 'completed' AND c.conclusion <> 'success' \
       ORDER BY c.name LIMIT $5) AS failed_checks";

/// `$1` the caller, `$2` the previous page's last id (NULL: from the
/// newest), `$3` the page size, `$4` the chain cap, `$5` the most failed
/// check names per row.
const KEYSET: &str = "($2::bigint IS NULL OR merge_requests.id < $2) \
     ORDER BY merge_requests.id DESC LIMIT $3";

fn authored_sql() -> String {
    format!(
        "SELECT {COLUMNS}, {FACTS} FROM forge.merge_requests \
         WHERE merge_requests.author_id = $1 AND merge_requests.state = 'open' \
           AND $4::int > 0 AND {KEYSET}"
    )
}

/// The projects where the caller (`$1`) holds a membership: the project
/// namespaces under a group the caller is a direct member of (descendants
/// to `$4` steps) or under the caller's personal namespace, and the projects
/// the caller is a direct member of.
fn approvable_sql() -> String {
    format!(
        "WITH RECURSIVE roots AS ( \
           SELECT n.id FROM forge.group_members m \
             JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = m.group_id \
             WHERE m.user_id = $1 \
           UNION SELECT n.id FROM forge.namespaces n WHERE n.kind = 'user' AND n.owner_id = $1), \
         down (id, step) AS ( \
           SELECT id, 1 FROM roots \
           UNION ALL \
           SELECT n.id, d.step + 1 FROM forge.namespaces n JOIN down d ON n.parent_id = d.id \
             WHERE d.step < $4), \
         related AS ( \
           SELECT n.owner_id AS project_id FROM down d \
             JOIN forge.namespaces n ON n.id = d.id AND n.kind = 'project' \
           UNION SELECT pm.project_id FROM forge.project_members pm WHERE pm.user_id = $1) \
         SELECT {COLUMNS}, {FACTS} FROM forge.merge_requests \
         WHERE merge_requests.project_id IN (SELECT project_id FROM related) \
           AND merge_requests.state = 'open' AND NOT merge_requests.draft \
           AND merge_requests.head_sha IS NOT NULL AND merge_requests.author_id <> $1 \
           AND NOT EXISTS (SELECT 1 FROM forge.merge_request_approvals a \
             WHERE a.merge_request_id = merge_requests.id AND a.approver_id = $1 \
               AND a.revoked_at IS NULL AND a.head_sha = merge_requests.head_sha \
               AND a.target_branch = merge_requests.target_branch) \
           AND {KEYSET}"
    )
}

fn attention_sql() -> String {
    format!(
        "SELECT * FROM (SELECT {COLUMNS}, {FACTS} FROM forge.merge_requests \
           WHERE merge_requests.author_id = $1 AND merge_requests.state = 'open' \
             AND merge_requests.head_sha IS NOT NULL AND $4::int > 0 \
             AND ($2::bigint IS NULL OR merge_requests.id < $2)) merge_requests \
         WHERE merge_requests.conflict OR merge_requests.conflict_predicted \
           OR cardinality(merge_requests.failed_checks) > 0 \
         ORDER BY merge_requests.id DESC LIMIT $3"
    )
}

#[derive(sqlx::FromRow)]
struct FactsRow {
    #[sqlx(flatten)]
    row: Row,
    conflict: bool,
    conflict_predicted: bool,
    failed_checks: Vec<String>,
}

/// One page of `scope`'s candidates for `user`: ids below `before` when
/// given, newest first, at most `limit` (capped at [`MAX_PAGE`]).
pub async fn user_merge_requests(
    db: &Db,
    user: UserId,
    scope: UserListScope,
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<UserListRow>, ForgeError> {
    let query: &'static str = match scope {
        UserListScope::Authored => "review.user_merge_requests.authored",
        UserListScope::Approvable => "review.user_merge_requests.approvable",
        UserListScope::Attention => "review.user_merge_requests.attention",
    };
    let sql = match scope {
        UserListScope::Authored => authored_sql(),
        UserListScope::Approvable => approvable_sql(),
        UserListScope::Attention => attention_sql(),
    };
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    let rows: Vec<FactsRow> = db
        .within(query, async {
            sqlx::query_as(&sql)
                .bind(user.get())
                .bind(before)
                .bind(limit)
                .bind(CAP)
                .bind(MAX_FAILED_CHECKS)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(query, e))
        })
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(UserListRow {
                merge_request: r.row.parse()?,
                conflict: r.conflict,
                conflict_predicted: r.conflict_predicted,
                failed_checks: r.failed_checks,
            })
        })
        .collect()
}
