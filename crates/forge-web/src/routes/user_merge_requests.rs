//! The merge requests of the signed-in person's own work across projects
//! (plan 15 MR 6), for the "My work" home:
//!
//! - `GET /api/v1/user/merge-requests?scope=authored|approvable|attention
//!   &before=&limit=` (`UserMergeRequestList`, `read_api`): one page of a
//!   scope, newest first. Only rows whose merge request the caller may read
//!   now; a page may be shorter than `limit` (hidden rows are skipped) and
//!   `next_before` says whether more rows may follow.
//! - `GET /api/v1/user/merge-requests/counts` (`UserMergeRequestList`): the
//!   visible rows of every scope, each at most 100 (`capped`); a count
//!   that stopped at its scan bound with candidates left says so
//!   (`truncated`) instead of undercounting silently.
//!
//! The scopes are forge-review's (`user_lists`): `authored` (open, the
//! caller's own), `approvable` (open, not draft, the caller may approve it
//! now and has not approved its current head; never the caller's own),
//! `attention` (the caller's open ones with a conflict, a predicted
//! conflict or a failed required check on the current head). The routes
//! have no user in them: the lists are always the caller's own (another
//! user's are not found, administrators included). A strict query:
//! `FG-WEB-0006 reason=param_invalid`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::UserMergeRequestsCtx;
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{
    Review, UserListCount, UserListCounts, UserListItem, UserListQuery, UserListScope, MAX_PAGE,
};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::merge_requests::UserNameBody;
use crate::routes::write::{absent_because, bounded, json, no_store};

/// `GET` one page of a scope.
pub const LIST: &str = "/api/v1/user/merge-requests";
/// `GET` the counts of every scope.
pub const COUNTS: &str = "/api/v1/user/merge-requests/counts";
/// The default page.
const DEFAULT_LIMIT: usize = 20;

/// The query parameters of the list (`openapi.json`).
pub const LIST_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "scope",
        description: "Required: authored (open, the caller's own), approvable (open, not draft, the caller may approve its current head and has not yet; never the caller's own) or attention (the caller's open ones with a conflict, a predicted conflict or a failed required check)",
        integer_range: None,
    },
    QueryParam {
        name: "before",
        description: "Only merge requests with a smaller id (next_before of the previous page)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Rows read per page, 1..=100 (default 20), newest first; hidden rows are skipped, so a page may hold fewer",
        integer_range: Some((1, 100)),
    },
];

/// One merge request of the caller's lists.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserMergeRequestBody {
    /// The merge request's global id (the keyset of `before`).
    #[schema(minimum = 1)]
    pub id: i64,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's path now (`group/project`).
    pub project_path: String,
    /// The project's own number of it.
    #[schema(minimum = 1)]
    pub iid: i64,
    pub title: String,
    /// `open` (every scope lists open merge requests only).
    pub state: String,
    pub draft: bool,
    /// The branch name without `refs/heads/`.
    pub source_branch: String,
    pub target_branch: String,
    #[schema(minimum = 1)]
    pub author_id: i64,
    pub author: UserNameBody,
    /// The head commit of the latest version (`null` until one is recorded).
    #[schema(required = true)]
    pub head_sha: Option<String>,
    /// Why it waits on its author: `conflict`, `conflict_predicted`,
    /// `required_check_failed` (empty: nothing known; never empty on
    /// `attention`).
    pub attention: Vec<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    pub updated_at: String,
}

/// A page of one scope.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserMergeRequestList {
    /// `authored`, `approvable` or `attention`.
    pub scope: String,
    pub merge_requests: Vec<UserMergeRequestBody>,
    /// Pass as `before` for the next (older) page; `null`: no more.
    #[schema(required = true)]
    pub next_before: Option<i64>,
}

/// The visible rows of one scope.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserMergeRequestCount {
    /// At most 100 (counted over at most 1000 candidates, newest first).
    pub count: i64,
    /// 100 visible rows were found (show "99+"); hidden rows never set it.
    pub capped: bool,
    /// The scan stopped after 1000 candidates with more left (hidden rows
    /// among them): `count` is a lower bound (show "n+"). Never true with
    /// `capped`.
    pub truncated: bool,
}

/// The counts of the caller's lists.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserMergeRequestCounts {
    pub authored: UserMergeRequestCount,
    pub approvable: UserMergeRequestCount,
    pub attention: UserMergeRequestCount,
}

fn branch(name: &forge_core::RefName) -> String {
    name.branch().unwrap_or(name.as_str()).to_owned()
}

fn item_body(item: UserListItem) -> UserMergeRequestBody {
    let UserListItem {
        merge_request: mr,
        project_path,
        reasons,
    } = item;
    UserMergeRequestBody {
        id: mr.id.get(),
        project_id: mr.project_id.get(),
        project_path,
        iid: mr.iid,
        source_branch: branch(&mr.source_branch),
        target_branch: branch(&mr.target_branch),
        state: mr.state.as_str().to_owned(),
        draft: mr.draft,
        author_id: mr.author.get(),
        author: mr.author_name.into(),
        head_sha: mr.head_sha,
        attention: reasons.iter().map(|r| r.as_str().to_owned()).collect(),
        title: mr.title,
        created_at: mr.created_at,
        updated_at: mr.updated_at,
    }
}

