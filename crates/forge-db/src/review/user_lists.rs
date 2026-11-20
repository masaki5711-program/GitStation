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

/// The projects where `user` holds a membership, as common table
/// expressions of a `WITH RECURSIVE` (each ending with `, `; the last,
/// `related`, has the column `project_id`): the project namespaces under a
/// group `user` is a direct member of (descendants to `cap` steps) or under
/// `user`'s personal namespace, and the projects `user` is a direct member
/// of. `user` and `cap` are the statement's placeholders (`$n`). Shared by
/// `approvable` and the activity list (plan 15 MR 8).
pub(crate) fn related_ctes(user: &str, cap: &str) -> String {
    format!(
        "roots AS (            SELECT n.id FROM forge.group_members gm              JOIN forge.namespaces n ON n.kind = 'group' AND n.owner_id = gm.group_id              WHERE gm.user_id = {user}            UNION SELECT n.id FROM forge.namespaces n WHERE n.kind = 'user' AND n.owner_id = {user}),          down (id, step) AS (            SELECT id, 1 FROM roots            UNION ALL            SELECT n.id, d.step + 1 FROM forge.namespaces n JOIN down d ON n.parent_id = d.id              WHERE d.step < {cap}),          related AS (            SELECT n.owner_id AS project_id FROM down d              JOIN forge.namespaces n ON n.id = d.id AND n.kind = 'project'            UNION SELECT pm.project_id FROM forge.project_members pm WHERE pm.user_id = {user}), "
    )
}

/// The projects where the caller (`$1`) holds a membership: the project
/// namespaces under a group the caller is a direct member of (descendants
/// to `$5` steps) or under the caller's personal namespace, and the
/// projects the caller is a direct member of. One bounded newest-first
/// probe per project (index `merge_requests_open_project`), then the
/// newest `$3` of them.
fn approvable_sql() -> String {
    page_sql(
        &related_ctes("$1", "$5"),
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

/// The PFS portal's `my-reviews`, `stopped-count` and `attention-count`
/// widgets (ADR-0036, widget design P1): which candidates, in which
/// projects, in which order. Candidates only, as above: forge-review
/// decides every row with `authorize()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidgetList {
    /// `approvable`: waiting for the person's review.
    Review,
    /// `approvable` in projects that release a specification (a
    /// `spec.released` in their audit trail; ADR-0023: a repository has a
    /// specification by its content, not by a kind): the specifications
    /// awaiting the person's approval.
    Spec,
    /// `authored`.
    Mine,
    /// Open merge requests of the person whose auto-merge stopped on the
    /// current head.
    StoppedMine,
    /// The same, plus those the person approved (a live approval of any
    /// head): the person as author or reviewer.
    StoppedInvolved,
    /// Open merge requests (anyone's) with an unresolved thread the person
    /// started whose newest note is somebody else's: a reply waits for
    /// them (index `merge_request_discussions_started_open`). Notes are
    /// people's comments only (resolving, reopening and pushing write no
    /// note), so no system note can count as a reply.
    Threads,
    /// The person's open merge requests with a head where a conflict is
    /// known, predicted, or could not be ruled out (the conflict scan ended
    /// `incomplete` / `failed`, the mergeability check `check_failed`, or a
    /// pair with another open merge request `failed`): never shown as "no
    /// conflict" when the scan did not finish.
    Conflicts,
    /// The person's open merge requests whose newest queue entry is live
    /// (`queued` / `testing`) or was removed by the queue itself (no
    /// `removed_by`: ejected, not dequeued by a person). An ejected merge
    /// request stays listed until it is queued again, closed or merged.
    Queue,
}

impl WidgetList {
    fn query(self) -> &'static str {
        match self {
            Self::Review => "review.widget.review",
            Self::Spec => "review.widget.spec",
            Self::Mine => "review.widget.mine",
            Self::StoppedMine => "review.widget.stopped_mine",
            Self::StoppedInvolved => "review.widget.stopped_involved",
            Self::Threads => "review.widget.threads",
            Self::Conflicts => "review.widget.conflicts",
            Self::Queue => "review.widget.queue",
        }
    }
}

/// The order of a widget's rows (by the merge request's id: its age).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WidgetOrder {
    /// Oldest first: what has waited longest (the widgets' default).
    Oldest,
    /// Newest first.
    Newest,
}

