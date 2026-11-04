//! The event feed of an integration (M3 I1c; plan 02 §5.7, §11.2;
//! ADR-0016 §7; docs/api/integration-feed.md, `schema_version` 1, fields
//! only ever added):
//!
//! - `GET /api/v1/integrations/{id}/events?cursor=…&limit=…`
//!   (`IntegrationEventRead`): the events of the integration's bound
//!   projects after the cursor, in order, each with its `work_items`.
//! - `GET /api/v1/integrations/{id}/events/cursor`
//!   (`IntegrationEventCursorRead`): the head position (a resync takes it
//!   first, then the development status, then reads from it).
//!
//! The caller must be that integration's own service account with a
//! `read_api` token: another integration's id, a missing one, a person
//! (administrators included) and a bot are the same 404 (`FG-REPO-0005`);
//! an agent's token, a `Forge-Act-As` header and a disabled integration are
//! 403. What it sees is decided per bound project by
//! `forge_auth::prove_visible` inside forge-integ. A cursor that is not this
//! integration's is 400 `FG-INT-0008`; one the feed cannot continue is 410
//! `FG-INT-0007` with `reason` in the body. The cursor's value is never
//! logged (the access log has the path only).
//!
//! Parameters: `limit` (1 ..= 500, default 100, a plain decimal) and
//! `cursor`; anything else, a repeated or malformed one is `FG-WEB-0006
//! reason=param_invalid` (`param=limit|cursor|query`).

use std::time::Duration;

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::IntegrationFeedCtx;
use forge_core::{parse_positive_id, IntegrationId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::{Actor, ActorUser, EventOut, EventsPage, Feed, FeedQuery, SCHEMA_VERSION};
use serde::Serialize;
use serde_json::Value;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{Dispatched, LoadFuture, Loader, QueryParam};
use crate::routes::protected_refs::found;
use crate::routes::write::{absent, bounded, json, no_store};

/// `GET` the feed.
pub const EVENTS: &str = "/api/v1/integrations/{id}/events";
/// `GET` the head cursor.
pub const CURSOR: &str = "/api/v1/integrations/{id}/events/cursor";
const PREFIX: &str = "/api/v1/integrations/";
/// The request header that asks to act for a person (ADR-0017); never on
/// the feed.
pub const ACT_AS_HEADER: &str = "forge-act-as";
/// The whole read's bound (the snapshot's statements are bounded by the
/// database timeouts too): `FG-WEB-0002 operation=integration_feed`.
const FEED_TIMEOUT: Duration = Duration::from_secs(30);

/// The query parameters of the feed (`openapi.json`).
pub const EVENTS_QUERY: &[QueryParam] = &[
    QueryParam {
        name: "cursor",
        description: "The previous answer's next_cursor, an event's cursor, or GET …/events/cursor (opaque; bound to this integration). Without it the feed starts at the oldest retained event",
        integer_range: None,
    },
    QueryParam {
        name: "limit",
        description: "Events per page, 1..=500 (default 100); a page also ends at about 2 MiB",
        integer_range: Some((1, 500)),
    },
];

/// Who caused an event (no e-mail or employee number without the token's
/// `user_email` / `user_employee_number` scope).
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedUserBody {
    #[schema(minimum = 1)]
    pub id: i64,
    /// The current username (`null`: the account is gone).
    #[schema(required = true)]
    pub username: Option<String>,
    #[schema(required = true)]
    pub display_name: Option<String>,
    /// Only with the `user_email` scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Only with the `user_employee_number` scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub employee_number: Option<String>,
}

/// The actor of an event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedActorBody {
    /// `user`, `service_account`, `user_via_service_account` or `system`.
    pub kind: String,
    /// The user or service account (`null` for `system`).
    #[schema(required = true)]
    pub user: Option<FeedUserBody>,
    /// The service account that acted for `user` (act-as), else `null`.
    #[schema(required = true)]
    pub via: Option<FeedUserBody>,
}

