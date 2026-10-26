//! Wiring: the registered routes, the fallback, and the middleware stack
//! (outermost first): request id + access log → request timeout → routes.

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::header::ACCEPT_LANGUAGE;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use forge_auth::Action;
use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};

use crate::context::{self, Ctx};
use crate::error::ApiError;
use crate::registry::{Registry, Verb};
use crate::routes;

/// Shared state of every handler.
#[derive(Debug, Clone)]
pub struct AppState {
    pub version: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct AppConfig {
    pub request_timeout: Duration,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// Actions that have permission-matrix rows but no route yet (M1 phase 8).
/// Explicit, so the `authz-coverage` gate can tell "not built yet" from a
/// stale row; an action leaves this list in the commit that routes it.
pub const PLANNED: &[Action] = &[
    Action::ProjectRead,
    Action::ProjectCreate,
    Action::ProjectUpdate,
    Action::ProjectVisibilityChange,
    Action::ProjectTransfer,
    Action::ProjectDelete,
    Action::ProjectRestore,
    Action::ProtectedRefManage,
    Action::MemberManage,
    Action::GroupRead,
    Action::GroupCreateTopLevel,
    Action::SubgroupCreate,
    Action::GroupUpdate,
    Action::TokenCreate,
    Action::TokenList,
    Action::TokenRevoke,
    Action::UserCreate,
    Action::UserDisable,
    Action::AuditRead,
    Action::Metrics,
];

/// Every route of the product. The `gen-authz` binary and the tests read the
/// same list.
pub fn registry() -> Registry<AppState> {
    Registry::new().route(
        Verb::Get,
        "/healthz",
        Action::Health,
        "Liveness probe",
        routes::health::healthz,
    )
}

/// The full application router.
pub fn app(state: AppState, config: AppConfig) -> Result<Router, ForgeError> {
    build(registry(), state, config)
}

/// Assemble a router from a registry (tests use their own registries).
pub fn build(
    registry: Registry<AppState>,
    state: AppState,
    config: AppConfig,
) -> Result<Router, ForgeError> {
    Ok(registry
        .into_router()?
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            config.request_timeout,
            request_timeout,
        ))
        .layer(middleware::from_fn(context::request_id))
        .with_state(state))
}

async fn not_found(ctx: Ctx, req: Request) -> ApiError {
    ctx.fail(
        ForgeError::new(ErrorCode::RouteNotFound)
            .with_context("method", req.method())
            .with_context("path", req.uri().path()),
    )
}

async fn request_timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    let method = req.method().clone(); // clone: needed after the request moves
    let path = req.uri().path().to_string();
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(res) => res,
        Err(_) => ApiError::new(
            ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("method", method)
                .with_context("path", path)
                .with_context("timeout_ms", limit.as_millis()),
            request_id,
            lang,
        )
        .into_response(),
    }
}