impl WidgetOrder {
    fn dir(self) -> &'static str {
        match self {
            Self::Oldest => "ASC",
            Self::Newest => "DESC",
        }
    }

    /// The keyset comparison: rows after the cursor in this order.
    fn after(self) -> &'static str {
        match self {
            Self::Oldest => ">",
            Self::Newest => "<",
        }
    }
}

/// One widget candidate: the row and the facts of the P1 lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetRow {
    pub row: UserListRow,
    /// `Conflicts`: a conflict could not be ruled out for the current head.
    pub conflict_unknown: bool,
    /// `Conflicts`: the smallest number of an open merge request a conflict
    /// is predicted with (current heads).
    pub conflict_peer_iid: Option<i64>,
    /// `Queue`: the 1-based place among the queue's live entries.
    pub queue_position: Option<i64>,
    /// `Queue`: the newest entry was removed by the queue itself.
    pub queue_ejected: bool,
}

/// A pair of `t` with another open merge request whose stored heads are
/// both current and whose outcome is `outcome` (one side of the unordered
/// pair; the reads of [`super::conflict_predictions`]).
fn pair_side(t: &str, me: &str, other: &str, outcome: &str, select: &str) -> String {
    format!(
        "SELECT {select} FROM forge.merge_request_conflict_pairs p \
           JOIN forge.merge_requests o ON o.id = p.{other}_mr_id \
           WHERE p.{me}_mr_id = {t}.id AND p.{me}_head_sha = {t}.head_sha \
             AND o.head_sha = p.{other}_head_sha AND p.outcome = '{outcome}' \
             AND p.target_branch = {t}.target_branch AND o.state = 'open' \
             AND o.project_id = {t}.project_id AND o.target_branch = p.target_branch"
    )
}

/// A conflict of `t`'s current head could not be ruled out: the latest
/// conflict scan has not finished (`pending`, `scanning`) or ended
/// `incomplete` / `failed`, the mergeability check has not finished
/// (`unchecked`, `checking`) or ended `check_failed`, or a pair with a
/// current open peer `failed` (indexes: the scans' and the mergeability's
/// primary key `merge_request_id`; the pairs' primary key `(low_mr_id,
/// high_mr_id)` and `merge_request_conflict_pairs_high`).
fn conflict_unknown(t: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM forge.merge_request_conflict_scans s \
            WHERE s.merge_request_id = {t}.id \
              AND s.status IN ('pending', 'scanning', 'incomplete', 'failed')) \
          OR EXISTS (SELECT 1 FROM forge.merge_request_mergeability g \
            WHERE g.merge_request_id = {t}.id \
              AND g.status IN ('unchecked', 'checking', 'check_failed')) \
          OR EXISTS ({}) OR EXISTS ({}))",
        pair_side(t, "low", "high", "failed", "1"),
        pair_side(t, "high", "low", "failed", "1")
    )
}

/// The newest queue entry of `t`, as `SELECT <cols> … LIMIT 1`.
fn newest_entry(t: &str, cols: &str) -> String {
    format!(
        "SELECT {cols} FROM forge.merge_queue_entries e \
           WHERE e.merge_request_id = {t}.id ORDER BY e.id DESC LIMIT 1"
    )
}

/// The P1 facts of `list`'s rows (four columns; constants for the lists
/// that do not show them, so nothing more is read for them).
fn widget_extras(list: WidgetList, light: bool) -> String {
    let t = "merge_requests";
    let conflicts = !light && matches!(list, WidgetList::Conflicts);
    let queue = !light && matches!(list, WidgetList::Queue);
    let unknown = if conflicts {
        conflict_unknown(t)
    } else {
        "false".to_owned()
    };
    let peer = if conflicts {
        format!(
            "(SELECT min(x.iid) FROM ({} UNION ALL {}) x)",
            pair_side(t, "low", "high", "conflict", "o.iid"),
            pair_side(t, "high", "low", "conflict", "o.iid")
        )
    } else {
        "NULL::bigint".to_owned()
    };
    let position = if queue {
        format!(
            "({})",
            newest_entry(
                t,
                "CASE WHEN e.state IN ('queued', 'testing') THEN (SELECT count(*) + 1 \
                   FROM forge.merge_queue_entries q WHERE q.queue_id = e.queue_id \
                   AND q.state IN ('queued', 'testing') AND q.position < e.position) END"
            )
        )
    } else {
        "NULL::bigint".to_owned()
    };
    let ejected = if queue {
        format!(
            "COALESCE(({}), false)",
            newest_entry(t, "e.state = 'removed' AND e.removed_by IS NULL")
        )
    } else {
        "false".to_owned()
    };
    format!(
        "{unknown} AS conflict_unknown, {peer} AS conflict_peer_iid, \
         {position} AS queue_position, {ejected} AS queue_ejected"
    )
}