/// One work item an event concerns.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedWorkItemBody {
    /// `kanflow`.
    pub provider: String,
    /// The kanflow project id of the binding.
    pub project_id: String,
    /// `PBI-<n>`.
    #[serde(rename = "ref")]
    pub reference: String,
}

/// One event.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedEventBody {
    /// `evt_<32 hex>`: deduplicate by it (delivery is at least once).
    pub id: String,
    /// The position right after this event (resume here once it is done).
    pub cursor: String,
    /// `<aggregate>.<verb>` (unknown types are to be skipped).
    #[serde(rename = "type")]
    pub event_type: String,
    /// The version of `data`.
    pub schema_version: i16,
    /// UTC, RFC 3339 with milliseconds.
    pub occurred_at: String,
    #[schema(minimum = 1)]
    pub project_id: i64,
    pub actor: FeedActorBody,
    /// Possibly empty.
    pub work_items: Vec<FeedWorkItemBody>,
    /// The event's data (its own schema_version).
    #[schema(value_type = Object)]
    pub data: Value,
}

/// One page of the feed.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedPageBody {
    /// `1`.
    pub schema_version: u32,
    pub events: Vec<FeedEventBody>,
    /// Where the next read starts (the head when nothing visible is left).
    pub next_cursor: String,
    /// Visible events remain after `next_cursor`.
    pub has_more: bool,
}

/// The head position.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedCursorBody {
    /// `1`.
    pub schema_version: u32,
    pub cursor: String,
}

fn user_body(user: ActorUser) -> FeedUserBody {
    FeedUserBody {
        id: user.id.get(),
        username: user.username,
        display_name: user.display_name,
        email: user.email,
        employee_number: user.employee_number,
    }
}

fn actor_body(actor: Actor) -> FeedActorBody {
    FeedActorBody {
        kind: actor.kind,
        user: actor.user.map(user_body),
        via: actor.via.map(user_body),
    }
}

fn event_body(event: EventOut) -> FeedEventBody {
    FeedEventBody {
        id: event.id,
        cursor: event.cursor,
        event_type: event.event_type,
        schema_version: event.schema_version,
        occurred_at: event.occurred_at,
        project_id: event.project_id,
        actor: actor_body(event.actor),
        work_items: event
            .work_items
            .into_iter()
            .map(|w| FeedWorkItemBody {
                provider: w.provider,
                project_id: w.project_id,
                reference: w.reference,
            })
            .collect(),
        data: event.data,
    }
}

fn page_body(page: EventsPage) -> FeedPageBody {
    FeedPageBody {
        schema_version: SCHEMA_VERSION,
        events: page.events.into_iter().map(event_body).collect(),
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    }
}

fn feed(state: &AppState) -> Result<&Feed, ForgeError> {
    state.feed.as_deref().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "feed_unconfigured")
    })
}

/// The `{id}` of `…/integrations/{id}/<tail>` (`events` or
/// `events/cursor`).
fn feed_id(path: &str, tail: &str) -> Result<IntegrationId, ForgeError> {
    let (raw, rest) = path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    if rest != tail {
        return Err(param_invalid("id", "not_positive_decimal"));
    }
    let id = parse_positive_id(raw).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    Ok(IntegrationId::new(id))
}

/// The feed routes' loader: the integration of the path (its service
/// account, whether it is enabled), the caller, and whether the request
/// asks to act for a person.
pub struct FeedOf {
    tail: &'static str,
}

/// The loader of `GET …/events`.
pub const FEED_EVENTS: FeedOf = FeedOf { tail: "events" };
/// The loader of `GET …/events/cursor`.
pub const FEED_CURSOR: FeedOf = FeedOf {
    tail: "events/cursor",
};

impl Loader<AppState> for FeedOf {
    type Ctx = IntegrationFeedCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, IntegrationFeedCtx> {
        Box::pin(async move {
            let id = feed_id(parts.uri.path(), self.tail)?;
            let act_as = parts.headers.contains_key(ACT_AS_HEADER);
            let loaded = bounded(state, "integration_feed_load", feed(state)?.target(id)).await?;
            Ok(match loaded {
                Some(loaded) => found(IntegrationFeedCtx::new(loaded, viewer, act_as)),
                None => absent(),
            })
        })
    }
}

