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
//! A missing, repeated or malformed `path` parameter is `FG-WEB-0006
//! reason=param_invalid param=path rule=missing|repeated|malformed|invalid`
//! (phase 8b MR6), decided from the string alone after authentication;
//! such a request issues no resolver call (its shape, not the data, is
//! what differs).
//!
//! **`prefix=true`** (M2 MR6, ADR-0015 "GitHub の形の URL の救済"): the SPA
//! rescues a URL like `/a/b/pull/3`. Every leading prefix of the path that
//! is a valid namespace path (at most `MAX_DEPTH` segments, up to the first
//! segment that is not one) is resolved — **all of them, whatever exists**,
//! so the statements do not depend on the data — and only a **live**
//! match on a **project** counts (never a tombstone, never a group or a
//! personal namespace on its own). The longest prefix `authorize(PathResolve)`
//! allows for the caller is the answer (`registry` proves it again), with
//! the segments after it as `remainder`; without one the answer is the
//! same `FG-REPO-0005` as an absent path, whatever groups or invisible
//! projects lie along the path. Each caller may ask [`PREFIX_LIMIT`] times
//! per [`PREFIX_WINDOW`] (`FG-WEB-0010 route=resolve_prefix`, with
//! `Retry-After`), checked before any resolver call. `prefix` other than
//! `true` / `false`, or repeated, is `FG-WEB-0006 param=prefix`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::header::{CACHE_CONTROL, VARY};
use axum::http::request::Parts;
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::Json;
use forge_auth::{
    authorize, Action, Principal, ResolvedCtx, ResolvedVia, ResourceCtx, UnresolvedCtx,
    UnresolvedReason,
};
use forge_core::{
    telemetry, LookupPath, NamespaceOwner, RequestId, Resolution as CoreResolution, ResolvedTarget,
    UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use serde::Serialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::dispatch::{loaded_namespace, path_resolve_ctx, resolve_bounded};
use crate::registry::{Dispatched, LoadFuture, Loader, Lookup, QueryParam};

/// The route's path.
pub const PATH: &str = "/api/v1/namespaces/resolve";

/// Which namespace a path names. Exactly one of `project_id`, `group_id`,
/// `user_id` is present, by `kind`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    #[schema(minimum = 1)]
    pub namespace_id: i64,
    /// `project`, `group` or `user`.
    pub kind: String,
    /// The namespace's current path (a moved path: the target's).
    pub canonical_path: String,
    /// A `moved` tombstone led here.
    pub moved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub project_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub group_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub user_id: Option<i64>,
    /// `prefix=true` only: the requested path's segments after the
    /// project's (`pull/3`); absent when nothing follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remainder: Option<String>,
}

/// The query parameter holding the path.
const PARAM: &str = "path";
/// The query parameter that asks for the longest visible project prefix.
const PREFIX_PARAM: &str = "prefix";

/// The query parameters (`openapi.json`).
pub const QUERY: &[QueryParam] = &[
    QueryParam {
        name: "path",
        description: "The namespace path (percent-encoded)",
        integer_range: None,
    },
    QueryParam {
        name: "prefix",
        description: "true: the longest prefix of path that is a project the caller can see, with the rest as remainder (M2 rescue of GitHub-shaped URLs)",
        integer_range: None,
    },
];

/// `prefix=true` requests per caller per [`PREFIX_WINDOW`].
pub const PREFIX_LIMIT: u32 = 60;
pub const PREFIX_WINDOW: Duration = Duration::from_secs(60);
/// Callers remembered at once (beyond it expired windows go first, then
/// the caller whose window started first: a full table never refuses all).
const MAX_CALLERS: usize = 10_000;

/// The fixed-window counter of `prefix=true` requests per caller.
#[derive(Debug, Default)]
pub struct PrefixLimiter {
    callers: Mutex<HashMap<i64, (Instant, u32)>>,
    /// The poisoned table was logged (once per process).
    poison_logged: AtomicBool,
}

