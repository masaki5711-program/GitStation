//! The one dispatcher for namespace URLs (ADR-0015 "判定の順序", M1 phase 8
//! MR4). It is the router's fallback: every path no fixed route matches
//! comes here, and a path in no table is `FG-WEB-0001` (fail closed). It
//! reads the **raw** path (`req.uri().path()`), never axum's decoded `Path`.
//!
//! 1. `parse_web_path(raw path, raw query)` — string only (`FG-WEB-0001`).
//! 2. The vocabulary: some `scoped()` route has `(verb, route)`; otherwise
//!    `FG-WEB-0001` (`/api/…`, dumb HTTP, unrouted words — never the shell).
//! 3. `service` (git): checked by the parser, part of the route; the git
//!    subcommand comes from the matched route (phase 9).
//! 4. Authentication by word: a public word (every route with it is
//!    `WebShell`) answers the shell here without credentials or the
//!    database; any other word needs an authenticated, active principal
//!    (`FG-AUTH-0001` / `FG-AUTH-0004`, the same for every path).
//! 5. **Exactly one** resolver call (`NamespaceResolver::resolve`, which
//!    also loads the viewer's authorization inputs); the route is chosen by
//!    the resolved kind + the word. Nothing else is looked up before
//!    `authorize()` (MR4 must d).
//! 6. `prove(principal, def.action, ctx)`: absent, deleted tombstone, kind
//!    mismatch and invisible all answer the same `FG-REPO-0005`; only the
//!    log `reason` differs (MR4 must f).
//! 7. A `moved` tombstone redirects (`302`, `Cache-Control: no-store`) only
//!    after `authorize()` allowed and only for a safe verb that is not a
//!    push, logged as `namespace.redirected` at warn; otherwise the proof is
//!    admitted (`proof.action() == def.action`) and the handler runs.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, LOCATION};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use forge_auth::{
    prove, InstanceCtx, LoadedNamespace, LoadedProject, NamespaceView, Principal, ProjectCtx,
    ProjectView, ResolvedCtx, ResolvedVia, UnresolvedCtx, UnresolvedReason,
};
use forge_core::web_path::{parse_web_path, LogSafePath, ParsedPath};
use forge_core::{
    LookupPath, NamespacePath, ProjectAccess, Resolution, Resolved, ResolvedTarget, TailSegments,
    TombstoneId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};

use crate::context::Ctx;
use crate::registry::{
    admit_scoped, deny_unresolved, DispatchState, RouteScope, ScopedEntry, ScopedHandler,
    ScopedRoute, ScopedTable, Verb,
};

