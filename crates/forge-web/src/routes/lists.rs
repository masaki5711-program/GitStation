//! `GET|HEAD /api/v1/projects` (`ProjectList`) and `GET|HEAD /api/v1/groups`
//! (`GroupList`), M1 phase 8b MR7b: keyset-paginated lists of what the
//! caller may read.
//!
//! - The list itself is decided on the instance (any active signed-in user,
//!   `read_api` on a token): the registry proves `ProjectList` / `GroupList`
//!   before the handler runs, so an anonymous or disabled caller loads
//!   nothing.
//! - **Every row is decided by `authorize()`**, never by SQL (plan option
//!   L1): the loader returns the next rows in id order whatever the caller
//!   may see; a project shows only when `prove(ProjectRead)` allows it (so a
//!   Guest on a private project, a project pending deletion and an
//!   invisible one are all left out), a group only when
//!   `authorize(GroupRead)` does.
//! - **Scan budget**: one page reads at most [`SCAN_BUDGET`] rows (one
//!   statement, `SCAN_BUDGET + 1` rows: the extra one only says whether more
//!   exist). When the budget ends before `limit` visible rows, the page is
//!   short, with `has_more: true` and a cursor after the last row scanned.
//! - **Opaque cursor** ([`forge_core::cursor`]): the position is the last
//!   id consumed (shown or scanned), sealed with AES-256-GCM for this list
//!   and this caller (`purpose = "<list>:user=<id>"`), so it reveals no
//!   invisible id and a cursor of another caller or list is `FG-WEB-0006
//!   reason=cursor_invalid` (`rule=tag`); a payload that is not one
//!   positive id is `rule=payload`.
//! - Parameters: `limit` (1 ..= 100, default 20, a plain decimal) and
//!   `cursor`; anything else, a repeated or malformed one is `FG-WEB-0006
//!   reason=param_invalid` (`param=limit|cursor|query`), decided after the
//!   list's own authorization and before any lookup.
//! - Stable under concurrent inserts: ids only grow, so a row created after
//!   a cursor was handed out appears on a later page, and no row before the
//!   cursor is repeated.
//!
//! The answer is `{items, next_cursor, has_more}`; a project item is
//! [`crate::routes::projects::project_view`]'s body, the one builder `GET /api/v1/projects/{id}` uses.

use std::sync::Arc;

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{authorize, prove, Action, InstanceCtx, Principal, ProjectCtx, Resource};
use forge_core::{
    parse_positive_id, CursorKeys, GroupId, GroupListing, GroupPageLoader, ProjectId,
    ProjectPageLoader, ProjectRecord, Role, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::dispatch::loaded_project;
use crate::registry::{Dispatched, QueryParam};
use crate::routes::projects::{project_body, ProjectBody};

/// `GET /api/v1/projects`.
pub const PROJECTS_PATH: &str = "/api/v1/projects";
/// `GET /api/v1/groups`.
pub const GROUPS_PATH: &str = "/api/v1/groups";

/// The most rows one page scans (plan L1: switch to an SQL superset only
/// past ~10k projects per scan, ADR-0010 amendment then).
pub const SCAN_BUDGET: u32 = 1000;
/// `limit` without the parameter.
pub const DEFAULT_LIMIT: usize = 20;
/// The largest `limit`.
pub const MAX_LIMIT: usize = 100;

/// The query parameters of both lists (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "limit",
        description: "Rows per page, 1..=100 (default 20); a page can be shorter when its scan budget ends first",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "cursor",
        description: "The previous page's next_cursor (opaque; bound to this list and this caller)",
        integer_range: None,
    },
];

/// The page loaders and the cursor keys (`forge serve` passes the database
/// and `api.cursor_keys_file`). Without them a list route answers
/// `FG-SYS-0001 reason=lists_unconfigured`.
pub struct Lists {
    pub projects: Arc<dyn ProjectPageLoader>,
    pub groups: Arc<dyn GroupPageLoader>,
    pub keys: Arc<CursorKeys>,
    /// Rows scanned per page: [`SCAN_BUDGET`] (tests lower it).
    pub scan_budget: u32,
}

impl Lists {
    pub fn new(
        projects: Arc<dyn ProjectPageLoader>,
        groups: Arc<dyn GroupPageLoader>,
        keys: Arc<CursorKeys>,
    ) -> Self {
        Self {
            projects,
            groups,
            keys,
            scan_budget: SCAN_BUDGET,
        }
    }

    /// Scan `budget` rows per page instead (1 ..= [`SCAN_BUDGET`]; tests).
    #[must_use]
    pub fn with_scan_budget(mut self, budget: u32) -> Self {
        self.scan_budget = budget.clamp(1, SCAN_BUDGET);
        self
    }
}

/// A page of projects.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectPage {
    pub items: Vec<ProjectBody>,
    /// Pass as `cursor` for the next page; `null` on the last one.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// More rows may follow (a page can be short, even empty, when its
    /// scan budget ended first).
    pub has_more: bool,
}

