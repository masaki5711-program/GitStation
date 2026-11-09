//! The signed-in person's in-app notifications (M3 U4, ADR-0016 Amendment
//! U4):
//!
//! - `GET /api/v1/user/notifications?before=&limit=&unread=`
//!   (`NotificationInboxRead`, `read_api`): newest first; only items whose
//!   merge request the caller may read now. A page may be shorter than
//!   `limit` (hidden items are skipped); `next_before` says whether more
//!   rows may follow.
//! - `GET /api/v1/user/notifications/unread-count` (`NotificationInboxRead`):
//!   the count the bell polls.
//! - `POST /api/v1/user/notifications/{id}/read` (`NotificationInboxUpdate`,
//!   `api`): the item's owner is loaded and decided like a token's — an
//!   item of somebody else and an absent one answer alike (`FG-REPO-0005`).
//!   The answer carries no words of the item (it may no longer be visible).
//! - `POST /api/v1/user/notifications/read-all` (`NotificationInboxUpdate`):
//!   every unread item up to `up_to` (the newest the person saw); `204`,
//!   no count (it would include items the caller may no longer see).
//!
//! The routes have no user in them except the item id: the list is always
//! the caller's own.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::NotificationPreferencesCtx;
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_notify::in_app::{Listed, UnreadCount};
use forge_notify::in_app::{PageQuery, MAX_PAGE};
use forge_notify::Inbox;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::write::{absent, bounded, json, no_store};

/// The web page of the list (the SPA shell).
pub const PAGE: &str = "/-/notifications";
/// `GET` the caller's list.
pub const LIST: &str = "/api/v1/user/notifications";
/// `GET` the unread count.
pub const UNREAD_COUNT: &str = "/api/v1/user/notifications/unread-count";
/// `POST`: mark one read.
pub const READ_ONE: &str = "/api/v1/user/notifications/{id}/read";
/// `POST`: mark all read.
pub const READ_ALL: &str = "/api/v1/user/notifications/read-all";
/// The largest body of `read-all`.
pub const MAX_BODY: usize = 256;
/// The default page.
const DEFAULT_PAGE: i64 = 20;
const PREFIX: &str = "/api/v1/user/notifications/";

/// The query parameters of the list (`openapi.json`).
pub const LIST_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "before",
        description: "Only items with a smaller id (next_before of the previous page)",
        integer_range: Some((1, i64::MAX)),
    },
    QueryParam {
        name: "limit",
        description: "Rows read per page, 1..=100 (default 20), newest first; hidden items are skipped, so a page may hold fewer",
        integer_range: Some((1, MAX_PAGE)),
    },
    QueryParam {
        name: "unread",
        description: "true: only unread items (default false)",
        integer_range: None,
    },
];

/// One in-app notification.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InAppNotificationBody {
    pub id: i64,
    /// `mr_approved`, `mr_merged`, `mr_commented`, `mr_conflict_predicted`.
    pub kind: String,
    pub project_id: i64,
    /// The project's path now (`group/project`).
    pub project_path: String,
    pub merge_request_iid: i64,
    pub merge_request_title: String,
    /// Who did it (`null`: the system).
    #[schema(required = true)]
    pub actor_name: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
    /// `null`: unread.
    #[schema(required = true)]
    pub read_at: Option<String>,
}

/// A page of the caller's notifications.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InAppNotificationList {
    pub items: Vec<InAppNotificationBody>,
    /// Pass as `before` for the next (older) page; `null`: no more.
    #[schema(required = true)]
    pub next_before: Option<i64>,
}

/// The unread count.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UnreadCountBody {
    /// Unread items the caller may see, at most 100 (counted over at most
    /// 1000 unread rows, newest first; older visible ones are not counted).
    pub count: i64,
    /// 100 visible unread items were found (show "99+"); hidden items never
    /// set it.
    pub capped: bool,
}

/// One item marked read.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkedRead {
    pub id: i64,
    /// When it was first read (UTC, RFC 3339).
    pub read_at: String,
}

/// `POST …/read-all`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadAllInput {
    /// Only items up to this id (the newest the person saw); absent: all.
    #[schema(minimum = 1)]
    pub up_to: Option<i64>,
}

fn listed_body(listed: Listed) -> InAppNotificationBody {
    let Listed { item, view } = listed;
    InAppNotificationBody {
        id: item.id,
        kind: item.kind.as_str().to_owned(),
        project_id: item.project.get(),
        project_path: view.project_path,
        merge_request_iid: item.iid,
        merge_request_title: view.title,
        actor_name: item.actor_name,
        created_at: item.created_at,
        read_at: item.read_at,
    }
}

fn count_body(count: UnreadCount) -> UnreadCountBody {
    UnreadCountBody {
        count: count.count,
        capped: count.capped,
    }
}

fn inbox(state: &AppState) -> Result<&Inbox, ForgeError> {
    state.inbox.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "in_app_notifications_unconfigured")
    })
}

/// The caller (the registry's guard admitted an authenticated one).
fn me(viewer: Option<UserId>) -> Result<UserId, ForgeError> {
    viewer.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "notification_route_without_user")
    })
}

