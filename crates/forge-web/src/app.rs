//! Wiring: the registered routes, the dispatcher (the fallback for every
//! path no fixed route matches), and the middleware stack (outermost
//! first): request id + access log → request timeout → panic catcher →
//! authenticator extension → body limit → routes (non-public fixed routes:
//! token authentication → guard → load → prove → handler; scoped routes:
//! the dispatcher's seven steps, `crate::dispatch`).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::ACCEPT_LANGUAGE;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use forge_auth::Action;
use forge_core::readiness::ReadinessProbe;
use forge_core::web_path::{InstanceRoute, RouteWord};
use forge_core::{log_safe_path, Authenticator, NamespaceResolver, ProjectAccessLoader, RequestId};
use forge_errors::{ErrorCode, ForgeError};
use futures_util::FutureExt;

use crate::auth::AuthenticatorExt;
use crate::context::{self, Ctx};
use crate::error::ApiError;
use crate::registry::{DispatchState, Registry, RouteScope, ScopedRoute, Verb};
use crate::routes;

/// Shared state of every handler.
#[derive(Clone)]
pub struct AppState {
    pub version: &'static str,
    /// Dependencies `GET /readyz` checks, in order.
    pub probes: Arc<[Arc<dyn ReadinessProbe>]>,
    /// Client-side bound for each probe (FG-WEB-0005 when exceeded). Must
    /// exceed the probe's own bounds, so the probe reports its real cause
    /// (`forge serve` derives it from the database timeouts).
    pub probe_timeout: Duration,
    /// Single-flight + cache for `/readyz` (it is public).
    pub readiness: Arc<routes::health::ReadinessCache>,
    /// Resolves personal access tokens (ADR-0019 §7). `forge serve` passes
    /// the database (`forge_db::tokens::authenticate`); forge-web does not
    /// depend on forge-db. Required, so a server cannot start without it.
    pub authenticator: Arc<dyn Authenticator>,
    /// The data the dispatcher and the path / project routes read
    /// (ADR-0015 steps 5–6). Required, like the authenticator.
    pub lookups: Lookups,
    /// Client-side bound of one lookup (`FG-WEB-0002`
    /// `operation=namespace_resolve` / `project_load` when exceeded); `forge serve` derives it
    /// from the database timeouts.
    pub lookup_timeout: Duration,
}

/// Path → namespace and project-by-id lookups (`forge serve` passes the
/// database: `forge_db::resolve`); forge-web reads them through forge-core
/// traits and never depends on forge-db.
#[derive(Clone)]
pub struct Lookups {
    pub namespaces: Arc<dyn NamespaceResolver>,
    /// For `GET|HEAD /api/v1/projects/{id}` (MR5): one `load_project` call
    /// per request, bounded by `lookup_timeout`.
    pub projects: Arc<dyn ProjectAccessLoader>,
}

impl AppState {
    pub fn new(
        version: &'static str,
        probes: Vec<Arc<dyn ReadinessProbe>>,
        authenticator: Arc<dyn Authenticator>,
        lookups: Lookups,
    ) -> Self {
        Self {
            version,
            probes: probes.into(),
            probe_timeout: Duration::from_secs(3),
            readiness: routes::health::ReadinessCache::new(),
            authenticator,
            lookups,
            lookup_timeout: Duration::from_secs(5),
        }
    }

    #[must_use]
    pub fn with_lookup_timeout(mut self, timeout: Duration) -> Self {
        self.lookup_timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.probes.iter().map(|p| p.name()).collect();
        f.debug_struct("AppState")
            .field("version", &self.version)
            .field("probes", &names)
            .field("probe_timeout", &self.probe_timeout)
            .field("lookup_timeout", &self.lookup_timeout)
            .finish_non_exhaustive()
    }
}

impl DispatchState for AppState {
    fn resolver(&self) -> &dyn NamespaceResolver {
        &*self.lookups.namespaces
    }

    fn lookup_timeout(&self) -> Duration {
        self.lookup_timeout
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AppConfig {
    /// Time until the response headers are produced (not body streaming;
    /// per-route timeouts for git streams arrive with Smart HTTP, phase 9).
    pub request_timeout: Duration,
    /// Request body limit for extractors (raw streams set their own).
    pub body_limit_bytes: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            body_limit_bytes: 1024 * 1024,
        }
    }
}