/// Serve one request that no fixed route matched.
pub(crate) async fn dispatch<S: DispatchState>(
    table: &ScopedTable<S>,
    state: S,
    mut req: Request,
) -> Response {
    let ctx = Ctx::of_request(&req);
    // The raw path and query, made safe for the log once (never the input
    // itself in an error context: ADR-0015, MR1 amendment).
    let safe_path = LogSafePath(req.uri().path()).to_string();
    let method = req.method().as_str().to_owned();
    let fail = |ctx: &Ctx, err: ForgeError| -> Response {
        ctx.fail(
            err.with_context("method", &method)
                .with_context("path", &safe_path),
        )
        .into_response()
    };

    // 1. The string.
    let Some(verb) = Verb::of(req.method()) else {
        return fail(&ctx, unknown_route("method"));
    };
    let parsed = match parse_web_path(req.uri().path(), req.uri().query()) {
        Ok(parsed) => parsed,
        Err(e) => return fail(&ctx, e),
    };
    let (lookup, route, tail) = split(parsed);

    // 2. The vocabulary.
    let candidates = table.candidates(verb, route);
    let Some(first) = candidates.first() else {
        return fail(
            &ctx,
            unknown_route("route").with_context("route", route.name()),
        );
    };

    // 4. Authentication by word.
    if table.is_public_word(verb, route) {
        return serve_public(first, state, ctx, req, tail, verb).await;
    }
    let guard_action = candidates
        .iter()
        .map(|e| e.def.action)
        .find(|a| !a.is_public())
        .unwrap_or(first.def.action);
    if let Err(res) = crate::auth::authenticate_request(&mut req).await {
        return *res;
    }
    let principal = match crate::guard::active_principal(&req, guard_action) {
        Ok(principal) => principal,
        Err(err) => return fail(&ctx, err.with_context("route", route.name())),
    };
    let Some(lookup) = lookup else {
        // Registration refuses a non-public instance route; reaching this
        // is a programming error, never a pass.
        return fail(
            &ctx,
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "non_public_route_without_namespace")
                .with_context("route", route.name()),
        );
    };

    // 5. Resolve — exactly once, whatever comes back.
    let resolution = match resolve_bounded(&state, &lookup, principal.user()).await {
        Ok(resolution) => resolution,
        Err(e) => return fail(&ctx, e.with_context("route", route.name())),
    };
    let found = match resolution {
        Resolution::Live(resolved) => Found {
            resolved,
            moved: None,
            canonical: lookup.as_namespace_path().clone(), // clone: the path stays in the log fields
        },
        Resolution::Moved {
            resolved,
            tombstone,
            current,
        } => Found {
            resolved,
            moved: Some(tombstone),
            canonical: current,
        },
        Resolution::Deleted { tombstone } => {
            let err = deny_unresolved(
                &principal,
                guard_action,
                UnresolvedCtx::new(UnresolvedReason::TombstoneDeleted),
            )
            .with_context("tombstone_id", tombstone.get());
            return fail(&ctx, err.with_context("route", route.name()));
        }
        Resolution::Absent => {
            let err = deny_unresolved(
                &principal,
                guard_action,
                UnresolvedCtx::new(UnresolvedReason::Absent),
            );
            return fail(&ctx, err.with_context("route", route.name()));
        }
    };
    let scope = scope_of(&found.resolved, route);
    let Some(entry) = table.find(scope, verb, route) else {
        // `raw` on a group, a word on a personal namespace: as absent.
        let err = deny_unresolved(
            &principal,
            guard_action,
            UnresolvedCtx::new(UnresolvedReason::KindMismatch),
        )
        .with_context("resolved_kind", found.resolved.kind().as_str());
        return fail(&ctx, err.with_context("route", route.name()));
    };

    // 6. authorize() (through prove), 7. redirect or handler.
    let endpoint = match &entry.handler {
        ScopedHandler::Project(endpoint) => endpoint,
        // A word that mixes a public route (a group's shell) with a
        // non-public one (a project's): authenticated above, resolved to the
        // public one. Registration admits an instance context only for a
        // public action; it never sees the resolution.
        ScopedHandler::Instance(_) => {
            return serve_public(entry, state, ctx, req, tail, verb).await;
        }
    };
    let ResolvedTarget::Project { id, access } = found.resolved.target else {
        return fail(
            &ctx,
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "project_scope_without_project")
                .with_context("route", route.name()),
        );
    };
    let project = ProjectCtx::new(loaded_project(id, access));
    let proof = match prove(&principal, entry.def.action, project) {
        Ok(proof) => proof,
        Err(e) => {
            let e = match found.moved {
                Some(tombstone) => e
                    .with_context("via", "moved")
                    .with_context("tombstone_id", tombstone.get()),
                None => e,
            };
            return fail(&ctx, e.with_context("route", route.name()));
        }
    };
    if let Some(tombstone) = found.moved {
        return redirect_or_refuse(&ctx, &fail, verb, route, &found, tombstone, &tail);
    }
    let dispatched = match admit_scoped(
        &entry.def,
        proof,
        ctx.clone(), // clone: the error path below still needs it
        principal,
        req,
        tail,
        Some(found.canonical),
    ) {
        Ok(dispatched) => dispatched,
        Err(e) => return fail(&ctx, e.with_context("route", route.name())),
    };
    head_without_body(verb, endpoint.call(state, dispatched).await)
}

/// What step 5 found.
struct Found {
    resolved: Resolved,
    moved: Option<TombstoneId>,
    canonical: NamespacePath,
}

