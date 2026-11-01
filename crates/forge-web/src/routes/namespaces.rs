//! `GET /api/v1/namespaces/resolve?path=` (`PathResolve`, ADR-0015 "パスの
//! 解決"): which namespace a path names, for a caller who can see it.
//!
//! The registry authenticates, then [`PathParam`] resolves the path with
//! **one** resolver call (which also loads the caller's authorization
//! inputs), then `prove(PathResolve)` decides: absent, deleted tombstone
//! and invisible all answer the same `FG-REPO-0005` (only the log `reason`
//! differs). A `moved` tombstone answers the target's current path —
//! revealed only after `authorize()` allowed — and is logged as
//! `namespace.redirected` (warn).
//!
//! A missing, repeated or malformed `path` parameter is answered like an
//! absent path (`FG-REPO-0005`, logged with `lookup=`) until request
//! validation gets its own code (FG-WEB-0006 `RequestInvalid`, phase 8b);
//! such a request issues no resolver call (its shape, not the data, is
//! what differs).

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{ResolvedCtx, ResolvedVia, UnresolvedCtx, UnresolvedReason};
use forge_core::{LookupPath, NamespaceOwner, UserId};
use serde_json::json;

use crate::app::AppState;
use crate::dispatch::{path_resolve_ctx, resolve_bounded};
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup};

/// The query parameter holding the path.
const PARAM: &str = "path";

/// Loads the `path` parameter's resolution for `PathResolve`.
pub struct PathParam;

impl Loader<AppState> for PathParam {
    type Ctx = ResolvedCtx;

    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, ResolvedCtx> {
        Box::pin(async move {
            let path = match lookup_param(parts.uri.query()) {
                Ok(path) => path,
                Err(why) => {
                    return Ok(Lookup::Unresolved {
                        ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
                        why: Some(why),
                    })
                }
            };
            let resolution = resolve_bounded(state, &path, viewer).await?;
            Ok(match path_resolve_ctx(resolution, &path) {
                Ok((ctx, canonical, tombstone)) => Lookup::Found {
                    ctx,
                    canonical: Some(canonical),
                    tombstone,
                    project: None,
                },
                Err(ctx) => Lookup::Unresolved { ctx, why: None },
            })
        })
    }
}

/// The `path` parameter, folded for lookup. `Err` names what was wrong
/// (logged as `lookup`, never the value).
pub(crate) fn lookup_param(query: Option<&str>) -> Result<LookupPath, &'static str> {
    let mut values = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('=').filter(|(k, _)| *k == PARAM))
        .map(|(_, v)| v);
    let Some(raw) = values.next() else {
        return Err("path_param_missing");
    };
    if values.next().is_some() {
        return Err("path_param_repeated");
    }
    let decoded = percent_decode(raw).ok_or("path_param_malformed")?;
    let trimmed = decoded.strip_prefix('/').unwrap_or(&decoded);
    let segments: Vec<&str> = trimmed.split('/').collect();
    LookupPath::fold(&segments).map_err(|_| "path_param_invalid")
}

/// Strict percent-decoding (a `%` must start two hex digits; `+` is not a
/// space: a namespace has neither).
fn percent_decode(raw: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut iter = raw.bytes();
    while let Some(b) = iter.next() {
        if b != b'%' {
            bytes.push(b);
            continue;
        }
        let hi = iter.next().and_then(|c| char::from(c).to_digit(16))?;
        let lo = iter.next().and_then(|c| char::from(c).to_digit(16))?;
        bytes.push(u8::try_from(hi * 16 + lo).ok()?);
    }
    String::from_utf8(bytes).ok()
}

/// The answer: the namespace id, kind and owner id from the proof, the
/// current path (moved: the target's path now) and whether a tombstone led
/// here. The owner id is the kind's own key (`project_id`, `group_id` or
/// `user_id`), revealed only now that `authorize()` allowed (MR5): a
/// client can go on to `GET /api/v1/projects/{project_id}`.
pub async fn resolve(_state: AppState, request: Dispatched<ResolvedCtx>) -> Response {
    let proof = request.proof();
    let moved = proof.ctx().via() == ResolvedVia::Moved;
    let path = request.canonical_path().map(|p| p.full_path());
    if let (true, Some(tombstone)) = (moved, request.via_tombstone()) {
        tracing::warn!(
            target: "forge_web::namespaces",
            request_id = %request.ctx().request_id,
            namespace_id = proof.id().get(),
            tombstone_id = tombstone.get(),
            route = "namespaces_resolve",
            "namespace.redirected"
        );
    }
    let (owner_key, owner_id) = match proof.ctx().owner() {
        NamespaceOwner::User(id) => ("user_id", id.get()),
        NamespaceOwner::Group(id) => ("group_id", id.get()),
        NamespaceOwner::Project(id) => ("project_id", id.get()),
    };
    let mut body = json!({
        "namespace_id": proof.id().get(),
        "kind": proof.ctx().view().kind(),
        "canonical_path": path,
        "moved": moved,
    });
    if let Some(obj) = body.as_object_mut() {
        obj.insert(owner_key.to_owned(), json!(owner_id));
    }
    let mut res = Json(body).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}