impl PrefixLimiter {
    /// Count one request of `user`; `Err(retry_after_secs)` over the limit.
    pub fn check(&self, user: UserId, now: Instant) -> Result<(), u64> {
        let Ok(mut callers) = self.callers.lock() else {
            if !self.poison_logged.swap(true, Ordering::Relaxed) {
                telemetry::log_failure(
                    &ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "resolve_limiter_poisoned"),
                    &RequestId::generate(),
                );
            }
            return Err(PREFIX_WINDOW.as_secs());
        };
        let key = user.get();
        if callers.len() >= MAX_CALLERS && !callers.contains_key(&key) {
            callers.retain(|_, (start, _)| now.duration_since(*start) < PREFIX_WINDOW);
            if callers.len() >= MAX_CALLERS {
                let oldest = callers
                    .iter()
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(id, _)| *id);
                if let Some(oldest) = oldest {
                    callers.remove(&oldest);
                }
            }
        }
        let entry = callers.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= PREFIX_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= PREFIX_LIMIT {
            let left = PREFIX_WINDOW.saturating_sub(now.duration_since(entry.0));
            return Err(left.as_secs().max(1));
        }
        entry.1 += 1;
        Ok(())
    }
}

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
            let prefix =
                prefix_mode(parts.uri.query()).map_err(|rule| param_invalid(PREFIX_PARAM, rule))?;
            if prefix {
                return load_prefix(state, parts, viewer).await;
            }
            let path =
                lookup_param(parts.uri.query()).map_err(|rule| param_invalid(PARAM, rule))?;
            let resolution = resolve_bounded(state, &path, viewer).await?;
            Ok(match path_resolve_ctx(resolution, &path) {
                Ok((ctx, canonical, tombstone)) => Lookup::Found {
                    ctx,
                    canonical: Some(canonical),
                    tombstone,
                    project: None,
                    group: None,
                },
                Err(ctx) => Lookup::Unresolved { ctx, why: None },
            })
        })
    }
}

/// The `path` parameter, folded for lookup. `Err` names the rule it broke
/// (logged as `rule`, never the value).
pub(crate) fn lookup_param(query: Option<&str>) -> Result<LookupPath, &'static str> {
    let decoded = path_value(query)?;
    let segments = segments_of(&decoded);
    LookupPath::fold(&segments).map_err(|_| "invalid")
}

/// The one `path` value, percent-decoded.
fn path_value(query: Option<&str>) -> Result<String, &'static str> {
    let mut values = query
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('=').filter(|(k, _)| *k == PARAM))
        .map(|(_, v)| v);
    let Some(raw) = values.next() else {
        return Err("missing");
    };
    if values.next().is_some() {
        return Err("repeated");
    }
    percent_decode(raw).ok_or("malformed")
}

/// The segments of a decoded path (one leading `/` ignored).
fn segments_of(decoded: &str) -> Vec<&str> {
    decoded
        .strip_prefix('/')
        .unwrap_or(decoded)
        .split('/')
        .collect()
}

/// The `prefix` parameter: absent or `false` → `false`, `true` → `true`;
/// anything else, or repeated, names the rule it broke.
pub(crate) fn prefix_mode(query: Option<&str>) -> Result<bool, &'static str> {
    let mut values = query.unwrap_or_default().split('&').filter_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == PREFIX_PARAM).then_some(v)
    });
    let first = values.next();
    if values.next().is_some() {
        return Err("repeated");
    }
    match first {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => Err("malformed"),
    }
}

/// The candidate prefixes (longest first, each with how many segments it
/// takes) and the decoded segments of the whole path.
pub(crate) type Candidates = (Vec<(LookupPath, usize)>, Vec<String>);

/// The candidate prefixes of the `path` parameter for `prefix=true`,
/// longest first: every leading run of segments that folds to a namespace
/// path (at most `MAX_DEPTH`, up to the first segment that is not one),
/// with how many segments it takes and the decoded segments. `Err` names
/// the rule: `missing`, `repeated`, `malformed`, `invalid` (not even the
/// first segment is a namespace path).
pub(crate) fn prefix_candidates(query: Option<&str>) -> Result<Candidates, &'static str> {
    let decoded = path_value(query)?;
    let segments: Vec<String> = segments_of(&decoded)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut candidates = Vec::new();
    for len in 1..=segments.len().min(forge_core::namespace_path::MAX_DEPTH) {
        let head: Vec<&str> = segments.iter().take(len).map(String::as_str).collect();
        match LookupPath::fold(&head) {
            Ok(path) => candidates.push((path, len)),
            Err(_) => break,
        }
    }
    if candidates.is_empty() {
        return Err("invalid");
    }
    candidates.reverse();
    Ok((candidates, segments))
}