/// A group as a list shows it.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ListedGroup {
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// The current full path (`group/sub`).
    pub path: String,
    /// The path's last segment.
    pub name: String,
    pub display_name: String,
    pub description: String,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// The caller's effective role (`guest` … `owner`); `null` without one.
    #[schema(required = true)]
    pub role: Option<String>,
}

/// A page of groups.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupPage {
    pub items: Vec<ListedGroup>,
    /// Pass as `cursor` for the next page; `null` on the last one.
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    /// More rows may follow (a page can be short, even empty, when its
    /// scan budget ended first).
    pub has_more: bool,
}

/// The two lists (names the cursor purpose and the timeout operation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Projects,
    Groups,
}

impl List {
    fn name(self) -> &'static str {
        match self {
            Self::Projects => "projects.list",
            Self::Groups => "groups.list",
        }
    }

    fn operation(self) -> &'static str {
        match self {
            Self::Projects => "project_page",
            Self::Groups => "group_page",
        }
    }

    /// The cursor's purpose: this list, this caller.
    fn purpose(self, viewer: UserId) -> String {
        format!("{}:user={}", self.name(), viewer.get())
    }
}

/// The query of a list request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListQuery {
    pub(crate) limit: usize,
    pub(crate) cursor: Option<String>,
}

/// `limit` and `cursor`, strictly: unknown or repeated parameters, a
/// `limit` that is not a plain decimal in 1 ..= 100 and an empty cursor are
/// `FG-WEB-0006 reason=param_invalid` (the value is never logged).
pub(crate) fn parse_query(query: Option<&str>) -> Result<ListQuery, ForgeError> {
    let mut limit: Option<&str> = None;
    let mut cursor: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let slot = match key {
            "limit" => &mut limit,
            "cursor" => &mut cursor,
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            let param = if key == "limit" { "limit" } else { "cursor" };
            return Err(param_invalid(param, "repeated"));
        }
    }
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_LIMIT)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let cursor = match cursor {
        None => None,
        Some("") => return Err(param_invalid("cursor", "empty")),
        Some(raw) => Some(raw.to_owned()),
    };
    Ok(ListQuery { limit, cursor })
}

/// The position a cursor holds: one positive id (8 bytes, big endian).
fn open_cursor(
    keys: &CursorKeys,
    list: List,
    viewer: UserId,
    cursor: Option<&str>,
) -> Result<Option<i64>, ForgeError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let payload = keys.decode(&list.purpose(viewer), cursor)?;
    let id = <[u8; 8]>::try_from(payload.as_slice())
        .ok()
        .map(i64::from_be_bytes)
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::RequestInvalid)
                .with_context("reason", "cursor_invalid")
                .with_context("rule", "payload")
        })?;
    Ok(Some(id))
}

/// One page as decided: the shown items, where the next page starts (the
/// last id consumed) and whether more rows may follow.
#[derive(Debug, PartialEq)]
pub(crate) struct Paged<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next_after: Option<i64>,
    pub(crate) has_more: bool,
}

/// Walk `rows` (ascending ids after `after`, at most `budget + 1`) and keep
/// what `show` lets through, until `limit` are kept or `budget` rows are
/// consumed. `show` returns `Ok(None)` for a row `authorize()` denied and
/// `Err` only for a failure (which fails the page). Pure.
pub(crate) fn paginate<R, T>(
    rows: &[R],
    after: Option<i64>,
    limit: usize,
    budget: usize,
    id: impl Fn(&R) -> i64,
    mut show: impl FnMut(&R) -> Result<Option<T>, ForgeError>,
) -> Result<Paged<T>, ForgeError> {
    let mut items = Vec::with_capacity(limit.min(rows.len()));
    let mut last = after;
    let mut consumed = 0usize;
    for row in rows.iter().take(budget) {
        let row_id = id(row);
        // The loader's contract: strictly ascending after the position. A
        // row out of order could repeat or skip rows: never page on it.
        if last.is_some_and(|l| row_id <= l) {
            return Err(ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "page_out_of_order")
                .with_context("id", row_id));
        }
        last = Some(row_id);
        consumed += 1;
        if let Some(item) = show(row)? {
            items.push(item);
            if items.len() == limit {
                break;
            }
        }
    }
    let has_more = rows.len() > consumed;
    Ok(Paged {
        items,
        next_after: if has_more { last } else { None },
        has_more,
    })
}

/// A row `authorize()` denied is left out; anything that is not a denial
/// (a resource that does not fit the action) fails the page.
fn denied_or_failed(e: ForgeError) -> Result<(), ForgeError> {
    if e.code() == ErrorCode::Internal {
        Err(e)
    } else {
        Ok(())
    }
}

fn unconfigured() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "lists_unconfigured")
}

/// One loader call, bounded on the client side (forge-db bounds its own
/// statement): `FG-WEB-0002 operation=project_page|group_page`.
async fn bounded<T>(
    state: &AppState,
    list: List,
    call: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    let limit = state.lookup_timeout;
    match tokio::time::timeout(limit, call).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", list.operation())
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// The caller (`prove(ProjectList|GroupList)` allowed, so there is one).
fn viewer(principal: &Principal) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "list_viewer_missing")
    })
}