/// The namespace, the dispatcher's route and the tail of a parsed path.
fn split(parsed: ParsedPath) -> (Option<LookupPath>, ScopedRoute, TailSegments) {
    match parsed {
        ParsedPath::NamespaceHome(path) => (Some(path), ScopedRoute::Home, TailSegments::default()),
        ParsedPath::Scoped { path, word, rest } => (Some(path), ScopedRoute::Word(word), rest),
        ParsedPath::Git { path, route } => {
            (Some(path), ScopedRoute::Git(route), TailSegments::default())
        }
        ParsedPath::Instance { route, rest } => (None, ScopedRoute::Instance(route), rest),
    }
}

/// The route scope a resolved namespace selects for `route`.
fn scope_of(resolved: &Resolved, route: ScopedRoute) -> RouteScope {
    match (resolved.target, route) {
        (_, ScopedRoute::Home) => RouteScope::Namespace,
        (ResolvedTarget::Project { .. }, ScopedRoute::Git(_)) => RouteScope::ProjectGit,
        (ResolvedTarget::Project { .. }, _) => RouteScope::Project,
        (ResolvedTarget::Group { .. }, _) => RouteScope::Group,
        // A personal namespace has no scoped words: always a kind mismatch.
        (ResolvedTarget::User(_), _) => RouteScope::Namespace,
    }
}

/// `FG-WEB-0001 reason=unknown_route`.
fn unknown_route(part: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RouteNotFound)
        .with_context("reason", "unknown_route")
        .with_context("part", part)
}

