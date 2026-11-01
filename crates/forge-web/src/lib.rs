//! forge-web: the HTTP layer.
//!
//! - [`registry`] — the route registry (ADR-0010): router, OpenAPI and the
//!   authz route manifest come from one list
//! - [`context`] — request id + language per request, access log
//! - [`error`] — the single error renderer (`error_code`, `user_message`,
//!   `request_id` only; logged once)
//! - [`body`] — strict JSON request bodies of the write routes (phase 8b)
//! - `auth` — token authentication (Bearer / Basic with the token as the
//!   password) on every non-public route
//! - `dispatch` — the one dispatcher for namespace URLs (ADR-0015's seven
//!   steps on the raw path)
//! - [`routes::git`] / `git_stream` — Smart HTTP fetch (M1 phase 9): git
//!   only through forge-git's Smart HTTP API, streamed responses
//! - [`security_headers`] — the CSP and security headers of every answer
//!   (ADR-0022 §3); [`ui`] — the embedded web UI (feature `embed-ui`)
//! - [`app`] — wiring and middleware; [`server`] — bind and serve

pub mod app;
mod auth;
pub mod body;
pub mod context;
mod dispatch;
pub mod error;
mod git_stream;
mod guard;
pub mod registry;
pub mod routes;
pub mod security_headers;
pub mod server;
pub mod ui;

pub use app::{app, registry as route_registry, AppConfig, AppState, Lookups};
pub use auth::{BoundedAuthenticator, CallerProfile};
pub use context::Ctx;
pub use error::ApiError;
pub use routes::git::{GitHttpLimits, PushTasks, SmartHttp};
pub use routes::lists::Lists;
pub use security_headers::SecurityHeaders;
pub use server::ServerLimits;
pub use ui::UiStatus;

/// Where the generated documents live, relative to the product root (`Git/`).
pub const ROUTES_MANIFEST: &str = "docs/authz/routes.generated.json";
pub const MATRIX_MANIFEST: &str = "docs/authz/matrix.generated.json";
pub const OPENAPI_DOC: &str = "docs/api/openapi.json";

/// The generated documents, as `(path relative to Git/, contents)`.
pub fn generated_documents(version: &str) -> Vec<(&'static str, String)> {
    let registry = app::registry();
    vec![
        (
            ROUTES_MANIFEST,
            registry::routes_json(registry.defs(), app::PLANNED),
        ),
        (MATRIX_MANIFEST, forge_auth::matrix::summary_json()),
        (
            OPENAPI_DOC,
            registry::openapi_json(registry.defs(), version),
        ),
    ]
}

#[cfg(test)]
mod readyz_tests;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
#[cfg(test)]
mod web_ui_tests;
