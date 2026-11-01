//! The SPA shell (`WebShell`, ADR-0015): `index.html` for namespace homes,
//! the `/-/` page words and the permalinks. Public: the same bytes for every
//! path, without credentials, sessions or the database — so a shell answer
//! never says whether a path exists. The bytes are the embedded
//! `web/dist/index.html` with the feature `embed-ui`, the placeholder
//! otherwise (`crate::ui`); the data comes from the API (`PathResolve`,
//! `ProjectRead`, …), which authorizes. The CSP and the other security
//! headers come from `crate::security_headers` (one table).

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use forge_auth::InstanceCtx;

use crate::app::AppState;
use crate::registry::Dispatched;
use crate::security_headers::ResponseClass;

/// `WebShell`: the same answer for every path (nothing depends on the
/// request, the caller or the database).
pub async fn shell(_state: AppState, _request: Dispatched<InstanceCtx>) -> Response {
    let mut res = crate::ui::shell_html().into_response();
    let headers = res.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // Asset names change with every build: revalidate the shell each load.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res.extensions_mut().insert(ResponseClass::Shell);
    res
}