/// `limit` and `cursor`, strictly (the value is never logged).
fn parse_query(query: Option<&str>) -> Result<FeedQuery, ForgeError> {
    let mut limit: Option<&str> = None;
    let mut cursor: Option<&str> = None;
    for pair in query.unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (slot, param) = match key {
            "limit" => (&mut limit, "limit"),
            "cursor" => (&mut cursor, "cursor"),
            _ => return Err(param_invalid("query", "unknown_parameter")),
        };
        if slot.replace(value).is_some() {
            return Err(param_invalid(param, "repeated"));
        }
    }
    let limit = match limit {
        None => forge_integ::DEFAULT_LIMIT,
        Some(raw) => parse_positive_id(raw)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= forge_integ::MAX_LIMIT)
            .ok_or_else(|| param_invalid("limit", "out_of_range"))?,
    };
    let cursor = match cursor {
        None => None,
        Some("") => return Err(param_invalid("cursor", "empty")),
        Some(raw) => Some(raw.to_owned()),
    };
    Ok(FeedQuery { cursor, limit })
}

async fn timed<T>(
    read: impl std::future::Future<Output = Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    match tokio::time::timeout(FEED_TIMEOUT, read).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "integration_feed")
            .with_context("timeout_ms", FEED_TIMEOUT.as_millis())),
    }
}

/// `GET /api/v1/integrations/{id}/events` → `200`.
pub async fn events(state: AppState, request: Dispatched<IntegrationFeedCtx>) -> Response {
    let query = parse_query(request.raw_query());
    let w = request.into_write();
    let built = async {
        let query = query?;
        let page = timed(feed(&state)?.read_events(&w.proof, &w.ctx.request_id, &query)).await?;
        json(page_body(page))
    }
    .await;
    match built {
        Ok(body) => no_store(StatusCode::OK, body),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `GET /api/v1/integrations/{id}/events/cursor` → `200`.
pub async fn cursor(state: AppState, request: Dispatched<IntegrationFeedCtx>) -> Response {
    let unexpected = match request.raw_query() {
        Some(q) if !q.is_empty() => Some(param_invalid("query", "unknown_parameter")),
        _ => None,
    };
    let w = request.into_write();
    let built = async {
        if let Some(e) = unexpected {
            return Err(e);
        }
        let cursor = timed(feed(&state)?.head_cursor(&w.proof)).await?;
        json(FeedCursorBody {
            schema_version: SCHEMA_VERSION,
            cursor,
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

    fn rule(err: &ForgeError) -> String {
        err.context()
            .iter()
            .find(|(k, _)| *k == "rule")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    #[test]
    fn the_query_is_parsed_strictly() {
        assert_eq!(
            parse_query(None).expect("none"),
            FeedQuery {
                cursor: None,
                limit: 100
            }
        );
        let q = parse_query(Some("limit=500&cursor=abc")).expect("both");
        assert_eq!((q.limit, q.cursor.as_deref()), (500, Some("abc")));
        for (raw, wanted) in [
            ("limit=0", "out_of_range"),
            ("limit=501", "out_of_range"),
            ("limit=+1", "out_of_range"),
            ("limit=1&limit=2", "repeated"),
            ("cursor=", "empty"),
            ("after=x", "unknown_parameter"),
        ] {
            assert_eq!(
                rule(&parse_query(Some(raw)).expect_err(raw)),
                wanted,
                "{raw}"
            );
        }
    }

    #[test]
    fn the_id_and_tail_are_checked() {
        assert_eq!(
            feed_id("/api/v1/integrations/7/events", "events").expect("id"),
            IntegrationId::new(7)
        );
        assert_eq!(
            feed_id("/api/v1/integrations/7/events/cursor", "events/cursor").expect("id"),
            IntegrationId::new(7)
        );
        feed_id("/api/v1/integrations/07/events", "events").expect_err("leading zero");
        feed_id("/api/v1/integrations/7/events", "events/cursor").expect_err("tail");
    }
}
