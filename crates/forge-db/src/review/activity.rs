//! The recent events of the projects one person is involved in (plan 15
//! MR 8, §3.4): the SQL of forge-review's `Review::user_activity`
//! (`GET /api/v1/user/activity`).
//!
//! **Candidates only.** Each statement narrows the rows the list can hold;
//! it never decides access. forge-review decides every row again with
//! `forge_auth` (`ActivityRead` on the event's project and visibility
//! class) on the project as loaded for the caller now, so an event of a
//! project the caller may not read is never shown.
//!
//! - [`involved_projects`]: the projects where the caller holds a
//!   membership (the walk of `user_lists`' `approvable`: directly, through
//!   an ancestor group, or as the owner of the personal namespace), at most
//!   [`MAX_MEMBERSHIP_CANDIDATES`], newest project first; more is said
//!   (`truncated`), never silently cut. forge-review loads their access
//!   rows in one statement (`lists::project_records`), decides each with
//!   `authorize()` and keeps at most [`MAX_INVOLVED_PROJECTS`] allowed ones
//!   before any event is read (review MEDIUM: a Guest-only membership
//!   costs no event scan).
//! - [`activity_page`]: the newest sequenced events of those projects and
//!   of the types asked for, below `before` (a `seq`). One ordered,
//!   cut-short range per (project, type) on `events_project_type_seq
//!   (project_id, event_type, seq) WHERE seq IS NOT NULL`, merged: the rows
//!   touched are bounded by pairs × the page size, never a project's
//!   backlog, and types the list does not show are never walked (the
//!   feed's statement, run backwards). No payload leaves the database: the
//!   statement extracts the few scalars the list may show (a merge
//!   request's `iid` and current title, a push's `total_refs`), each only
//!   when it has the expected shape.
//!
//! Both statements run on the read pool under [`Db::within`] (and the
//! pool's statement timeout).

use forge_core::ids::{ProjectId, UserId};
use forge_errors::ForgeError;

use super::user_lists::related_ctes;
use super::MAX_PAGE;
use crate::roles::CAP;
use crate::{read_query_error, Db};

/// The most allowed (Reporter and above) projects one read takes events
/// from (plan 15 §3.4); forge-review applies it after deciding.
pub const MAX_INVOLVED_PROJECTS: usize = 500;

/// The most membership candidates one read decides on (the batch access
/// read's bound, `lists::MAX_RECORD_IDS`).
pub const MAX_MEMBERSHIP_CANDIDATES: usize = crate::lists::MAX_RECORD_IDS;

/// The projects a person holds a membership in (candidates: not decided).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvolvedProjects {
    /// Newest project first, at most [`MAX_MEMBERSHIP_CANDIDATES`].
    pub ids: Vec<ProjectId>,
    /// More memberships than [`MAX_MEMBERSHIP_CANDIDATES`]: the oldest
    /// projects were left out.
    pub truncated: bool,
}

/// One candidate event, with only the scalars the list may show.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ActivityRow {
    /// The event's position in the feed (the keyset of `before`).
    pub seq: i64,
    pub event_type: String,
    /// UTC, RFC 3339 with milliseconds.
    pub occurred_at: String,
    pub project_id: i64,
    /// `user`, `service_account`, `user_via_service_account` or `system`.
    pub actor_kind: String,
    /// The acting account's current username and display name (`None`
    /// for `system` or an account that is gone).
    pub actor_username: Option<String>,
    pub actor_display_name: Option<String>,
    /// `data.iid` when it is a positive integer.
    pub iid: Option<i64>,
    /// The current title of the merge request `data.merge_request_id` names,
    /// when it is a merge request of the event's own project.
    pub title: Option<String>,
    /// `data.total_refs` when it is a non-negative integer.
    pub total_refs: Option<i64>,
}

/// The involved projects of `$1` (chain cap `$2`), newest first, at most
/// `$3`.
fn involved_sql() -> String {
    format!(
        "WITH RECURSIVE {} SELECT r.project_id FROM related r ORDER BY r.project_id DESC LIMIT $3",
        // The shared CTEs end with `, ` for a following one: none here.
        related_ctes("$1", "$2").trim_end().trim_end_matches(',')
    )
}