/// Seal the next position for this list and caller.
fn next_cursor(
    keys: &CursorKeys,
    list: List,
    viewer: UserId,
    next_after: Option<i64>,
) -> Result<Option<String>, ForgeError> {
    next_after
        .map(|id| keys.encode(&list.purpose(viewer), &id.to_be_bytes()))
        .transpose()
}

async fn project_page(
    state: &AppState,
    raw_query: Option<&str>,
    principal: &Principal,
) -> Result<ProjectPage, ForgeError> {
    let lists = state.lists.as_deref().ok_or_else(unconfigured)?;
    let query = parse_query(raw_query)?;
    let viewer = viewer(principal)?;
    let after = open_cursor(&lists.keys, List::Projects, viewer, query.cursor.as_deref())?;
    let rows: Vec<ProjectRecord> = bounded(
        state,
        List::Projects,
        lists.projects.project_page(
            after.map(ProjectId::new),
            viewer,
            lists.scan_budget.saturating_add(1),
        ),
    )
    .await?;
    let budget = usize::try_from(lists.scan_budget).unwrap_or(usize::MAX);
    let paged = paginate(
        &rows,
        after,
        query.limit,
        budget,
        |r| r.id.get(),
        |record| {
            let ctx = ProjectCtx::new(loaded_project(record.id, record.access));
            match prove(principal, Action::ProjectRead, ctx) {
                Ok(proof) => project_body(&proof, record).map(Some),
                Err(e) => denied_or_failed(e).map(|()| None),
            }
        },
    )?;
    Ok(ProjectPage {
        next_cursor: next_cursor(&lists.keys, List::Projects, viewer, paged.next_after)?,
        has_more: paged.has_more,
        items: paged.items,
    })
}

async fn group_page(
    state: &AppState,
    raw_query: Option<&str>,
    principal: &Principal,
) -> Result<GroupPage, ForgeError> {
    let lists = state.lists.as_deref().ok_or_else(unconfigured)?;
    let query = parse_query(raw_query)?;
    let viewer = viewer(principal)?;
    let after = open_cursor(&lists.keys, List::Groups, viewer, query.cursor.as_deref())?;
    let rows: Vec<GroupListing> = bounded(
        state,
        List::Groups,
        lists.groups.group_page(
            after.map(GroupId::new),
            viewer,
            lists.scan_budget.saturating_add(1),
        ),
    )
    .await?;
    let budget = usize::try_from(lists.scan_budget).unwrap_or(usize::MAX);
    let paged = paginate(
        &rows,
        after,
        query.limit,
        budget,
        |g| g.id.get(),
        |group| {
            let resource = Resource::Group {
                role: group.viewer_role,
            };
            match authorize(principal, Action::GroupRead, &resource) {
                Ok(()) => Ok(Some(group_view(group))),
                Err(e) => denied_or_failed(e).map(|()| None),
            }
        },
    )?;
    Ok(GroupPage {
        next_cursor: next_cursor(&lists.keys, List::Groups, viewer, paged.next_after)?,
        has_more: paged.has_more,
        items: paged.items,
    })
}

/// A group's list item (shown only after `authorize(GroupRead)` allowed).
fn group_view(group: &GroupListing) -> ListedGroup {
    ListedGroup {
        id: group.id.get(),
        namespace_id: group.namespace_id.get(),
        path: group.path.full_path(),
        name: group
            .path
            .segments()
            .last()
            .map(|s| s.as_str().to_owned())
            .unwrap_or_default(),
        display_name: group.display_name.clone(), // clone: the row is borrowed
        description: group.description.clone(),
        created_at: group.created_at.clone(),
        role: group.viewer_role.map(|r| Role::as_str(r).to_owned()),
    }
}

/// The JSON answer, never cached by a shared cache (it depends on who asks).
fn answer<T: Serialize>(
    request: &Dispatched<InstanceCtx>,
    page: Result<T, ForgeError>,
) -> Response {
    let page = match page {
        Ok(page) => page,
        Err(e) => return request.ctx().fail(e).into_response(),
    };
    let mut res = Json(page).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}

/// `GET|HEAD /api/v1/projects`.
pub async fn projects(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    // Owned copies: the request (its body is not Sync) stays out of the await.
    let (query, principal) = (
        request.raw_query().map(str::to_owned),
        request.principal().clone(),
    );
    let page = project_page(&state, query.as_deref(), &principal).await;
    answer(&request, page)
}

/// `GET|HEAD /api/v1/groups`.
pub async fn groups(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    // Owned copies: the request (its body is not Sync) stays out of the await.
    let (query, principal) = (
        request.raw_query().map(str::to_owned),
        request.principal().clone(),
    );
    let page = group_page(&state, query.as_deref(), &principal).await;
    answer(&request, page)
}

#[cfg(test)]
#[path = "lists_tests.rs"]
mod tests;
