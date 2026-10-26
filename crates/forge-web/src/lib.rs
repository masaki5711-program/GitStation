//! forge-web: the HTTP layer.
//!
//! - [`registry`] — the route registry (ADR-0010): router, OpenAPI and the
//!   authz route manifest come from one list
//! - [`context`] — request id + language per request, access log
//! - [`error`] — the single error renderer (`error_code`, `user_message`,
//!   `request_id` only; logged once)
//! - [`app`] — wiring and middleware; [`server`] — bind and serve

pub mod app;
pub mod context;
pub mod error;
mod guard;
pub mod registry;
pub mod routes;
pub mod server;

pub use app::{app, registry as route_registry, AppConfig, AppState};
pub use context::Ctx;
pub use error::ApiError;
pub use server::ServerLimits;

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
#[path = "tests.rs"]
mod tests;