fn found<R>(ctx: R) -> Lookup<R> {
    Lookup::Found {
        ctx,
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
    }
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `GET`s and `read-all`: the caller's own notifications.
pub struct OwnInbox;

impl Loader<AppState> for OwnInbox {
    type Ctx = NotificationPreferencesCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, NotificationPreferencesCtx> {
        Box::pin(async move { Ok(found(NotificationPreferencesCtx::new(me(viewer)?))) })
    }
}

impl BodyLoader<AppState, ReadAllInput> for OwnInbox {
    type Ctx = NotificationPreferencesCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        _: &'a Parts,
        _: &'a ReadAllInput,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, NotificationPreferencesCtx> {
        Box::pin(async move { Ok(found(NotificationPreferencesCtx::new(me(viewer)?))) })
    }
}

/// The `{id}` of `…/{id}/read`.
fn item_id(path: &str) -> Option<i64> {
    path.strip_prefix(PREFIX)?
        .strip_suffix("/read")
        .filter(|id| !id.contains('/'))
        .and_then(parse_positive_id)
}

/// `POST …/{id}/read`: the item's owner (absent: not found for everyone).
pub struct ItemToMark;

impl Loader<AppState> for ItemToMark {
    type Ctx = NotificationPreferencesCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, NotificationPreferencesCtx> {
        Box::pin(async move {
            let id = item_id(parts.uri.path())
                .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            let owner =
                bounded(state, "in_app_notification_owner", inbox(state)?.owner(id)).await?;
            Ok(match owner {
                Some(owner) => found(NotificationPreferencesCtx::new(owner)),
                None => absent(),
            })
        })
    }
}

/// `before`, `limit` and `unread`, strictly.
fn page_query(query: Option<&str>) -> Result<PageQuery, ForgeError> {
    let mut before: Option<&str> = None;
    let mut limit: Option<&str> = None;
    let mut unread: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "before" => (&mut before, "before"),
            "limit" => (&mut limit, "limit"),
            "unread" => (&mut unread, "unread"),
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
        None => DEFAULT_PAGE,
        Some(raw) => parse_positive_id(raw)
            .filter(|n| *n <= MAX_PAGE)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let unread_only = match unread {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(param_invalid("unread", "not_boolean")),
    };
    Ok(PageQuery {
        before,
        unread_only,
        limit,
    })
}

/// `GET /api/v1/user/notifications` → `200`.
pub async fn list(state: AppState, request: Dispatched<NotificationPreferencesCtx>) -> Response {
    let query = page_query(request.raw_query());
    // The request (its body is not Sync) stays out of the await.
    let w = request.into_write();
    let built = async {
        let query = query?;
        let page = bounded(
            &state,
            "in_app_notifications_list",
            inbox(&state)?.list(&w.proof, query),
        )
        .await?;
        json(InAppNotificationList {
            items: page.items.into_iter().map(listed_body).collect(),
            next_before: page.next_before,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `GET /api/v1/user/notifications/unread-count` → `200`.
pub async fn unread_count(
    state: AppState,
    request: Dispatched<NotificationPreferencesCtx>,
) -> Response {
    let w = request.into_write();
    let built = async {
        let count = bounded(
            &state,
            "in_app_notifications_unread_count",
            inbox(&state)?.unread_count(&w.proof),
        )
        .await?;
        json(count_body(count))
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/user/notifications/{id}/read` → `200`.
pub async fn mark_read(
    state: AppState,
    request: Dispatched<NotificationPreferencesCtx>,
) -> Response {
    let id = item_id(request.raw_path());
    let w = request.into_write();
    let built = async {
        let id = id.ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
        let item = bounded(
            &state,
            "in_app_notification_mark_read",
            inbox(&state)?.mark_read(&w.proof, id),
        )
        .await?;
        let read_at = item.read_at.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "marked_item_unread")
        })?;
        json(MarkedRead {
            id: item.id,
            read_at,
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

/// `POST /api/v1/user/notifications/read-all` → `204` (no count: the rows
/// marked include items the caller may no longer see).
pub async fn mark_all_read(
    state: AppState,
    request: Dispatched<NotificationPreferencesCtx, ReadAllInput>,
) -> Response {
    let w = request.into_write();
    let done = async {
        let up_to = match w.input.up_to {
            Some(id) if id < 1 => return Err(param_invalid("up_to", "not_positive")),
            other => other,
        };
        bounded(
            &state,
            "in_app_notifications_mark_all_read",
            inbox(&state)?.mark_all_read(&w.proof, up_to),
        )
        .await
    }
    .await;
    match done {
        Ok(marked) => {
            tracing::debug!(
                target: "forge_web",
                event = "forge.notify.in_app_marked_all",
                marked,
                "in-app notifications were marked read"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_page_query_is_strict() {
        assert_eq!(
            page_query(None).expect("default"),
            PageQuery {
                before: None,
                unread_only: false,
                limit: 20
            }
        );
        assert_eq!(
            page_query(Some("before=9&limit=5&unread=true")).expect("page"),
            PageQuery {
                before: Some(9),
                unread_only: true,
                limit: 5
            }
        );
        for bad in [
            "limit=0",
            "limit=101",
            "before=x",
            "before=0",
            "unread=1",
            "x=1",
            "limit=1&limit=2",
        ] {
            assert!(page_query(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_item_id_is_the_segment_before_read() {
        assert_eq!(item_id("/api/v1/user/notifications/7/read"), Some(7));
        for bad in [
            "/api/v1/user/notifications/0/read",
            "/api/v1/user/notifications/x/read",
            "/api/v1/user/notifications/7",
            "/api/v1/user/notifications/7/8/read",
            "/api/v1/user/notifications/read-all",
        ] {
            assert_eq!(item_id(bad), None, "{bad}");
        }
    }
}