/// The facts' columns as constants: a count reads the rows only to decide
/// them, never the facts it does not show.
const NO_FACTS: &str =
    "false AS conflict, false AS conflict_predicted, ARRAY[]::text[] AS failed_checks";

/// One widget page: as [`page_sql`], in `order`, with the P1 facts (none
/// when `light`: a count).
fn widget_page_sql(
    list: WidgetList,
    order: WidgetOrder,
    light: bool,
    ctes: &str,
    page: &str,
) -> String {
    format!(
        "WITH RECURSIVE {ctes}page AS ({page}) \
         SELECT {COLUMNS}, {facts}, {extras} FROM forge.merge_requests \
         WHERE merge_requests.id IN (SELECT id FROM page) \
         ORDER BY merge_requests.id {dir}",
        facts = if light { NO_FACTS.to_owned() } else { facts() },
        extras = widget_extras(list, light),
        dir = order.dir()
    )
}

/// The open merge requests of `$1` (index `merge_requests_open_author`) in
/// the projects `$5` (NULL: all), `extra` narrowing them, in `order`.
fn own_widget_page(order: WidgetOrder, extra: &str) -> String {
    format!(
        "SELECT m.id FROM forge.merge_requests m \
         WHERE m.author_id = $1 AND m.state = 'open' {extra} \
           AND ($2::bigint IS NULL OR m.id {after} $2) \
           AND ($5::bigint[] IS NULL OR m.project_id = ANY($5)) \
         ORDER BY m.id {dir} LIMIT $3",
        after = order.after(),
        dir = order.dir()
    )
}

/// The statement of `list` in `order`: `$1` the person, `$2` the cursor
/// (the previous page's last id), `$3` the page, `$4` the failed check
/// names per row, `$5` the projects (or NULL), then `$6` the chain cap
/// (`Review` / `Spec`) or whether approvals count (`Stopped*`). `light`:
/// without the facts (a count).
pub(crate) fn widget_sql(list: WidgetList, order: WidgetOrder, light: bool) -> String {
    let (after, dir) = (order.after(), order.dir());
    match list {
        WidgetList::Mine => widget_page_sql(list, order, light, "", &own_widget_page(order, "")),
        WidgetList::Review | WidgetList::Spec => widget_page_sql(
            list,
            order,
            light,
            &related_ctes("$1", "$6"),
            &approvable_page(
                order,
                if list == WidgetList::Spec {
                    "AND EXISTS (SELECT 1 FROM forge.audit_log al \
                       WHERE al.project_id = r.project_id AND al.action = 'spec.released')"
                } else {
                    ""
                },
            ),
        ),
        WidgetList::StoppedMine | WidgetList::StoppedInvolved => widget_page_sql(
            list,
            order,
            light,
            "",
            &format!(
                "SELECT m.id FROM forge.auto_merge_states s \
                 JOIN forge.merge_requests m ON m.id = s.merge_request_id AND m.head_sha = s.head_sha \
                 WHERE s.state = 'stopped' AND m.state = 'open' \
                   AND (m.author_id = $1 OR ($6 AND EXISTS (SELECT 1 FROM forge.merge_request_approvals a \
                     WHERE a.merge_request_id = m.id AND a.approver_id = $1 AND a.revoked_at IS NULL))) \
                   AND ($2::bigint IS NULL OR m.id {after} $2) \
                   AND ($5::bigint[] IS NULL OR m.project_id = ANY($5)) \
                 ORDER BY m.id {dir} LIMIT $3"
            ),
        ),
        WidgetList::Threads => widget_page_sql(
            list,
            order,
            light,
            "",
            &format!(
                "SELECT m.id FROM forge.merge_requests m \
                 WHERE m.state = 'open' AND m.id IN ( \
                   SELECT d.merge_request_id FROM forge.merge_request_discussions d \
                   WHERE d.started_by = $1 AND NOT d.resolved \
                     AND (SELECT n.author_id FROM forge.merge_request_notes n \
                          WHERE n.discussion_id = d.id ORDER BY n.id DESC LIMIT 1) <> $1) \
                   AND ($2::bigint IS NULL OR m.id {after} $2) \
                   AND ($5::bigint[] IS NULL OR m.project_id = ANY($5)) \
                 ORDER BY m.id {dir} LIMIT $3"
            ),
        ),
        WidgetList::Conflicts => widget_page_sql(
            list,
            order,
            light,
            "",
            &own_widget_page(
                order,
                &format!(
                    "AND m.head_sha IS NOT NULL AND ({} OR {} OR {})",
                    conflict("m"),
                    conflict_predicted("m"),
                    conflict_unknown("m")
                ),
            ),
        ),
        WidgetList::Queue => widget_page_sql(
            list,
            order,
            light,
            "",
            &own_widget_page(
                order,
                &format!(
                    "AND EXISTS (SELECT 1 FROM ({}) l WHERE l.state IN ('queued', 'testing') \
                       OR (l.state = 'removed' AND l.removed_by IS NULL))",
                    newest_entry("m", "e.state, e.removed_by")
                ),
            ),
        ),
    }
}

