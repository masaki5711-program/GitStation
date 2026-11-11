//! The recent events of the projects the signed-in person is involved in
//! (plan 15 MR 8), for the "My work" activity page and the home's recent
//! updates:
//!
//! - `GET /api/v1/user/activity?before=&limit=` (`ActivityRead`,
//!   `read_api`; people only): one page, newest first. Only the events
//!   forge-review's `activity` allows — the shown types, each decided
//!   again on the event's project and visibility class — with only the
//!   allowed fields; a page may be shorter than `limit` (hidden rows are
//!   skipped) and `next_before` says whether more rows may follow.
//!
//! The route has no user in it: the list is always the caller's own
//! (another user's is not found, administrators included). Every answer
//! logs one `forge.user_activity.read` line with the request id, the rows
//! read and how many of them were hidden (`rows_hidden`: a count, never
//! what they were). A strict query: `FG-WEB-0006 reason=param_invalid`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::UserActivityCtx;
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_review::{ActivityItem, ActivityQuery, Review, MAX_PAGE};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::write::{absent_because, bounded, json, no_store};

/// `GET` one page.
pub const ACTIVITY: &str = "/api/v1/user/activity";
/// The default page (plan 15 §3.3).
const DEFAULT_LIMIT: usize = 30;

/// The query parameters (`openapi.json`).
pub const ACTIVITY_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "before",
        description: "Only events with a smaller seq (next_before of the previous page)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Events read per page, 1..=100 (default 30), newest first; hidden events are skipped, so a page may hold fewer",
        integer_range: Some((1, 100)),
    },
];

/// Who caused an event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserActivityActorBody {
    pub username: String,
    pub display_name: String,
}

/// The merge request an event is about.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserActivityMergeRequestBody {
    /// The project's own number of it.
    #[schema(minimum = 1)]
    pub iid: i64,
    /// Its current title (`null`: not found).
    #[schema(required = true)]
    pub title: Option<String>,
}

/// One event of the caller's activity.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserActivityEventBody {
    /// The event's position (the keyset of `before`).
    #[schema(minimum = 1)]
    pub seq: i64,
    /// One of project.created, repository.pushed, merge_request.opened,
    /// merge_request.closed, merge_request.reopened,
    /// merge_request.commented, merge_request.merged,
    /// merge_request.approved (more may be added: show an unknown type
    /// with a general sentence).
    #[serde(rename = "type")]
    pub event_type: String,
    /// UTC, RFC 3339 with milliseconds.
    pub occurred_at: String,
    #[schema(minimum = 1)]
    pub project_id: i64,
    /// The project's path now (`group/project`).
    pub project_path: String,
    /// `null`: forge itself, or an account that is gone.
    #[schema(required = true)]
    pub actor: Option<UserActivityActorBody>,
    /// Merge request events only (`null` otherwise).
    #[schema(required = true)]
    pub merge_request: Option<UserActivityMergeRequestBody>,
    /// `repository.pushed` only: how many refs the push changed.
    #[schema(required = true, minimum = 0)]
    pub ref_count: Option<i64>,
}

/// A page of the caller's activity.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserActivityList {
    pub events: Vec<UserActivityEventBody>,
    /// Pass as `before` for the next (older) page; `null`: no more.
    #[schema(required = true)]
    pub next_before: Option<i64>,
    /// The caller is involved in more than 500 projects: the events of the
    /// oldest projects were not read.
    pub truncated: bool,
}

fn item_body(item: ActivityItem) -> UserActivityEventBody {
    UserActivityEventBody {
        seq: item.seq,
        event_type: item.event_type.as_str().to_owned(),
        occurred_at: item.occurred_at,
        project_id: item.project_id.get(),
        project_path: item.project_path,
        actor: item.actor.map(|a| UserActivityActorBody {
            username: a.username,
            display_name: a.display_name,
        }),
        merge_request: item.merge_request.map(|m| UserActivityMergeRequestBody {
            iid: m.iid,
            title: m.title,
        }),
        ref_count: item.ref_count,
    }
}

/// The review commands; `FG-SYS-0001 reason=review_unconfigured` when the
/// server was built without them (`forge serve` always sets them).
fn review(state: &AppState) -> Result<&Review, ForgeError> {
    state.review.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "review_unconfigured")
    })
}

/// The caller's own activity.
pub struct OwnActivity;

impl Loader<AppState> for OwnActivity {
    type Ctx = UserActivityCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, UserActivityCtx> {
        Box::pin(async move {
            // A runner's or a job's credential has no user (the guard lets
            // it through by design): there is no activity of theirs, and
            // `authorize()` refuses it on the unresolved context (its CI
            // arm first) instead of an internal error here.
            let Some(me) = viewer else {
                return Ok(absent_because("user_activity_without_user"));
            };
            Ok(Lookup::Found {
                ctx: UserActivityCtx::new(me),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `before` and `limit`, strictly.
fn page_query(query: Option<&str>) -> Result<ActivityQuery, ForgeError> {
    let mut before: Option<&str> = None;
    let mut limit: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "before" => (&mut before, "before"),
            "limit" => (&mut limit, "limit"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
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
    Ok(ActivityQuery { before, limit })
}

/// `GET /api/v1/user/activity` → `200`.
pub async fn list(state: AppState, request: Dispatched<UserActivityCtx>) -> Response {
    let query = page_query(request.raw_query());
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let query = query?;
        let page = bounded(
            &state,
            "user_activity",
            review(&state)?.user_activity(&w.proof, query),
        )
        .await?;
        // Hidden rows are counted, never described (plan 15 §3.4).
        tracing::info!(
            target: "forge_web::user_activity",
            request_id = %w.ctx.request_id,
            projects = page.projects,
            projects_hidden = page.projects_hidden,
            projects_truncated = page.truncated,
            rows_scanned = page.rows_scanned,
            rows_hidden = page.rows_hidden,
            "forge.user_activity.read"
        );
        json(UserActivityList {
            events: page.items.into_iter().map(item_body).collect(),
            next_before: page.next_before,
            truncated: page.truncated,
        })
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
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
        let q = page_query(Some("before=9&limit=5")).expect("query");
        assert_eq!((q.before, q.limit), (Some(9), 5));
        let q = page_query(None).expect("query");
        assert_eq!((q.before, q.limit), (None, DEFAULT_LIMIT));
        let q = page_query(Some("limit=100")).expect("query");
        assert_eq!(q.limit, 100);
        for (raw, param) in [
            (Some("limit=0"), "limit"),
            (Some("limit=101"), "limit"),
            (Some("limit="), "limit"),
            (Some("limit=1&limit=1"), "limit"),
            (Some("before=-1"), "before"),
            (Some("before=0"), "before"),
            (Some("before=x"), "before"),
            (Some("scope=authored"), "query"),
        ] {
            let e = page_query(raw).expect_err("refused");
            assert_eq!(e.code(), ErrorCode::RequestInvalid, "{raw:?}");
            assert_eq!(e.code().id(), "FG-WEB-0006", "{raw:?}");
            assert_eq!(param_of(&e), param, "{raw:?}");
        }
    }
}