/// `data.<key>` as a bigint when its text matches `pattern`, else NULL (a
/// CASE, so a malformed value is never cast).
fn number(key: &str, pattern: &str) -> String {
    format!(
        "CASE WHEN jsonb_typeof(e.payload -> '{key}') = 'number' \
         AND (e.payload ->> '{key}') ~ '{pattern}' THEN (e.payload ->> '{key}')::bigint END"
    )
}

/// A positive bigint (at most 18 digits).
const POSITIVE: &str = "^[1-9][0-9]{0,17}$";
/// A non-negative integer of at most 9 digits.
const COUNT: &str = "^(0|[1-9][0-9]{0,8})$";

/// One page: `$1` the projects, `$2` the types, `$3` the previous page's
/// last `seq` or NULL, `$4` the page size.
pub(crate) fn page_sql() -> String {
    format!(
        "WITH page AS ( \
           SELECT x.seq FROM unnest($1::bigint[]) AS p(id) \
           CROSS JOIN unnest($2::text[]) AS t(name) \
           CROSS JOIN LATERAL (SELECT e.seq FROM forge.events e \
             WHERE e.project_id = p.id AND e.event_type = t.name AND e.seq IS NOT NULL \
               AND ($3::bigint IS NULL OR e.seq < $3) \
             ORDER BY e.seq DESC LIMIT $4) AS x \
           ORDER BY x.seq DESC LIMIT $4) \
         SELECT e.seq, e.event_type, \
           to_char(e.occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS occurred_at, \
           e.project_id, e.actor_kind, \
           u.username AS actor_username, u.display_name AS actor_display_name, \
           {iid} AS iid, m.title AS title, {refs} AS total_refs \
         FROM forge.events e \
         LEFT JOIN forge.users u ON u.id = e.actor_user_id \
         LEFT JOIN forge.merge_requests m ON m.project_id = e.project_id AND m.id = {mr} \
         WHERE e.seq IN (SELECT seq FROM page) \
         ORDER BY e.seq DESC",
        iid = number("iid", POSITIVE),
        refs = number("total_refs", COUNT),
        mr = number("merge_request_id", POSITIVE),
    )
}

/// The projects `user` holds a membership in (see the module docs).
pub async fn involved_projects(db: &Db, user: UserId) -> Result<InvolvedProjects, ForgeError> {
    const QUERY: &str = "review.user_activity.projects";
    let sql = involved_sql();
    let cap = i64::try_from(MAX_MEMBERSHIP_CANDIDATES + 1).unwrap_or(i64::MAX);
    let mut ids: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(&sql)
                .bind(user.get())
                .bind(CAP)
                .bind(cap)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    let truncated = ids.len() > MAX_MEMBERSHIP_CANDIDATES;
    ids.truncate(MAX_MEMBERSHIP_CANDIDATES);
    Ok(InvolvedProjects {
        ids: ids.into_iter().map(ProjectId::new).collect(),
        truncated,
    })
}

/// One page of the events of `projects` and `types`: `seq` below `before`
/// when given, newest first, at most `limit` (capped at [`MAX_PAGE`]).
/// Nothing is read for no project or no type.
pub async fn activity_page(
    db: &Db,
    projects: &[ProjectId],
    types: &[&str],
    before: Option<i64>,
    limit: usize,
) -> Result<Vec<ActivityRow>, ForgeError> {
    const QUERY: &str = "review.user_activity.page";
    if projects.is_empty() || types.is_empty() {
        return Ok(Vec::new());
    }
    let sql = page_sql();
    let ids: Vec<i64> = projects.iter().map(|p| p.get()).collect();
    let types: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
    let limit = i64::try_from(limit.clamp(1, MAX_PAGE)).unwrap_or(1);
    db.within(QUERY, async {
        sqlx::query_as(&sql)
            .bind(ids)
            .bind(types)
            .bind(before)
            .bind(limit)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