/// `approvable`'s page over `related`, in the projects `$5` when given,
/// `extra` narrowing the projects, in `order`.
fn approvable_page(order: WidgetOrder, extra: &str) -> String {
    format!(
        "SELECT c.id FROM related r CROSS JOIN LATERAL ( \
           SELECT m.id FROM forge.merge_requests m \
           WHERE m.project_id = r.project_id AND m.state = 'open' AND NOT m.draft \
             AND m.head_sha IS NOT NULL AND m.author_id <> $1 \
             AND ($2::bigint IS NULL OR m.id {after} $2) \
             AND NOT EXISTS (SELECT 1 FROM forge.merge_request_approvals a \
               WHERE a.merge_request_id = m.id AND a.approver_id = $1 \
                 AND a.revoked_at IS NULL AND a.head_sha = m.head_sha \
                 AND a.target_branch = m.target_branch) \
           ORDER BY m.id {dir} LIMIT $3) c \
         WHERE ($5::bigint[] IS NULL OR r.project_id = ANY($5)) {extra} \
         ORDER BY c.id {dir} LIMIT $3",
        after = order.after(),
        dir = order.dir()
    )
}

#[derive(sqlx::FromRow)]
struct WidgetFactsRow {
    #[sqlx(flatten)]
    facts: FactsRow,
    conflict_unknown: bool,
    conflict_peer_iid: Option<i64>,
    queue_position: Option<i64>,
    queue_ejected: bool,
}

/// One page of `list`'s candidates for `user`, in `projects` when given:
/// ids after the cursor `after` in `order`, at most `limit` (capped at
/// [`MAX_PAGE`]). `light`: the facts are not read (constants: a count).
#[allow(clippy::too_many_arguments)] // one statement's parameters
pub async fn widget_merge_requests(
    db: &Db,
    user: UserId,
    list: WidgetList,
    projects: Option<&[forge_core::ProjectId]>,
    order: WidgetOrder,
    after: Option<i64>,
    limit: usize,
    light: bool,
) -> Result<Vec<WidgetRow>, ForgeError> {
    let query = list.query();
    let sql = widget_sql(list, order, light);
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    let projects: Option<Vec<i64>> = projects.map(|ids| ids.iter().map(|p| p.get()).collect());
    let rows: Vec<WidgetFactsRow> = db
        .within(query, async {
            let statement = sqlx::query_as(&sql)
                .bind(user.get())
                .bind(after)
                .bind(limit)
                .bind(MAX_FAILED_CHECKS)
                .bind(projects.as_deref());
            let statement = match list {
                WidgetList::Mine
                | WidgetList::Threads
                | WidgetList::Conflicts
                | WidgetList::Queue => statement,
                WidgetList::Review | WidgetList::Spec => statement.bind(CAP),
                WidgetList::StoppedMine => statement.bind(false),
                WidgetList::StoppedInvolved => statement.bind(true),
            };
            statement
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(query, e))
        })
        .await?;
    rows.into_iter()
        .map(|r| {
            Ok(WidgetRow {
                row: UserListRow {
                    merge_request: r.facts.row.parse()?,
                    conflict: r.facts.conflict,
                    conflict_predicted: r.facts.conflict_predicted,
                    failed_checks: r.facts.failed_checks,
                },
                conflict_unknown: r.conflict_unknown,
                conflict_peer_iid: r.conflict_peer_iid,
                queue_position: r.queue_position,
                queue_ejected: r.queue_ejected,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "user_lists_tests.rs"]
mod tests;