/// Step 4 for a public word: no credentials, no database. The proof is
/// for the route's own action (`WebShell`), decided for an anonymous
/// caller, admitted like every other.
async fn serve_public<S: DispatchState>(
    entry: &ScopedEntry<S>,
    state: S,
    ctx: Ctx,
    req: Request,
    tail: TailSegments,
    verb: Verb,
) -> Response {
    let ScopedHandler::Instance(endpoint) = &entry.handler else {
        return ctx
            .fail(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "public_route_without_instance_context"),
            )
            .into_response();
    };
    let principal = Principal::anonymous();
    let proof = match prove(&principal, entry.def.action, InstanceCtx) {
        Ok(proof) => proof,
        Err(e) => return ctx.fail(e).into_response(),
    };
    match admit_scoped(
        &entry.def,
        proof,
        ctx.clone(), // clone: the error path below still needs it
        principal,
        req,
        tail,
        None,
    ) {
        Ok(dispatched) => head_without_body(verb, endpoint.call(state, dispatched).await),
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// `HEAD` answers the `GET` headers without the body.
fn head_without_body(verb: Verb, res: Response) -> Response {
    if verb != Verb::Head {
        return res;
    }
    let (parts, _) = res.into_parts();
    Response::from_parts(parts, Body::empty())
}

/// A `moved` tombstone the caller may use: `302` to the current path for a
/// safe, non-push route (git clients follow the first redirect;
/// receive-pack must not be redirected, ADR-0015 "git"), `FG-REPO-0005`
/// otherwise. The `Location` is a path built from validated segments only
/// (never `Host` / `X-Forwarded-*`); the query is dropped except a git
/// advertisement's `service`, rebuilt from the matched route.
fn redirect_or_refuse(
    ctx: &Ctx,
    fail: &dyn Fn(&Ctx, ForgeError) -> Response,
    verb: Verb,
    route: ScopedRoute,
    found: &Found,
    tombstone: TombstoneId,
    tail: &TailSegments,
) -> Response {
    let push = matches!(route, ScopedRoute::Git(g) if g.is_push());
    if !verb.is_safe() || push {
        let reason = if push {
            "tombstone_push"
        } else {
            "tombstone_post"
        };
        return fail(
            ctx,
            ForgeError::new(ErrorCode::NotFound)
                .with_context("reason", reason)
                .with_context("route", route.name())
                .with_context("namespace_id", found.resolved.namespace_id.get())
                .with_context("tombstone_id", tombstone.get()),
        );
    }
    let location = location(&found.canonical, route, tail);
    let Ok(value) = HeaderValue::from_str(&location) else {
        // Built from validated, percent-encoded segments: always ASCII.
        return fail(
            ctx,
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "redirect_location_invalid")
                .with_context("route", route.name()),
        );
    };
    tracing::warn!(
        target: "forge_web::dispatch",
        request_id = %ctx.request_id,
        namespace_id = found.resolved.namespace_id.get(),
        tombstone_id = tombstone.get(),
        route = route.name(),
        "namespace.redirected"
    );
    let mut res = StatusCode::FOUND.into_response();
    res.headers_mut().insert(LOCATION, value);
    res.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// `/{current}` + the route's suffix, every tail segment percent-encoded.
pub(crate) fn location(current: &NamespacePath, route: ScopedRoute, tail: &TailSegments) -> String {
    let mut out = format!("/{}", current.full_path());
    match route {
        ScopedRoute::Home | ScopedRoute::Instance(_) => {}
        ScopedRoute::Word(word) => {
            out.push_str("/-/");
            out.push_str(word.as_str());
        }
        ScopedRoute::Git(git) => {
            out.push_str(".git");
            if git.is_advertisement() {
                out.push_str("/info/refs?service=");
                out.push_str(git.service());
            } else {
                out.push('/');
                out.push_str(git.service());
            }
            return out;
        }
    }
    for segment in tail.iter() {
        out.push('/');
        out.push_str(&encode_segment(segment));
    }
    out
}

/// Percent-encode everything but the RFC 3986 unreserved characters.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One resolver call, bounded on the client side (the resolver bounds its
/// own statement too). A call that does not answer in time is
/// `FG-WEB-0002` with `operation=namespace_resolve`.
pub(crate) async fn resolve_bounded<S: DispatchState>(
    state: &S,
    path: &LookupPath,
    viewer: Option<UserId>,
) -> Result<Resolution, ForgeError> {
    let limit = state.lookup_timeout();
    match tokio::time::timeout(limit, state.resolver().resolve(path, viewer)).await {
        Ok(result) => result,
        Err(_) => Err(ForgeError::new(ErrorCode::RequestTimeout)
            .with_context("operation", "namespace_resolve")
            .with_context("timeout_ms", limit.as_millis())),
    }
}

/// forge-core's project access → forge-auth's loaded project (one value).
pub(crate) fn loaded_project(id: forge_core::ProjectId, access: ProjectAccess) -> LoadedProject {
    LoadedProject {
        id,
        visibility: access.visibility,
        role: access.role,
        pending_deletion: access.pending_deletion,
    }
}

/// forge-core's resolution → forge-auth's loaded namespace (`PathResolve`):
/// `Live` → `ResolvedVia::Live`, `Moved` → `ResolvedVia::Moved`.
pub(crate) fn loaded_namespace(resolved: &Resolved, via: ResolvedVia) -> LoadedNamespace {
    let view = match resolved.target {
        ResolvedTarget::User(_) => NamespaceView::User,
        ResolvedTarget::Group { viewer_role, .. } => NamespaceView::Group { role: viewer_role },
        ResolvedTarget::Project { access, .. } => NamespaceView::Project(ProjectView {
            visibility: access.visibility,
            role: access.role,
            pending_deletion: access.pending_deletion,
        }),
    };
    LoadedNamespace {
        namespace: resolved.namespace_id,
        via,
        view,
    }
}

/// The context `PathResolve` decides on, from one resolution:
/// `(Resolved or Unresolved context, canonical path, moved tombstone)`.
pub(crate) fn path_resolve_ctx(
    resolution: Resolution,
    requested: &LookupPath,
) -> Result<(ResolvedCtx, NamespacePath, Option<TombstoneId>), UnresolvedCtx> {
    match resolution {
        Resolution::Live(resolved) => Ok((
            ResolvedCtx::new(loaded_namespace(&resolved, ResolvedVia::Live)),
            requested.as_namespace_path().clone(), // clone: the request keeps its path
            None,
        )),
        Resolution::Moved {
            resolved,
            tombstone,
            current,
        } => Ok((
            ResolvedCtx::new(loaded_namespace(&resolved, ResolvedVia::Moved)),
            current,
            Some(tombstone),
        )),
        Resolution::Deleted { .. } => Err(UnresolvedCtx::new(UnresolvedReason::TombstoneDeleted)),
        Resolution::Absent => Err(UnresolvedCtx::new(UnresolvedReason::Absent)),
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
