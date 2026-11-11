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
//!   `merge_requests_open_author`).
//! - `approvable`: open, not draft, with a head, not the caller's own, in a
//!   project where the caller holds a membership (directly, through an
//!   ancestor group — the role chain of ADR-0010 — or as the owner of the
//!   personal namespace), and without the caller's live approval of the
//!   current head and target (the pair `evaluate_merge()` counts); one
//!   bounded probe per such project (index `merge_requests_open_project`).
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
//! The statement picks the page's ids first and computes the facts for
//! those rows only.

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

/// The mergeability check found a conflict for the current head of the
/// merge request row `t`.
fn conflict(t: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM forge.merge_request_mergeability g            WHERE g.merge_request_id = {t}.id AND g.status = 'cannot_be_merged'              AND g.checked_head_sha = {t}.head_sha)"
    )
}

/// A conflict with another open merge request is predicted for the
/// current head of `t` (both heads current, both open, on the pair's
/// target: the reads of [`super::conflict_predictions`]).
fn conflict_predicted(t: &str) -> String {
    let side = |me: &str, other: &str| {
        format!(
            "EXISTS (SELECT 1 FROM forge.merge_request_conflict_pairs p                JOIN forge.merge_requests o ON o.id = p.{other}_mr_id                WHERE p.{me}_mr_id = {t}.id AND p.{me}_head_sha = {t}.head_sha                  AND o.head_sha = p.{other}_head_sha AND p.outcome = 'conflict'                  AND p.target_branch = {t}.target_branch AND o.state = 'open'                  AND o.project_id = {t}.project_id AND o.target_branch = p.target_branch)"
        )
    };
    format!("({} OR {})", side("low", "high"), side("high", "low"))
}

/// The `FROM … WHERE` of the check runs on the current head of `t` that
/// completed without success.
fn failed_runs(t: &str) -> String {
    format!(
        "FROM forge.check_runs c WHERE c.project_id = {t}.project_id            AND c.head_sha = {t}.head_sha AND c.status = 'completed'            AND c.conclusion <> 'success'"
    )
}

/// The facts of [`UserListRow`], as columns of a `SELECT … FROM
/// forge.merge_requests` (unaliased: [`COLUMNS`] qualifies by the table).
/// `$4`: the most failed check names per row.
fn facts() -> String {
    let t = "merge_requests";
    format!(
        "{} AS conflict, {} AS conflict_predicted,          ARRAY(SELECT c.name::text {} ORDER BY c.name LIMIT $4) AS failed_checks",
        conflict(t),
        conflict_predicted(t),
        failed_runs(t)
    )
}

/// One page: `ctes` (each ending with `, `), then `page` — a statement
/// returning the page's ids (`$1` the caller, `$2` the previous page's last
/// id or NULL, `$3` the page size) — and the rows of those ids with their
/// facts, newest first. The facts are computed for the page's rows only.
fn page_sql(ctes: &str, page: &str) -> String {
    format!(
        "WITH RECURSIVE {ctes}page AS ({page})          SELECT {COLUMNS}, {facts} FROM forge.merge_requests          WHERE merge_requests.id IN (SELECT id FROM page)          ORDER BY merge_requests.id DESC",
        facts = facts()
    )
}

/// The open merge requests of `$1` (index `merge_requests_open_author`),
/// `extra` narrowing them.
fn own_page(extra: &str) -> String {
    format!(
        "SELECT m.id FROM forge.merge_requests m          WHERE m.author_id = $1 AND m.state = 'open' {extra}            AND ($2::bigint IS NULL OR m.id < $2)          ORDER BY m.id DESC LIMIT $3"
    )
}

fn authored_sql() -> String {
    page_sql("", &own_page(""))
}

/// The projects where the caller (`$1`) holds a membership: the project
/// namespaces under a group the caller is a direct member of (descendants
/// to `$5` steps) or under the caller's personal namespace, and the
/// projects the caller is a direct member of. One bounded newest-first
/// probe per project (index `merge_requests_open_project`), then the
/// newest `$3` of them.
fn approvable_sql() -> String {
    page_sql(
        "roots AS (            SELECT n.id FROM forge.group_members gm              JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = gm.group_id              WHERE gm.user_id = $1            UNION SELECT n.id FROM forge.namespaces n WHERE n.kind = 'user' AND n.owner_id = $1),          down (id, step) AS (            SELECT id, 1 FROM roots            UNION ALL            SELECT n.id, d.step + 1 FROM forge.namespaces n JOIN down d ON n.parent_id = d.id              WHERE d.step < $5),          related AS (            SELECT n.owner_id AS project_id FROM down d              JOIN forge.namespaces n ON n.id = d.id AND n.kind = 'project'            UNION SELECT pm.project_id FROM forge.project_members pm WHERE pm.user_id = $1), ",
        "SELECT c.id FROM related r CROSS JOIN LATERAL (            SELECT m.id FROM forge.merge_requests m            WHERE m.project_id = r.project_id AND m.state = 'open' AND NOT m.draft              AND m.head_sha IS NOT NULL AND m.author_id <> $1              AND ($2::bigint IS NULL OR m.id < $2)              AND NOT EXISTS (SELECT 1 FROM forge.merge_request_approvals a                WHERE a.merge_request_id = m.id AND a.approver_id = $1                  AND a.revoked_at IS NULL AND a.head_sha = m.head_sha                  AND a.target_branch = m.target_branch)            ORDER BY m.id DESC LIMIT $3) c          ORDER BY c.id DESC LIMIT $3",
    )
}

/// The caller's open merge requests with a head, walked newest first and
/// kept only when a fact holds (the cheap predicates first: the author
/// index bounds the walk).
fn attention_sql() -> String {
    page_sql(
        "",
        &own_page(&format!(
            "AND m.head_sha IS NOT NULL AND ({} OR {} OR EXISTS (SELECT 1 {}))",
            conflict("m"),
            conflict_predicted("m"),
            failed_runs("m")
        )),
    )
}

/// The statement of `scope` (exposed for the statement tests).
pub(crate) fn sql_of(scope: UserListScope) -> String {
    match scope {
        UserListScope::Authored => authored_sql(),
        UserListScope::Approvable => approvable_sql(),
        UserListScope::Attention => attention_sql(),
    }
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
    let sql = sql_of(scope);
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    let rows: Vec<FactsRow> = db
        .within(query, async {
            let statement = sqlx::query_as(&sql)
                .bind(user.get())
                .bind(before)
                .bind(limit)
                .bind(MAX_FAILED_CHECKS);
            // Only the membership walk has a chain cap.
            let statement = match scope {
                UserListScope::Approvable => statement.bind(CAP),
                UserListScope::Authored | UserListScope::Attention => statement,
            };
            statement
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

#[cfg(test)]
#[path = "user_lists_tests.rs"]
mod tests;
