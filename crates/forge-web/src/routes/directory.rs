//! Finding and administering users (M2 MR6, plan 10 §5):
//!
//! - `GET /api/v1/users?search=` (`UserSearch`, `read_api`): active users
//!   whose username starts with the term or whose display name contains
//!   it — **names only** (id, username, display name), at most 20, the
//!   term 2..=64 characters. For the member picker; every active
//!   signed-in user may ask.
//! - `GET /api/v1/admin/users` (`UserAdminList`, `read_api` + administrator
//!   rights): every user in id order, keyset-paginated with an opaque
//!   cursor sealed for this list, this caller and the `state` filter
//!   (`active` / `disabled`; another filter's cursor fails the tag).
//! - `POST /api/v1/admin/users/{id}/disable` (`UserDisable`, `api` +
//!   administrator rights, never an agent): the administration guard always
//!   holds (`FG-AUTH-0009` `last_admin` / `last_owner`; there is no force
//!   over HTTP, and the request has no body to ask for one). The user's
//!   tokens and sessions end in the same change.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{InstanceCtx, UserTargetCtx};
use forge_core::{parse_positive_id, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::accounts::{AdminUserRow, DisableOutcome, UserStateFilter};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};
use crate::routes::namespaces::percent_decode;
use crate::routes::write::{bounded, hosting, json, no_store, write_request};

/// `GET /api/v1/users?search=`.
pub const SEARCH: &str = "/api/v1/users";
/// `GET /api/v1/admin/users`.
pub const ADMIN_USERS: &str = "/api/v1/admin/users";
/// `POST /api/v1/admin/users/{id}/disable`.
pub const DISABLE: &str = "/api/v1/admin/users/{id}/disable";
const ADMIN_PREFIX: &str = "/api/v1/admin/users/";
const DISABLE_SUFFIX: &str = "/disable";

/// `limit` without the parameter, and its largest value.
pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 100;

pub const SEARCH_QUERY: &[QueryParam] = &[QueryParam {
    name: "search",
    description: "2..=64 characters: a username prefix or part of a display name",
    integer_range: None,
}];

pub const ADMIN_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "limit",
        description: "Rows per page, 1..=100 (default 50)",
        integer_range: Some((1, 100)),
    },
    QueryParam {
        name: "cursor",
        description: "The previous page's next_cursor (opaque; bound to this list, this caller and the state filter)",
        integer_range: None,
    },
    QueryParam {
        name: "state",
        description: "active or disabled (default: every user)",
        integer_range: None,
    },
];

/// One search match: names only.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserName {
    #[schema(minimum = 1)]
    pub id: i64,
    pub username: String,
    pub display_name: String,
}

/// The matches of a search.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UserNames {
    pub items: Vec<UserName>,
}

/// One user as the administration sees it (no e-mail or other personal
/// identifiers: the read pool never has them).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminUser {
    #[schema(minimum = 1)]
    pub id: i64,
    /// `human`, `service_account` or `bot`.
    pub kind: String,
    pub username: String,
    pub display_name: String,
    /// `active` or `disabled`.
    pub state: String,
    pub is_admin: bool,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// A page of users.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AdminUserPage {
    pub items: Vec<AdminUser>,
    #[schema(required = true)]
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

/// What a disable did.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Disabled {
    #[schema(minimum = 1)]
    pub user_id: i64,
    /// `disabled`, or `already_disabled` (nothing was written).
    pub outcome: String,
}

/// The query's parameters, each at most once; `allowed` names the keys.
fn params<'q>(
    query: Option<&'q str>,
    allowed: &[&'static str],
) -> Result<Vec<(&'static str, &'q str)>, ForgeError> {
    let mut out: Vec<(&'static str, &'q str)> = Vec::new();
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(name) = allowed.iter().copied().find(|a| *a == key) else {
            return Err(param_invalid("query", "unknown_parameter"));
        };
        if out.iter().any(|(k, _)| *k == name) {
            return Err(param_invalid(name, "repeated"));
        }
        out.push((name, value));
    }
    Ok(out)
}

