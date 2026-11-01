//! The SPA shell (`WebShell`, ADR-0015): `index.html` for namespace homes,
//! the `/-/` page words and the permalinks. Public: the same bytes for every
//! path, without credentials, sessions or the database — so a shell answer
//! never says whether a path exists. The SPA (M2) replaces the placeholder
//! body; the data comes from the API (`PathResolve`, `ProjectRead`, …),
//! which authorizes.

use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;

use crate::app::AppState;
use crate::registry::Dispatched;

/// The shell document (placeholder until the M2 SPA build replaces it).
pub const SHELL_HTML: &str = "<!doctype html>\n<html lang=\"ja\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>GitStation</title>\n\
</head>\n<body>\n<div id=\"app\"></div>\n</body>\n</html>\n";

/// `WebShell`: the same answer for every path (nothing depends on the
/// request, the caller or the database).
pub async fn shell(_state: AppState, _request: Dispatched<InstanceCtx>) -> Response {
    let mut res = SHELL_HTML.into_response();
    let headers = res.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'self'; frame-ancestors 'none'; base-uri 'none'"),
    );
    res
}