/// Actions that have permission-matrix rows but no route yet (git operations:
/// M1 phase 9-10; API resources: phase 8).
/// Explicit, so the `authz-coverage` gate can tell "not built yet" from a
/// stale row; an action leaves this list in the commit that routes it.
pub const PLANNED: &[Action] = &[
    Action::RepoRead,
    Action::RepoPush,
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

/// The words whose page is the SPA shell on a project (ADR-0015 "ルートの
/// 系統"). `raw` and `archive` are `RepoRead` content routes (not routed
/// yet: the dispatcher answers `FG-WEB-0001` for them).
pub const PROJECT_SHELL_WORDS: &[RouteWord] = &[
    RouteWord::Tree,
    RouteWord::Blob,
    RouteWord::Blame,
    RouteWord::Commits,
    RouteWord::Commit,
    RouteWord::Compare,
    RouteWord::Branches,
    RouteWord::Tags,
    RouteWord::Settings,
    RouteWord::Members,
];

/// The words whose page is the SPA shell on a group.
pub const GROUP_SHELL_WORDS: &[RouteWord] = &[RouteWord::Settings, RouteWord::Members];

/// Every route of the product. The `gen-authz` binary and the tests read the
/// same list.
pub fn registry() -> Registry<AppState> {
    let fixed = Registry::new()
        .route(
            Verb::Get,
            "/healthz",
            Action::Health,
            "Liveness probe",
            routes::health::healthz,
        )
        .route(
            Verb::Get,
            "/readyz",
            Action::Health,
            "Readiness probe (database and other dependencies)",
            routes::health::readyz,
        )
        .route_with(
            Verb::Get,
            "/api/v1/namespaces/resolve",
            Action::PathResolve,
            "Resolve a namespace path for the caller (ADR-0015)",
            routes::namespaces::PathParam,
            routes::namespaces::resolve,
        )
        // HEAD is this route too (axum answers it without the body).
        .route_with(
            Verb::Get,
            routes::projects::PATH,
            Action::ProjectRead,
            "Read one project by id (ADR-0015 MR5)",
            routes::projects::ProjectById,
            routes::projects::read,
        );
    shell_routes(fixed)
}

/// The SPA shell (`WebShell`) for every page URL, `GET` and `HEAD`.
fn shell_routes(mut registry: Registry<AppState>) -> Registry<AppState> {
    let shell = routes::shell::shell;
    for verb in [Verb::Get, Verb::Head] {
        registry = registry
            .scoped(
                RouteScope::Namespace,
                verb,
                ScopedRoute::Home,
                None,
                Action::WebShell,
                "SPA shell: namespace home",
                shell,
            )
            .scoped(
                RouteScope::Instance,
                verb,
                ScopedRoute::Instance(InstanceRoute::ProjectPermalink),
                None,
                Action::WebShell,
                "SPA shell: project permalink",
                shell,
            );
        for &word in PROJECT_SHELL_WORDS {
            registry = registry.scoped(
                RouteScope::Project,
                verb,
                ScopedRoute::Word(word),
                None,
                Action::WebShell,
                "SPA shell: project page",
                shell,
            );
        }
        for &word in GROUP_SHELL_WORDS {
            registry = registry.scoped(
                RouteScope::Group,
                verb,
                ScopedRoute::Word(word),
                None,
                Action::WebShell,
                "SPA shell: group page",
                shell,
            );
        }
    }
    registry
}

/// The full application router.
pub fn app(state: AppState, config: AppConfig) -> Result<Router, ForgeError> {
    build(registry(), state, config)
}

/// The product's middleware around a test's own registry (integration
/// tests need guarded routes; the product registers none yet). Not compiled
/// into the server.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn with_registry(
    registry: Registry<AppState>,
    state: AppState,
    config: AppConfig,
) -> Result<Router, ForgeError> {
    build(registry, state, config)
}

/// Assemble a router from a registry (tests use their own registries).
pub(crate) fn build(
    registry: Registry<AppState>,
    state: AppState,
    config: AppConfig,
) -> Result<Router, ForgeError> {
    let authenticator = AuthenticatorExt(Arc::clone(&state.authenticator));
    Ok(registry
        .into_router()?
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .layer(axum::Extension(authenticator))
        .layer(middleware::from_fn(catch_panic))
        .layer(middleware::from_fn_with_state(
            config.request_timeout,
            request_timeout,
        ))
        .layer(middleware::from_fn(context::request_id))
        .with_state(state))
}

async fn method_not_allowed(ctx: Ctx, req: Request) -> ApiError {
    ctx.fail(
        ForgeError::new(ErrorCode::MethodNotAllowed)
            .with_context("method", req.method())
            .with_context("path", log_safe_path(req.uri().path())),
    )
}

/// A panicking handler becomes a logged `FG-SYS-0001` with the request id,
/// not a dropped connection.
async fn catch_panic(req: Request, next: Next) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    // clone: the request moves into the handler; the log needs where it panicked.
    let method = req.method().clone();
    let path = log_safe_path(req.uri().path());
    match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(res) => res,
        Err(payload) => ApiError::new(
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "handler_panicked")
                .with_context("method", method)
                .with_context("path", path)
                .with_context("panic_message", panic_message(payload.as_ref())),
            request_id,
            lang,
        )
        .into_response(),
    }
}

/// The text of a panic payload (`panic!("..")` carries `&str` or `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

async fn request_timeout(State(limit): State<Duration>, req: Request, next: Next) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .cloned()
        .unwrap_or_else(RequestId::generate);
    let lang = context::language(req.headers().get(ACCEPT_LANGUAGE));
    let method = req.method().clone(); // clone: needed after the request moves
    let path = log_safe_path(req.uri().path());
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