fn value<'q>(params: &[(&'static str, &'q str)], name: &str) -> Option<&'q str> {
    params.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// `GET /api/v1/users?search=` → `200` the names.
pub async fn search(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let ctx = request.into_write().ctx;
    let built = async {
        let params = params(query.as_deref(), &["search"])?;
        let raw = value(&params, "search").ok_or_else(|| param_invalid("search", "missing"))?;
        // A form-encoded `+` is a space in a search term.
        let term = percent_decode(&raw.replace('+', " "))
            .ok_or_else(|| param_invalid("search", "malformed"))?;
        let found = bounded(&state, "user_search", hosting(&state)?.search_users(&term)).await?;
        json(UserNames {
            items: found
                .into_iter()
                .map(|m| UserName {
                    id: m.id.get(),
                    username: m.username,
                    display_name: m.display_name,
                })
                .collect(),
        })
    }
    .await;
    respond(&ctx, StatusCode::OK, built)
}

fn admin_purpose(viewer: UserId, state: UserStateFilter) -> String {
    let state = match state {
        UserStateFilter::All => "all",
        UserStateFilter::Active => "active",
        UserStateFilter::Disabled => "disabled",
    };
    format!("admin_users.list:user={}:state={state}", viewer.get())
}

fn admin_row(row: AdminUserRow) -> AdminUser {
    AdminUser {
        id: row.id.get(),
        kind: row.kind.as_str().to_owned(),
        username: row.username,
        display_name: row.display_name,
        state: if row.active { "active" } else { "disabled" }.to_owned(),
        is_admin: row.is_admin,
        created_at: row.created_at,
    }
}

/// `GET /api/v1/admin/users` → `200` a page.
pub async fn admin_users(state: AppState, request: Dispatched<InstanceCtx>) -> Response {
    let query = request.raw_query().map(str::to_owned);
    let viewer = request.principal().user();
    let ctx = request.into_write().ctx;
    let built = async {
        let params = params(query.as_deref(), &["limit", "cursor", "state"])?;
        let limit = match value(&params, "limit") {
            None => DEFAULT_LIMIT,
            Some(raw) => parse_positive_id(raw)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n <= MAX_LIMIT)
                .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
        };
        let filter = match value(&params, "state") {
            None => UserStateFilter::All,
            Some("active") => UserStateFilter::Active,
            Some("disabled") => UserStateFilter::Disabled,
            Some(_) => return Err(param_invalid("state", "invalid_value")),
        };
        let viewer = viewer.ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "list_viewer_missing")
        })?;
        let lists = state.lists.as_deref().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "lists_unconfigured")
        })?;
        let purpose = admin_purpose(viewer, filter);
        let after = match value(&params, "cursor") {
            None => None,
            Some("") => return Err(param_invalid("cursor", "empty")),
            Some(raw) => {
                let payload = lists.keys.decode(&purpose, raw, lists.cursor_max_age)?;
                let id = <[u8; 8]>::try_from(payload.as_slice())
                    .ok()
                    .map(i64::from_be_bytes)
                    .filter(|id| *id > 0)
                    .ok_or_else(|| {
                        ForgeError::new(ErrorCode::RequestInvalid)
                            .with_context("reason", "cursor_invalid")
                            .with_context("rule", "payload")
                    })?;
                Some(UserId::new(id))
            }
        };
        let mut rows = bounded(
            &state,
            "admin_user_page",
            hosting(&state)?.admin_users(after, filter, limit + 1),
        )
        .await?;
        let has_more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next_cursor = match (has_more, rows.last()) {
            (true, Some(last)) => Some(lists.keys.encode(&purpose, &last.id.get().to_be_bytes())?),
            _ => None,
        };
        json(AdminUserPage {
            items: rows.into_iter().map(admin_row).collect(),
            next_cursor,
            has_more,
        })
    }
    .await;
    respond(&ctx, StatusCode::OK, built)
}

/// `POST /api/v1/admin/users/{id}/disable`: the user.
pub struct UserToDisable;

impl Loader<AppState> for UserToDisable {
    type Ctx = UserTargetCtx;
    fn load<'a>(
        &'a self,
        _: &'a AppState,
        parts: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, UserTargetCtx> {
        Box::pin(async move {
            let id = parts
                .uri
                .path()
                .strip_prefix(ADMIN_PREFIX)
                .and_then(|rest| rest.strip_suffix(DISABLE_SUFFIX))
                .filter(|raw| !raw.contains('/'))
                .and_then(parse_positive_id)
                .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
            Ok(Lookup::Found {
                ctx: UserTargetCtx::new(UserId::new(id)),
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// `POST /api/v1/admin/users/{id}/disable` → `200`.
pub async fn disable(state: AppState, request: Dispatched<UserTargetCtx>) -> Response {
    let w = request.into_write();
    let built = async {
        let outcome = hosting(&state)?
            .disable_user(&w.proof, &write_request(&w))
            .await?;
        let outcome = match outcome {
            DisableOutcome::Disabled => "disabled",
            DisableOutcome::AlreadyDisabled => "already_disabled",
            // The guard is enforced here: forge-db never returns this.
            DisableOutcome::DisabledPastGuard { .. } => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "http_disable_past_guard")
                    .with_context("user", w.proof.id().get()))
            }
        };
        json(Disabled {
            user_id: w.proof.id().get(),
            outcome: outcome.to_owned(),
        })
    }
    .await;
    respond(&w.ctx, StatusCode::OK, built)
}

fn respond(ctx: &crate::Ctx, status: StatusCode, body: Result<Value, ForgeError>) -> Response {
    match body {
        Ok(body) => no_store(status, body),
        Err(e) => ctx.fail(e).into_response(),
    }
}