fn count_body(count: UserListCount) -> UserMergeRequestCount {
    UserMergeRequestCount {
        count: count.count,
        capped: count.capped,
        truncated: count.truncated,
    }
}

fn counts_body(counts: UserListCounts) -> UserMergeRequestCounts {
    UserMergeRequestCounts {
        authored: count_body(counts.authored),
        approvable: count_body(counts.approvable),
        attention: count_body(counts.attention),
    }
}

/// The review commands; `FG-SYS-0001 reason=review_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

fn respond(ctx: &crate::Ctx, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// Both routes: the caller's own lists.
pub struct OwnMergeRequests;

impl Loader<AppState> for OwnMergeRequests {
    type Ctx = UserMergeRequestsCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, UserMergeRequestsCtx> {
        Box::pin(async move {
            // A runner's or a job's credential has no user (the guard lets
            // it through by design): there is no list of theirs, and
            // `authorize()` refuses it on the unresolved context (its CI
            // arm first) instead of an internal error here.
            let Some(me) = viewer else {
                return Ok(absent_because("user_merge_requests_without_user"));
            };
            Ok(Lookup::Found {
                ctx: UserMergeRequestsCtx::new(me),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `scope`, `before` and `limit`, strictly.
fn page_query(query: Option<&str>) -> Result<UserListQuery, ForgeError> {
    let mut scope: Option<&str> = None;
    let mut before: Option<&str> = None;
    let mut limit: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "scope" => (&mut scope, "scope"),
            "before" => (&mut before, "before"),
            "limit" => (&mut limit, "limit"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let scope = match scope {
        None => return Err(param_invalid("scope", "missing")),
        Some(raw) => {
            UserListScope::parse(raw).ok_or_else(|| param_invalid("scope", "unknown_value"))?
        }
    };
    let before = before
        .map(|raw| {
            parse_positive_id(raw).ok_or_else(|| param_invalid("before", "not_positive_decimal"))
        })
        .transpose()?;
    let limit = match limit {
        None => DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_PAGE)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    Ok(UserListQuery {
        scope,
        before,
        limit,
    })
}

/// `GET /api/v1/user/merge-requests` → `200`.
pub async fn list(state: AppState, request: Dispatched<UserMergeRequestsCtx>) -> Response {
    let query = page_query(request.raw_query());
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let query = query?;
        let page = bounded(
            &state,
            "user_merge_requests_list",
            review(&state)?.user_merge_requests(&w.proof, query),
        )
        .await?;
        json(UserMergeRequestList {
            scope: query.scope.as_str().to_owned(),
            merge_requests: page.items.into_iter().map(item_body).collect(),
            next_before: page.next_before,
        })
    }
    .await;
    respond(&w.ctx, built)
}

/// `GET /api/v1/user/merge-requests/counts` → `200`.
pub async fn counts(state: AppState, request: Dispatched<UserMergeRequestsCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let counts = bounded(
            &state,
            "user_merge_requests_counts",
            review(&state)?.user_merge_request_counts(&w.proof),
        )
        .await?;
        json(counts_body(counts))
    }
    .await;
    respond(&w.ctx, built)
}

#[cfg(test)]
mod unit {
    use super::*;

    fn param_of(e: &ForgeError) -> &str {
        e.context()
            .iter()
            .find(|(key, _)| *key == "param")
            .map_or("", |(_, v)| v.as_str())
    }

    #[test]
    fn the_page_query_is_strict() {
        let q = page_query(Some("scope=approvable&before=9&limit=5")).expect("query");
        assert_eq!(q.scope, UserListScope::Approvable);
        assert_eq!(q.before, Some(9));
        assert_eq!(q.limit, 5);
        let q = page_query(Some("scope=attention")).expect("query");
        assert_eq!((q.before, q.limit), (None, DEFAULT_LIMIT));
        for (raw, param) in [
            (None, "scope"),
            (Some("scope=mine"), "scope"),
            (Some("scope="), "scope"),
            (Some("scope=authored&scope=authored"), "scope"),
            (Some("scope=authored&limit=0"), "limit"),
            (Some("scope=authored&limit=101"), "limit"),
            (Some("scope=authored&before=-1"), "before"),
            (Some("scope=authored&page=2"), "query"),
        ] {
            let e = page_query(raw).expect_err("refused");
            assert_eq!(e.code(), ErrorCode::RequestInvalid, "{raw:?}");
            assert_eq!(e.code().id(), "FG-WEB-0006", "{raw:?}");
            assert_eq!(param_of(&e), param, "{raw:?}");
        }
    }
}