/// `prefix=true`: the rate limit, then every candidate resolved (see the
/// module docs), then the longest live project `authorize(PathResolve)`
/// allows for the caller. A denial is "not this prefix"; anything that is
/// not a denial (`FG-SYS-0001`) fails the request.
async fn load_prefix(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<Lookup<ResolvedCtx>, ForgeError> {
    // The registry's guard admitted an active, signed-in caller.
    let user = viewer.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "resolve_viewer_missing")
    })?;
    let principal = parts
        .extensions
        .get::<Principal>()
        .cloned()
        .unwrap_or_else(Principal::anonymous);
    if let Err(retry) = state.resolve_limiter.check(user, Instant::now()) {
        return Err(ForgeError::new(ErrorCode::RateLimited)
            .with_context("route", "resolve_prefix")
            .with_context("limit", PREFIX_LIMIT)
            .with_context("window_s", PREFIX_WINDOW.as_secs())
            .with_context(crate::error::RETRY_AFTER_CONTEXT, retry));
    }
    let (candidates, _) =
        prefix_candidates(parts.uri.query()).map_err(|rule| param_invalid(PARAM, rule))?;
    let mut best: Option<(ResolvedCtx, LookupPath)> = None;
    // Every candidate is resolved (no early exit): the statements are the
    // same whatever lies along the path.
    for (path, _) in candidates {
        let resolution = resolve_bounded(state, &path, viewer).await?;
        if best.is_some() {
            continue;
        }
        let CoreResolution::Live(resolved) = resolution else {
            continue;
        };
        if !matches!(resolved.target, ResolvedTarget::Project { .. }) {
            continue;
        }
        let ctx = ResolvedCtx::new(loaded_namespace(&resolved, ResolvedVia::Live));
        match authorize(&principal, Action::PathResolve, &ctx.resource()) {
            Ok(()) => best = Some((ctx, path)),
            Err(e) if e.code() == ErrorCode::Internal => return Err(e),
            Err(_) => {}
        }
    }
    Ok(match best {
        Some((ctx, path)) => Lookup::Found {
            ctx,
            canonical: Some(path.as_namespace_path().clone()), // clone: the lookup owns its path
            tombstone: None,
            project: None,
            group: None,
        },
        None => Lookup::Unresolved {
            ctx: UnresolvedCtx::new(UnresolvedReason::Absent),
            why: Some("no_visible_project_prefix"),
        },
    })
}

/// Strict percent-decoding (a `%` must start two hex digits; `+` is not a
/// space: a namespace has neither).
pub(crate) fn percent_decode(raw: &str) -> Option<String> {
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
    let Some(path) = request.canonical_path().map(|p| p.full_path()) else {
        // The loader always hands the path with a found namespace.
        let err = ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "canonical_path_missing")
            .with_context("namespace_id", proof.id().get());
        return request.ctx().fail(err).into_response();
    };
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
    let mut body = Resolution {
        namespace_id: proof.id().get(),
        kind: proof.ctx().view().kind().to_owned(),
        canonical_path: path,
        moved,
        project_id: None,
        group_id: None,
        user_id: None,
        remainder: None,
    };
    // `prefix=true`: what follows the project's segments (the loader
    // matched `depth` of them; the request parsed already).
    if let (Ok(true), Some(depth)) = (
        prefix_mode(request.raw_query()),
        request.canonical_path().map(|p| p.depth()),
    ) {
        if let Ok((_, segments)) = prefix_candidates(request.raw_query()) {
            let rest = segments.get(depth..).unwrap_or_default();
            if !rest.is_empty() {
                body.remainder = Some(rest.join("/"));
            }
        }
    }
    match proof.ctx().owner() {
        NamespaceOwner::User(id) => body.user_id = Some(id.get()),
        NamespaceOwner::Group(id) => body.group_id = Some(id.get()),
        NamespaceOwner::Project(id) => body.project_id = Some(id.get()),
    }
    let mut res = Json(body).into_response();
    let headers = res.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    headers.insert(VARY, HeaderValue::from_static("Authorization, Cookie"));
    res
}
