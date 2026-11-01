//! The route registry (ADR-0010, ADR-0015): every route is registered once
//! with its method, path (or scope + route word), authorization `Action` and
//! a summary. From the same list come the axum router, the dispatcher's
//! table, `Git/docs/api/openapi.json` and `Git/docs/authz/routes.generated.json`
//! (the `authz-coverage` gate input). Never call `Router::route` directly.
//!
//! **A handler is reached only through the registry, with a proof.** A
//! handler is an [`Endpoint`]: `async fn(state, Dispatched<R>)`. A
//! [`Dispatched`] has private fields and one constructor, `admit()` in this
//! module, which checks `proof.action() == def.action` (ADR-0015 MR4 must a)
//! — so a handler cannot be called from anywhere else (must b), and a proof
//! for another action is refused (`FG-SYS-0001 reason=proof_action_mismatch`,
//! must c):
//!
//! ```compile_fail
//! use forge_web::registry::Dispatched;
//! use forge_auth::InstanceCtx;
//! let forged: Dispatched<InstanceCtx> = Default::default();
//! ```
//!
//! ```compile_fail
//! use forge_web::registry::Dispatched;
//! use forge_auth::InstanceCtx;
//! fn needs_clone<T: Clone>() {}
//! needs_clone::<Dispatched<InstanceCtx>>();
//! ```
//!
//! A body route's request is no different: the parsed body travels inside
//! the `Dispatched`, which still cannot be built outside this module.
//!
//! ```compile_fail
//! use forge_web::registry::Dispatched;
//! use forge_auth::InstanceCtx;
//! #[derive(Default)]
//! struct Change;
//! let forged: Dispatched<InstanceCtx, Change> = Default::default();
//! ```
//!
//! Three ways in:
//! - [`Registry::route`] / [`Registry::route_with`]: a fixed path
//!   (`/api/v1/…`, `/healthz`), mounted on the axum router. The registry
//!   authenticates (non-public), loads the resource with a [`Loader`]
//!   (`InstanceCtx` for `route`), calls `prove(principal, def.action, ctx)`
//!   and admits the proof.
//! - [`Registry::route_with_body`]: a fixed write route with a strict JSON
//!   body (M1 phase 8b MR6). The registry authenticates, reads the body
//!   ([`crate::body::read_json`]: `Content-Type`, the route's cap ≤ 64 KiB,
//!   unknown fields refused — all before any lookup), then a
//!   [`BodyLoader`] loads with the parsed body, `prove()` decides and the
//!   admitted proof reaches the handler with the body.
//! - [`Registry::scoped`]: a namespace URL (`/{ns}`, `/{ns}/-/{word}/…`,
//!   `/{ns}.git/…`, `/-/projects/{id}/…`), served by the one dispatcher
//!   (`crate::dispatch`) on the raw path, in ADR-0015's seven steps.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use axum::Router;
use forge_auth::{
    prove, Action, Authorized, InstanceCtx, Principal, ProjectCtx, ResourceCtx, UnresolvedCtx,
};
use forge_core::web_path::{GitRoute, InstanceRoute, RouteWord};
use forge_core::{
    GroupRecord, NamespacePath, NamespaceResolver, ProjectRecord, ProjectRepository, TailSegments,
    TombstoneId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Map, Value};
use utoipa::openapi::{RefOr, Schema};

use crate::body::{RequestBody, MAX_JSON_BODY_BYTES};
use crate::context::Ctx;

/// HTTP methods the routes use. `Head` is registered explicitly for the
/// scoped routes (ADR-0015); a fixed `Get` route also answers `HEAD`
/// (axum strips the body).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Verb {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Verb {
    pub const ALL: &'static [Verb] = &[
        Verb::Get,
        Verb::Head,
        Verb::Post,
        Verb::Put,
        Verb::Patch,
        Verb::Delete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Get => "GET",
            Verb::Head => "HEAD",
            Verb::Post => "POST",
            Verb::Put => "PUT",
            Verb::Patch => "PATCH",
            Verb::Delete => "DELETE",
        }
    }

    /// The verb of an HTTP method, `None` for any other method.
    pub fn of(method: &Method) -> Option<Verb> {
        Verb::ALL
            .iter()
            .copied()
            .find(|v| v.as_str() == method.as_str())
    }

    /// Reads only (a redirect keeps the method's meaning).
    pub fn is_safe(self) -> bool {
        matches!(self, Verb::Get | Verb::Head)
    }

    fn filter(self) -> MethodFilter {
        match self {
            Verb::Get => MethodFilter::GET,
            Verb::Head => MethodFilter::HEAD,
            Verb::Post => MethodFilter::POST,
            Verb::Put => MethodFilter::PUT,
            Verb::Patch => MethodFilter::PATCH,
            Verb::Delete => MethodFilter::DELETE,
        }
    }
}

/// Where a route lives (ADR-0015 "ルートの登録に足すもの").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteScope {
    /// A fixed path ([`Registry::route`]): `/api/v1/…`, `/healthz`.
    Fixed,
    /// A namespace home of any kind (`/{ns}`).
    Namespace,
    /// `/{group}/-/{word}/…`.
    Group,
    /// `/{project}/-/{word}/…`.
    Project,
    /// `/{project}.git/…` (Smart HTTP, M1 phase 9).
    ProjectGit,
    /// `/-/{route}/…` (no namespace).
    Instance,
}

impl RouteScope {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteScope::Fixed => "fixed",
            RouteScope::Namespace => "namespace",
            RouteScope::Group => "group",
            RouteScope::Project => "project",
            RouteScope::ProjectGit => "project_git",
            RouteScope::Instance => "instance",
        }
    }
}

/// What a scoped route matches after the namespace (the dispatcher's
/// vocabulary, step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopedRoute {
    Home,
    Word(RouteWord),
    Git(GitRoute),
    Instance(InstanceRoute),
}

impl ScopedRoute {
    /// The manifest `word`.
    pub fn name(self) -> &'static str {
        match self {
            ScopedRoute::Home => "home",
            ScopedRoute::Word(w) => w.as_str(),
            ScopedRoute::Git(g) => g.as_str(),
            ScopedRoute::Instance(i) => i.as_str(),
        }
    }

    /// The scopes this route can be registered in.
    fn scopes(self) -> &'static [RouteScope] {
        match self {
            ScopedRoute::Home => &[RouteScope::Namespace],
            ScopedRoute::Word(_) => &[RouteScope::Group, RouteScope::Project],
            ScopedRoute::Git(_) => &[RouteScope::ProjectGit],
            ScopedRoute::Instance(_) => &[RouteScope::Instance],
        }
    }

    /// The `query_key` the route must carry (`info/refs` only).
    fn query_key(self) -> Option<(&'static str, &'static str)> {
        match self {
            ScopedRoute::Git(g) if g.is_advertisement() => Some(("service", g.service())),
            ScopedRoute::Git(_)
            | ScopedRoute::Home
            | ScopedRoute::Word(_)
            | ScopedRoute::Instance(_) => None,
        }
    }

    /// The verbs the route's shape allows.
    fn allows(self, verb: Verb) -> bool {
        match self {
            ScopedRoute::Git(g) if g.is_advertisement() => verb.is_safe(),
            ScopedRoute::Git(_) => verb == Verb::Post,
            ScopedRoute::Home | ScopedRoute::Word(_) | ScopedRoute::Instance(_) => true,
        }
    }

    /// The manifest path template (never matched by axum; the dispatcher
    /// reads the raw path).
    pub fn path_template(self) -> &'static str {
        match self {
            ScopedRoute::Home => "/{namespace}",
            ScopedRoute::Word(w) => match w {
                RouteWord::Tree => "/{namespace}/-/tree/{*rest}",
                RouteWord::Blob => "/{namespace}/-/blob/{*rest}",
                RouteWord::Raw => "/{namespace}/-/raw/{*rest}",
                RouteWord::Blame => "/{namespace}/-/blame/{*rest}",
                RouteWord::Commits => "/{namespace}/-/commits/{*rest}",
                RouteWord::Commit => "/{namespace}/-/commit/{*rest}",
                RouteWord::Compare => "/{namespace}/-/compare/{*rest}",
                RouteWord::Branches => "/{namespace}/-/branches/{*rest}",
                RouteWord::Tags => "/{namespace}/-/tags/{*rest}",
                RouteWord::Archive => "/{namespace}/-/archive/{*rest}",
                RouteWord::Settings => "/{namespace}/-/settings/{*rest}",
                RouteWord::Members => "/{namespace}/-/members/{*rest}",
            },
            ScopedRoute::Git(g) => match g {
                GitRoute::InfoRefsUploadPack | GitRoute::InfoRefsReceivePack => {
                    "/{namespace}.git/info/refs"
                }
                GitRoute::UploadPack => "/{namespace}.git/git-upload-pack",
                GitRoute::ReceivePack => "/{namespace}.git/git-receive-pack",
            },
            ScopedRoute::Instance(i) => match i {
                InstanceRoute::ProjectPermalink => "/-/projects/{id}/{*rest}",
                InstanceRoute::Assets => "/-/assets/{file}",
            },
        }
    }
}

/// One registered route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDef {
    pub scope: RouteScope,
    pub verb: Verb,
    /// axum 0.8 / OpenAPI style: `/api/v1/projects/{id}`; for a scoped
    /// route the manifest template ([`ScopedRoute::path_template`]).
    pub path: &'static str,
    /// `None` for a fixed route.
    pub route: Option<ScopedRoute>,
    /// `info/refs?service=…`: part of the route's identity.
    pub query_key: Option<(&'static str, &'static str)>,
    pub action: Action,
    pub summary: &'static str,
    /// The JSON request body (`route_with_body`), with its cap.
    pub request: Option<(SchemaDoc, usize)>,
    /// The success response body and its status, when documented
    /// ([`Registry::responds`]: `200`; [`Registry::responds_created`]: `201`).
    pub response: Option<(SchemaDoc, u16)>,
    /// The route answers `204 No Content` on success ([`Registry::responds_no_content`],
    /// a delete, M1 phase 8b MR8).
    pub no_content: bool,
    /// The documented query parameters ([`Registry::queries`]).
    pub query: &'static [QueryParam],
}

/// One optional query parameter of a fixed route, for `openapi.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryParam {
    pub name: &'static str,
    pub description: &'static str,
    /// `(minimum, maximum)` for an integer parameter; `None` = a string.
    pub integer_range: Option<(i64, i64)>,
}

/// A named OpenAPI schema of a Rust type (utoipa), for the generated
/// `openapi.json`. Compared by name: one name, one type.
#[derive(Clone, Copy)]
pub struct SchemaDoc {
    pub name: fn() -> String,
    /// The type's schema and the schemas it references.
    pub schemas: fn() -> Vec<(String, RefOr<Schema>)>,
}

impl SchemaDoc {
    pub fn of<T: utoipa::ToSchema>() -> Self {
        fn name<T: utoipa::ToSchema>() -> String {
            T::name().into_owned()
        }
        fn schemas<T: utoipa::ToSchema>() -> Vec<(String, RefOr<Schema>)> {
            let mut all = vec![(T::name().into_owned(), T::schema())];
            T::schemas(&mut all);
            all
        }
        Self {
            name: name::<T>,
            schemas: schemas::<T>,
        }
    }
}

impl std::fmt::Debug for SchemaDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SchemaDoc({})", (self.name)())
    }
}

impl PartialEq for SchemaDoc {
    fn eq(&self, other: &Self) -> bool {
        (self.name)() == (other.name)()
    }
}

impl Eq for SchemaDoc {}

impl RouteDef {
    /// The manifest `word` (`fixed` for a fixed route).
    pub fn word(&self) -> &'static str {
        self.route.map_or("fixed", ScopedRoute::name)
    }
}

/// The future an [`Endpoint`] returns.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// A handler: `async fn(state, Dispatched<R, B>) -> impl IntoResponse`
/// (`B` = `()` but for a body route). It can only run with a
/// [`Dispatched`], which only the registry makes.
pub trait Endpoint<S, R: ResourceCtx, B = ()>: Send + Sync + 'static {
    fn call(&self, state: S, request: Dispatched<R, B>) -> HandlerFuture;
}

impl<S, R, B, F, Fut, O> Endpoint<S, R, B> for F
where
    R: ResourceCtx + 'static,
    F: Fn(S, Dispatched<R, B>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = O> + Send + 'static,
    O: IntoResponse,
{
    fn call(&self, state: S, request: Dispatched<R, B>) -> HandlerFuture {
        let fut = self(state, request);
        Box::pin(async move { fut.await.into_response() })
    }
}

/// The request a handler receives: the proof, the request context, what is
/// left of the HTTP request and, on a body route, the parsed body. Private
/// fields, no `Clone` / `Default`, one constructor (`admit`, this module):
/// holding one means the registry checked `proof.action()` against the
/// route's action.
pub struct Dispatched<R: ResourceCtx, B = ()> {
    proof: Authorized<R>,
    request: Admitted,
    input: B,
}

/// The parts of a request a handler may read.
struct Admitted {
    ctx: Ctx,
    principal: Principal,
    parts: Parts,
    body: Body,
    tail: TailSegments,
    canonical: Option<NamespacePath>,
    tombstone: Option<TombstoneId>,
    project: Option<ProjectRecord>,
    group: Option<Box<GroupRecord>>,
    repository: Option<ProjectRepository>,
}

impl<R: ResourceCtx, B> Dispatched<R, B> {
    /// The parsed request body of a body route (`()` elsewhere): the same
    /// value the [`BodyLoader`] loaded with and `authorize()` decided on.
    pub fn input(&self) -> &B {
        &self.input
    }

    /// The decision (action, target id, context). Handlers take the target
    /// id from here, never from the path.
    pub fn proof(&self) -> &Authorized<R> {
        &self.proof
    }

    pub fn ctx(&self) -> &Ctx {
        &self.request.ctx
    }

    pub fn principal(&self) -> &Principal {
        &self.request.principal
    }

    pub fn method(&self) -> &Method {
        &self.request.parts.method
    }

    /// The headers (`Authorization` already removed after authentication).
    pub fn headers(&self) -> &HeaderMap {
        &self.request.parts.headers
    }

    /// The TCP peer of the connection (`crate::server::PeerAddr`), set by
    /// `server::serve` on every request; `None` only outside the server
    /// (in-process tests that do not set it).
    pub fn peer(&self) -> Option<std::net::IpAddr> {
        self.request
            .parts
            .extensions
            .get::<crate::server::PeerAddr>()
            .map(|p| p.0)
    }

    /// The raw query (without `?`).
    pub fn raw_query(&self) -> Option<&str> {
        self.request.parts.uri.query()
    }

    /// The decoded, validated segments after the route word (scoped routes;
    /// empty for fixed ones). A segment may start with `-`: pass it to git
    /// only after `--` / `--end-of-options`, or resolve it to a full ref
    /// name / oid first (ADR-0015 "ref とパスの境目", phase 9).
    pub fn tail(&self) -> &TailSegments {
        &self.request.tail
    }

    /// The target's current path, revealed only now that `authorize()`
    /// allowed (a live path as requested, a moved one rebuilt by the
    /// resolver); `None` where the route has no namespace.
    pub fn canonical_path(&self) -> Option<&NamespacePath> {
        self.request.canonical.as_ref()
    }

    /// The `moved` tombstone the request came through (for the
    /// `namespace.redirected` event), revealed only after `authorize()`.
    pub fn via_tombstone(&self) -> Option<TombstoneId> {
        self.request.tombstone
    }

    /// The project row the loader read with the context (a project loaded
    /// by id, `GET /api/v1/projects/{id}`), revealed only after
    /// `authorize()`; `None` for every other route. Its `id` is the proof's.
    pub fn project(&self) -> Option<&ProjectRecord> {
        self.request.project.as_ref()
    }

    /// The group row the loader read with the context (`GET
    /// /api/v1/groups/{id}`, M1 phase 8b MR7a), revealed only after
    /// `authorize()`; `None` for every other route. Its `id` is the proof's.
    pub fn group(&self) -> Option<&GroupRecord> {
        self.request.group.as_deref()
    }

    /// The project's repository (storage id and state) the resolver read
    /// with the path (scoped project routes; Smart HTTP), revealed only
    /// after `authorize()`; `None` for every other route.
    pub fn repository(&self) -> Option<ProjectRepository> {
        self.request.repository
    }

    pub fn into_body(self) -> Body {
        self.request.body
    }

    /// The id of the personal access token the request authenticated with
    /// (`None` for a session).
    pub fn credential_id(&self) -> Option<i64> {
        self.request
            .parts
            .extensions
            .get::<crate::auth::TokenCredential>()
            .map(|t| t.0)
    }

    /// What the authenticator knows of the caller's account (username,
    /// display name, flags; a token's or a session's).
    pub fn caller(&self) -> Option<&crate::auth::CallerProfile> {
        self.request
            .parts
            .extensions
            .get::<crate::auth::CallerProfile>()
    }

    /// The web session the request authenticated with (`None` for a
    /// token; M2 MR3): ids, expiries and its CSRF token, never the cookie.
    pub fn session(&self) -> Option<&crate::auth::SessionCredential> {
        self.request
            .parts
            .extensions
            .get::<crate::auth::SessionCredential>()
    }

    /// What a write handler keeps across its awaits (the request's body
    /// stream is not `Sync`, so a handler must not hold the `Dispatched`
    /// itself): the proof, the parsed input, the request context and the
    /// token id (M1 phase 8b MR7a).
    pub fn into_write(self) -> WriteParts<R, B> {
        let credential_id = self.credential_id();
        WriteParts {
            proof: self.proof,
            input: self.input,
            ctx: self.request.ctx,
            credential_id,
        }
    }

    /// The proof, the principal and the body (receive-pack decides every
    /// ref on the project proof after it read the command list).
    pub fn into_proof_and_body(self) -> (Authorized<R>, Principal, Body) {
        (self.proof, self.request.principal, self.request.body)
    }
}

/// A write handler's part of a [`Dispatched`] ([`Dispatched::into_write`]):
/// still only reachable through the registry, and the proof stays
/// unforgeable.
pub struct WriteParts<R: ResourceCtx, B> {
    pub proof: Authorized<R>,
    pub input: B,
    pub ctx: Ctx,
    /// The personal access token the request authenticated with.
    pub credential_id: Option<i64>,
}

/// The only constructor of [`Dispatched`]: the proof must be for the
/// route's own action (ADR-0015 MR4 must a). A mismatch is a programming
/// error, never a pass: `FG-SYS-0001 reason=proof_action_mismatch`.
fn admit<R: ResourceCtx, B>(
    route_action: Action,
    proof: Authorized<R>,
    request: Admitted,
    input: B,
) -> Result<Dispatched<R, B>, ForgeError> {
    if proof.action() != route_action {
        return Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_action_mismatch")
            .with_context("route_action", route_action.name())
            .with_context("proof_action", proof.action().name()));
    }
    Ok(Dispatched {
        proof,
        request,
        input,
    })
}

/// What a [`Loader`] found for the request.
pub enum Lookup<R> {
    /// The resource context to decide on; `canonical` (the target's
    /// current path), `tombstone` and `project` (the row a project was
    /// loaded from, with what its route shows) are handed to the handler
    /// only after `authorize()`.
    Found {
        ctx: R,
        canonical: Option<NamespacePath>,
        tombstone: Option<TombstoneId>,
        project: Option<ProjectRecord>,
        /// The group row a group was loaded from (`None` elsewhere; boxed:
        /// the variants stay small).
        group: Option<Box<GroupRecord>>,
    },
    /// Nothing the route can act on: decided by `authorize()` like every
    /// other request (`FG-REPO-0005`, after authentication). `why` is
    /// logged as `lookup` (e.g. a malformed `path` parameter).
    Unresolved {
        ctx: UnresolvedCtx,
        why: Option<&'static str>,
    },
}

/// The future a [`Loader`] returns.
pub type LoadFuture<'a, R> =
    Pin<Box<dyn Future<Output = Result<Lookup<R>, ForgeError>> + Send + 'a>>;

/// Loads the resource a fixed route decides on, from the request and the
/// state, for `viewer` (`None` = anonymous). The registry calls it after
/// authentication and before `prove()`; a loader never decides.
pub trait Loader<S>: Send + Sync + 'static {
    type Ctx: ResourceCtx<Id: Send> + Send + 'static;
    fn load<'a>(
        &'a self,
        state: &'a S,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, Self::Ctx>;
}

/// The instance itself: no I/O.
pub struct InstanceLoader;

impl<S: Sync> Loader<S> for InstanceLoader {
    type Ctx = InstanceCtx;
    fn load<'a>(
        &'a self,
        _: &'a S,
        _: &'a Parts,
        _: Option<UserId>,
    ) -> LoadFuture<'a, InstanceCtx> {
        Box::pin(async {
            Ok(Lookup::Found {
                ctx: InstanceCtx,
                canonical: None,
                tombstone: None,
                project: None,
                group: None,
            })
        })
    }
}

/// Loads the resource a body route decides on, from the request, the
/// **parsed body** and the state, for `viewer`. The registry calls it after
/// authentication and after the body was read and validated, before
/// `prove()`; a loader never decides (a body naming something the caller
/// may not see is answered by `authorize()`, like an absent one).
pub trait BodyLoader<S, B>: Send + Sync + 'static {
    type Ctx: ResourceCtx<Id: Send> + Send + 'static;
    fn load<'a>(
        &'a self,
        state: &'a S,
        parts: &'a Parts,
        body: &'a B,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, Self::Ctx>;
}

/// What the dispatcher needs from the state.
pub trait DispatchState: Clone + Send + Sync + 'static {
    /// Path → namespace + the viewer's authorization inputs (one call).
    fn resolver(&self) -> &dyn NamespaceResolver;
    /// Client-side bound of one resolver call.
    fn lookup_timeout(&self) -> Duration;
}

mod sealed {
    pub trait Sealed {}
}

/// The contexts a scoped handler can take: [`InstanceCtx`] (public routes:
/// the shell, which never resolves) and [`ProjectCtx`] (project and git
/// routes). Group and personal-namespace contexts join with their first
/// non-public scoped route. Sealed.
pub trait ScopeCtx: ResourceCtx + Send + Sized + 'static + sealed::Sealed {
    #[doc(hidden)]
    fn erase<S: 'static>(endpoint: Arc<dyn Endpoint<S, Self>>) -> ScopedHandler<S>;
}

impl sealed::Sealed for InstanceCtx {}
impl ScopeCtx for InstanceCtx {
    fn erase<S: 'static>(endpoint: Arc<dyn Endpoint<S, Self>>) -> ScopedHandler<S> {
        ScopedHandler::Instance(endpoint)
    }
}

impl sealed::Sealed for ProjectCtx {}
impl ScopeCtx for ProjectCtx {
    fn erase<S: 'static>(endpoint: Arc<dyn Endpoint<S, Self>>) -> ScopedHandler<S> {
        ScopedHandler::Project(endpoint)
    }
}

/// A scoped route's handler, by the context it takes.
#[doc(hidden)]
pub enum ScopedHandler<S> {
    Instance(Arc<dyn Endpoint<S, InstanceCtx>>),
    Project(Arc<dyn Endpoint<S, ProjectCtx>>),
}

impl<S> Clone for ScopedHandler<S> {
    fn clone(&self) -> Self {
        match self {
            Self::Instance(e) => Self::Instance(Arc::clone(e)),
            Self::Project(e) => Self::Project(Arc::clone(e)),
        }
    }
}

/// One scoped route of the dispatcher's table.
pub(crate) struct ScopedEntry<S> {
    pub(crate) def: RouteDef,
    pub(crate) handler: ScopedHandler<S>,
}

/// The dispatcher's table, built from the `scoped()` registrations.
pub(crate) struct ScopedTable<S> {
    pub(crate) entries: Vec<ScopedEntry<S>>,
}

impl<S> ScopedTable<S> {
    /// Every entry for `(verb, route)`, any scope (step 2).
    pub(crate) fn candidates(&self, verb: Verb, route: ScopedRoute) -> Vec<&ScopedEntry<S>> {
        self.entries
            .iter()
            .filter(|e| e.def.verb == verb && e.def.route == Some(route))
            .collect()
    }

    /// The entry for `(scope, verb, route)` (step 5).
    pub(crate) fn find(
        &self,
        scope: RouteScope,
        verb: Verb,
        route: ScopedRoute,
    ) -> Option<&ScopedEntry<S>> {
        self.entries
            .iter()
            .find(|e| e.def.scope == scope && e.def.verb == verb && e.def.route == Some(route))
    }

    /// Public only when every route with `(verb, route)` is public (and
    /// there is one), ADR-0015 step 4. Registration refuses a word that
    /// mixes a public and a non-public route (`FG-WEB-0004`), so for a
    /// built table "every" and "any" agree; the check stays "every" so a
    /// mixed word could only ever require authentication, never skip it.
    pub(crate) fn is_public_word(&self, verb: Verb, route: ScopedRoute) -> bool {
        let candidates = self.candidates(verb, route);
        !candidates.is_empty() && candidates.iter().all(|e| e.def.action.is_public())
    }
}

/// Collects routes; turns into the router and the generated documents.
pub struct Registry<S> {
    defs: Vec<RouteDef>,
    routers: BTreeMap<&'static str, MethodRouter<S>>,
    scoped: ScopedTable<S>,
    problems: Vec<String>,
}

impl<S: DispatchState> Default for Registry<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// A path axum accepts and the manifests can describe: non-empty, starts
/// with '/', no empty segment.
fn valid_path(path: &str) -> bool {
    path == "/" || (path.starts_with('/') && path.split('/').skip(1).all(|seg| !seg.is_empty()))
}

impl<S: DispatchState> Registry<S> {
    pub fn new() -> Self {
        Self {
            defs: Vec::new(),
            routers: BTreeMap::new(),
            scoped: ScopedTable {
                entries: Vec::new(),
            },
            problems: Vec::new(),
        }
    }

    /// Register `verb path` on the instance (`InstanceCtx`): the registry
    /// proves `action` and hands the handler the admitted proof.
    #[must_use]
    pub fn route<H>(
        self,
        verb: Verb,
        path: &'static str,
        action: Action,
        summary: &'static str,
        handler: H,
    ) -> Self
    where
        H: Endpoint<S, InstanceCtx>,
    {
        self.route_with(verb, path, action, summary, InstanceLoader, handler)
    }

    /// Register `verb path`, deciding `action` on what `loader` loads.
    ///
    /// Every non-public route is wrapped in token authentication
    /// ([`crate::auth`]) and the authentication guard: without an
    /// authenticated, active `Principal` it answers `FG-AUTH-0001` /
    /// `FG-AUTH-0004` and nothing is loaded. Then the loader runs once,
    /// `prove(principal, action, ctx)` decides, and only an admitted proof
    /// reaches the handler. Public routes never consult the credentials.
    #[must_use]
    pub fn route_with<L, H>(
        mut self,
        verb: Verb,
        path: &'static str,
        action: Action,
        summary: &'static str,
        loader: L,
        handler: H,
    ) -> Self
    where
        L: Loader<S>,
        H: Endpoint<S, L::Ctx>,
    {
        if !self.push_fixed(verb, path, action, summary, None) {
            return self;
        }
        let loader = Arc::new(loader);
        let endpoint: Arc<dyn Endpoint<S, L::Ctx>> = Arc::new(handler);
        let serve = move |State(state): State<S>, req: Request| {
            let loader = Arc::clone(&loader);
            let endpoint = Arc::clone(&endpoint);
            async move { serve_fixed(state, req, action, &*loader, &*endpoint).await }
        };
        self.mount(path, action, axum::routing::on(verb.filter(), serve));
        self
    }

    /// Register a write route `verb path` with a strict JSON body of type
    /// `B`, at most `max_body_bytes` (1 ..= 64 KiB), deciding `action` on
    /// what `loader` loads **with the parsed body** (M1 phase 8b MR6).
    ///
    /// Order, each step before the next can run: authentication (as for
    /// [`Registry::route_with`]) → the body ([`crate::body::read_json`]:
    /// `Content-Type`, size, idle, strict JSON → `FG-WEB-0006` /
    /// `FG-WEB-0009`, no lookup) → the loader → `prove()` → `admit()` (the
    /// proof's action must be the route's) → the handler, which gets the
    /// body as [`Dispatched::input`]. A safe verb, a public action or a cap
    /// out of range is a startup error (`FG-WEB-0004`).
    #[allow(clippy::too_many_arguments)] // route_with's signature plus the cap
    #[must_use]
    pub fn route_with_body<L, B, H>(
        mut self,
        verb: Verb,
        path: &'static str,
        action: Action,
        summary: &'static str,
        max_body_bytes: usize,
        loader: L,
        handler: H,
    ) -> Self
    where
        L: BodyLoader<S, B>,
        B: RequestBody,
        H: Endpoint<S, L::Ctx, B>,
    {
        let label = format!("{} {path}", verb.as_str());
        let problem = if verb.is_safe() {
            Some("body_route_needs_an_unsafe_verb")
        } else if action.is_public() {
            Some("body_route_must_authenticate")
        } else if max_body_bytes == 0 || max_body_bytes > MAX_JSON_BODY_BYTES {
            Some("body_limit_out_of_range")
        } else {
            None
        };
        if let Some(reason) = problem {
            self.problems.push(format!("{label}: {reason}"));
            return self;
        }
        let request = Some((SchemaDoc::of::<B>(), max_body_bytes));
        if !self.push_fixed(verb, path, action, summary, request) {
            return self;
        }
        let loader = Arc::new(loader);
        let endpoint: Arc<dyn Endpoint<S, L::Ctx, B>> = Arc::new(handler);
        let serve = move |State(state): State<S>, req: Request| {
            let loader = Arc::clone(&loader);
            let endpoint = Arc::clone(&endpoint);
            async move {
                serve_fixed_body(
                    state,
                    req,
                    (path, action, max_body_bytes),
                    &*loader,
                    &*endpoint,
                )
                .await
            }
        };
        self.mount(path, action, axum::routing::on(verb.filter(), serve));
        self
    }

    /// Document the success body of the fixed route `verb path` (its
    /// `200` response in `openapi.json`). An unknown route is a startup
    /// error (`FG-WEB-0004`).
    #[must_use]
    pub fn responds<T: utoipa::ToSchema>(mut self, verb: Verb, path: &'static str) -> Self {
        match self
            .defs
            .iter_mut()
            .find(|d| d.scope == RouteScope::Fixed && d.verb == verb && d.path == path)
        {
            Some(def) => def.response = Some((SchemaDoc::of::<T>(), 200)),
            None => self
                .problems
                .push(format!("responds: no route {} {path}", verb.as_str())),
        }
        self
    }

    /// Document the `201 Created` body of the fixed route `verb path` (a
    /// create, M1 phase 8b MR7a). An unknown route is a startup error.
    #[must_use]
    pub fn responds_created<T: utoipa::ToSchema>(mut self, verb: Verb, path: &'static str) -> Self {
        match self
            .defs
            .iter_mut()
            .find(|d| d.scope == RouteScope::Fixed && d.verb == verb && d.path == path)
        {
            Some(def) => def.response = Some((SchemaDoc::of::<T>(), 201)),
            None => self.problems.push(format!(
                "responds_created: no route {} {path}",
                verb.as_str()
            )),
        }
        self
    }

    /// Document the `204 No Content` success of the fixed route `verb path`
    /// (a delete, M1 phase 8b MR8). An unknown route is a startup error.
    #[must_use]
    pub fn responds_no_content(mut self, verb: Verb, path: &'static str) -> Self {
        match self
            .defs
            .iter_mut()
            .find(|d| d.scope == RouteScope::Fixed && d.verb == verb && d.path == path)
        {
            Some(def) => def.no_content = true,
            None => self.problems.push(format!(
                "responds_no_content: no route {} {path}",
                verb.as_str()
            )),
        }
        self
    }

    /// Document the optional query parameters of the fixed route `verb
    /// path` in `openapi.json` (the handler parses them). An unknown route
    /// is a startup error (`FG-WEB-0004`).
    #[must_use]
    pub fn queries(
        mut self,
        verb: Verb,
        path: &'static str,
        params: &'static [QueryParam],
    ) -> Self {
        match self
            .defs
            .iter_mut()
            .find(|d| d.scope == RouteScope::Fixed && d.verb == verb && d.path == path)
        {
            Some(def) => def.query = params,
            None => self
                .problems
                .push(format!("queries: no route {} {path}", verb.as_str())),
        }
        self
    }

    /// Validate and record a fixed route's definition; `false` (with a
    /// problem) when it cannot be mounted.
    fn push_fixed(
        &mut self,
        verb: Verb,
        path: &'static str,
        action: Action,
        summary: &'static str,
        request: Option<(SchemaDoc, usize)>,
    ) -> bool {
        if !valid_path(path) {
            self.problems.push(format!("invalid path {path:?}"));
            return false;
        }
        if self
            .defs
            .iter()
            .any(|d| d.scope == RouteScope::Fixed && d.verb == verb && d.path == path)
        {
            // axum would panic on an overlapping route; fail at build instead.
            self.problems
                .push(format!("duplicate {} {path}", verb.as_str()));
            return false;
        }
        self.defs.push(RouteDef {
            scope: RouteScope::Fixed,
            verb,
            path,
            route: None,
            query_key: None,
            action,
            summary,
            request,
            response: None,
            no_content: false,
            query: &[],
        });
        true
    }

    /// Mount a fixed route's method router, behind authentication and the
    /// guard unless the action is public.
    fn mount(&mut self, path: &'static str, action: Action, mut one: MethodRouter<S>) {
        if !action.is_public() {
            // The later route_layer runs first: token authentication builds
            // the principal, then the guard requires it.
            one = one
                .route_layer(axum::middleware::from_fn_with_state(
                    action,
                    crate::guard::require_principal,
                ))
                .route_layer(axum::middleware::from_fn(crate::auth::authenticate));
        }
        let merged = match self.routers.remove(path) {
            Some(existing) => existing.merge(one),
            None => one,
        };
        self.routers.insert(path, merged);
    }

    /// Register a scoped route for the dispatcher (ADR-0015): `(scope, verb,
    /// route, query_key)` is unique. The handler's context type must fit:
    /// public actions (`WebShell`) take [`InstanceCtx`] and never resolve;
    /// non-public routes are project-scoped ([`ProjectCtx`]) for now. A
    /// word (`verb`, `route`) is either public in every scope or in none:
    /// a mixed word would need a group shell served after authentication
    /// and resolution without a proof of its own, so it is refused. A
    /// registration that does not fit is a startup error (`FG-WEB-0004`).
    #[allow(clippy::too_many_arguments)] // the ADR-0015 signature
    #[must_use]
    pub fn scoped<R, H>(
        mut self,
        scope: RouteScope,
        verb: Verb,
        route: ScopedRoute,
        query_key: Option<(&'static str, &'static str)>,
        action: Action,
        summary: &'static str,
        handler: H,
    ) -> Self
    where
        R: ScopeCtx,
        H: Endpoint<S, R>,
    {
        let label = format!("[{}] {} {}", scope.as_str(), verb.as_str(), route.name());
        let endpoint: Arc<dyn Endpoint<S, R>> = Arc::new(handler);
        let handler = R::erase(endpoint);
        let problem = if !route.scopes().contains(&scope) {
            Some("scope_does_not_fit_route")
        } else if query_key != route.query_key() {
            Some("query_key_does_not_fit_route")
        } else if !route.allows(verb) {
            Some("verb_does_not_fit_route")
        } else {
            match (&handler, action.is_public()) {
                (ScopedHandler::Instance(_), true) => None,
                (ScopedHandler::Instance(_), false) => {
                    Some("non_public_route_needs_a_resolved_context")
                }
                (ScopedHandler::Project(_), true) => Some("public_route_never_resolves"),
                (ScopedHandler::Project(_), false)
                    if matches!(scope, RouteScope::Project | RouteScope::ProjectGit) =>
                {
                    None
                }
                (ScopedHandler::Project(_), false) => Some("project_context_outside_project_scope"),
            }
        };
        if let Some(reason) = problem {
            self.problems.push(format!("{label}: {reason}"));
            return self;
        }
        if self.scoped.find(scope, verb, route).is_some() {
            self.problems.push(format!("duplicate {label}"));
            return self;
        }
        if self
            .scoped
            .candidates(verb, route)
            .iter()
            .any(|e| e.def.action.is_public() != action.is_public())
        {
            self.problems
                .push(format!("{label}: word_mixes_public_and_non_public"));
            return self;
        }
        let def = RouteDef {
            scope,
            verb,
            path: route.path_template(),
            route: Some(route),
            query_key,
            action,
            summary,
            request: None,
            response: None,
            no_content: false,
            query: &[],
        };
        self.defs.push(def.clone());
        self.scoped.entries.push(ScopedEntry { def, handler });
        self
    }

    /// `true` only if every route with `(verb, route)` is public (step 4);
    /// the dispatcher's own check ([`ScopedTable::is_public_word`]).
    pub fn is_public_word(&self, verb: Verb, route: ScopedRoute) -> bool {
        self.scoped.is_public_word(verb, route)
    }

    pub fn defs(&self) -> &[RouteDef] {
        &self.defs
    }

    /// The router (crate-private: routes are added only through the
    /// registry): the fixed routes, and the dispatcher as the fallback for
    /// every other path (fail closed: a path in no table is `FG-WEB-0001`).
    /// An invalid or duplicate registration is a startup failure
    /// (`FG-WEB-0004`), not a panic.
    pub(crate) fn into_router(self) -> Result<Router<S>, ForgeError> {
        if !self.problems.is_empty() {
            return Err(ForgeError::new(ErrorCode::RouteRegistryInvalid)
                .with_context("reason", "invalid_or_duplicate_route")
                .with_context("routes", self.problems.join(", ")));
        }
        let table = Arc::new(self.scoped);
        let dispatcher = move |State(state): State<S>, req: Request| {
            let table = Arc::clone(&table);
            async move { crate::dispatch::dispatch(&table, state, req).await }
        };
        Ok(self
            .routers
            .into_iter()
            .fold(Router::new(), |router, (path, mr)| router.route(path, mr))
            .fallback(dispatcher))
    }
}

/// The principal of a fixed route's request (set by the authentication
/// layer; public routes decide as anonymous).
fn fixed_principal(parts: &Parts, action: Action) -> Principal {
    if action.is_public() {
        Principal::anonymous()
    } else {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .unwrap_or_else(Principal::anonymous)
    }
}

/// A fixed route: load, prove, admit, call.
async fn serve_fixed<S, L>(
    state: S,
    req: Request,
    action: Action,
    loader: &L,
    endpoint: &dyn Endpoint<S, L::Ctx>,
) -> Response
where
    S: Send + Sync + 'static,
    L: Loader<S>,
{
    let (parts, body) = req.into_parts();
    let ctx = Ctx::of(&parts);
    let principal = fixed_principal(&parts, action);
    let lookup = loader.load(&state, &parts, principal.user()).await;
    let request = Admitted {
        ctx: ctx.clone(), // clone: the error paths still need it
        principal,
        parts,
        body,
        tail: TailSegments::default(),
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
        repository: None,
    };
    match decide(action, lookup, request, ()) {
        Ok(dispatched) => endpoint.call(state, dispatched).await,
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// A body route: read the body (before any lookup), load with it, prove,
/// admit, call.
async fn serve_fixed_body<S, L, B>(
    state: S,
    req: Request,
    (path, action, cap): (&'static str, Action, usize),
    loader: &L,
    endpoint: &dyn Endpoint<S, L::Ctx, B>,
) -> Response
where
    S: Send + Sync + 'static,
    L: BodyLoader<S, B>,
    B: RequestBody,
{
    let (parts, body) = req.into_parts();
    let ctx = Ctx::of(&parts);
    let input: B = match crate::body::read_json(&parts, body, cap).await {
        Ok(input) => input,
        Err(e) => {
            return ctx
                .fail(
                    e.with_context("action", action.name())
                        .with_context("route", path),
                )
                .into_response()
        }
    };
    let principal = fixed_principal(&parts, action);
    let lookup = loader.load(&state, &parts, &input, principal.user()).await;
    let request = Admitted {
        ctx: ctx.clone(), // clone: the error paths still need it
        principal,
        parts,
        body: Body::empty(), // read above: a handler gets the parsed input
        tail: TailSegments::default(),
        canonical: None,
        tombstone: None,
        project: None,
        group: None,
        repository: None,
    };
    match decide(action, lookup, request, input) {
        Ok(dispatched) => endpoint.call(state, dispatched).await,
        Err(e) => ctx.fail(e).into_response(),
    }
}

/// The loader's answer → `prove()` → `admit()`: the one path from a fixed
/// route's lookup to a handler (a loader error, an unresolved lookup and a
/// denial all end here, before any handler).
fn decide<R: ResourceCtx, B>(
    action: Action,
    lookup: Result<Lookup<R>, ForgeError>,
    mut request: Admitted,
    input: B,
) -> Result<Dispatched<R, B>, ForgeError> {
    let lookup = lookup.map_err(|e| e.with_context("action", action.name()))?;
    let resource = match lookup {
        Lookup::Found {
            ctx,
            canonical,
            tombstone,
            project,
            group,
        } => {
            request.canonical = canonical;
            request.tombstone = tombstone;
            request.project = project;
            request.group = group;
            ctx
        }
        Lookup::Unresolved { ctx, why } => {
            let err = deny_unresolved(&request.principal, action, ctx);
            return Err(match why {
                Some(why) => err.with_context("lookup", why),
                None => err,
            });
        }
    };
    let proof = prove(&request.principal, action, resource)?;
    admit(action, proof, request, input)
}

/// Decide an unresolved path through `prove()` (one decision point). It
/// fails for every non-public action; an allowed one would be a route that
/// resolves without needing to, reported as `FG-SYS-0001`.
pub(crate) fn deny_unresolved(
    principal: &Principal,
    action: Action,
    unresolved: UnresolvedCtx,
) -> ForgeError {
    match prove(principal, action, unresolved) {
        Err(e) => e,
        Ok(_) => ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unresolved_path_allowed")
            .with_context("action", action.name()),
    }
}

/// Admit a scoped route's proof (the dispatcher's only way to a handler).
///
/// Invariant (the caller's, `crate::dispatch` only): `proof` was produced
/// by `prove(principal, def.action, ..)` for the entry `def` belongs to, on
/// the context that entry's handler takes. `admit()` re-checks
/// `proof.action() == def.action`; the context type is fixed by `R`. No
/// other module calls this (registered handlers get a `Dispatched` only
/// through the dispatcher).
#[allow(clippy::too_many_arguments)] // everything a handler may read, in one place
pub(crate) fn admit_scoped<R: ResourceCtx>(
    def: &RouteDef,
    proof: Authorized<R>,
    ctx: Ctx,
    principal: Principal,
    req: Request,
    tail: TailSegments,
    canonical: Option<NamespacePath>,
    repository: Option<ProjectRepository>,
) -> Result<Dispatched<R>, ForgeError> {
    let (parts, body) = req.into_parts();
    admit(
        def.action,
        proof,
        Admitted {
            ctx,
            principal,
            parts,
            body,
            tail,
            canonical,
            tombstone: None,
            project: None,
            group: None,
            repository,
        },
        (),
    )
}

/// `Git/docs/authz/routes.generated.json`: the routes (with `scope`,
/// `word` and, for `info/refs`, `query_key`), then the planned actions as
/// `{ "action": .., "planned": true }`.
pub fn routes_json(defs: &[RouteDef], planned: &[Action]) -> String {
    routes_json_with(defs, planned, &[])
}

/// [`routes_json`] plus the actions decided inside another route's handler
/// (`{"action", "decided_in": <path>}`, M2 MR4: `SessionCreate` in the
/// sign-in callback).
pub fn routes_json_with(
    defs: &[RouteDef],
    planned: &[Action],
    decided_in: &[(Action, &str)],
) -> String {
    let mut items: Vec<&RouteDef> = defs.iter().collect();
    items.sort_by(|a, b| {
        (a.path, a.scope, a.verb, a.query_key).cmp(&(b.path, b.scope, b.verb, b.query_key))
    });
    let list: Vec<Value> = items
        .iter()
        .map(|d| {
            let mut entry = json!({
                "method": d.verb.as_str(),
                "path": d.path,
                "scope": d.scope.as_str(),
                "word": d.word(),
                "action": d.action.name(),
                "public": d.action.is_public(),
            });
            if let (Some((key, value)), Some(obj)) = (d.query_key, entry.as_object_mut()) {
                obj.insert("query_key".into(), json!([key, value]));
            }
            entry
        })
        .chain(
            planned
                .iter()
                .map(|a| json!({ "action": a.name(), "planned": true })),
        )
        .chain(
            decided_in
                .iter()
                .map(|(a, path)| json!({ "action": a.name(), "decided_in": path })),
        )
        .collect();
    pretty(&Value::Array(list))
}

/// `Git/docs/api/openapi.json` (OpenAPI 3.1): the fixed routes only — a
/// scoped route spans several segments and is not an API operation
/// (ADR-0015). Every operation documents the error body and the security
/// requirement; a body route its `requestBody` (the Rust type's schema,
/// utoipa) and `x-forge-max-body-bytes`; a documented route its `200`
/// body. `{id}` is a positive int64. Deterministic: every map is sorted.
pub fn openapi_json(defs: &[RouteDef], version: &str) -> String {
    let mut paths: BTreeMap<&str, Map<String, Value>> = BTreeMap::new();
    let mut schemas: BTreeMap<String, Value> = BTreeMap::new();
    schemas.insert("Error".into(), error_schema());
    let mut problems: Vec<String> = Vec::new();
    let mut add_schemas = |doc: &SchemaDoc| -> String {
        for (name, schema) in (doc.schemas)() {
            let value = serde_json::to_value(&schema).unwrap_or(Value::Null);
            if let Some(existing) = schemas.get(&name) {
                if existing != &value {
                    problems.push(name.clone()); // one name, two shapes
                }
            }
            schemas.insert(name, value);
        }
        format!("#/components/schemas/{}", (doc.name)())
    };
    for d in defs.iter().filter(|d| d.scope == RouteScope::Fixed) {
        let mut op = Map::new();
        op.insert("operationId".into(), json!(operation_id(d.verb, d.path)));
        op.insert("summary".into(), json!(d.summary));
        op.insert("x-forge-action".into(), json!(d.action.name()));
        if d.action.is_public() {
            op.insert("security".into(), json!([]));
        }
        let params: BTreeSet<&str> = path_params(d.path).collect();
        let mut list: Vec<Value> = params
            .iter()
            .map(
                |p| json!({ "name": p, "in": "path", "required": true, "schema": param_schema(p) }),
            )
            .collect();
        list.extend(d.query.iter().map(|q| {
            let schema = match q.integer_range {
                Some((min, max)) => json!({ "type": "integer", "minimum": min, "maximum": max }),
                None => json!({ "type": "string" }),
            };
            json!({ "name": q.name, "in": "query", "required": false,
                    "description": q.description, "schema": schema })
        }));
        if !list.is_empty() {
            op.insert("parameters".into(), Value::Array(list));
        }
        if let Some((doc, cap)) = &d.request {
            let reference = add_schemas(doc);
            op.insert(
                "requestBody".into(),
                json!({
                    "required": true,
                    "content": { "application/json": { "schema": { "$ref": reference } } }
                }),
            );
            op.insert("x-forge-max-body-bytes".into(), json!(cap));
        }
        let mut responses = Map::new();
        if let Some((doc, status)) = &d.response {
            let reference = add_schemas(doc);
            let description = if *status == 201 { "Created" } else { "Success" };
            responses.insert(
                status.to_string(),
                json!({
                    "description": description,
                    "content": { "application/json": { "schema": { "$ref": reference } } }
                }),
            );
        }
        if d.no_content {
            responses.insert("204".into(), json!({ "description": "No Content" }));
        }
        responses.insert(
            "default".into(),
            json!({
                "description": "Error: registered code, localized message, request id",
                "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } }
            }),
        );
        op.insert("responses".into(), Value::Object(responses));
        paths
            .entry(d.path)
            .or_default()
            .insert(d.verb.as_str().to_ascii_lowercase(), Value::Object(op));
    }
    let mut components = json!({
        "securitySchemes": {
            "bearerToken": { "type": "http", "scheme": "bearer", "description": "Personal access token" }
        },
        "schemas": schemas,
    });
    if !problems.is_empty() {
        // Never silently pick one: the generated file says so, and the
        // registry test fails on it.
        if let Some(obj) = components.as_object_mut() {
            obj.insert("x-forge-schema-conflicts".into(), json!(problems));
        }
    }
    let doc = json!({
        "openapi": "3.1.0",
        "info": { "title": "GitStation API", "version": version },
        "paths": paths,
        "security": [ { "bearerToken": [] } ],
        "components": components,
    });
    pretty(&doc)
}

/// The error body every operation can answer.
fn error_schema() -> Value {
    json!({
        "type": "object",
        "required": ["error_code", "user_message", "request_id"],
        "additionalProperties": false,
        "properties": {
            "error_code": { "type": "string", "pattern": "^FG-[A-Z]+-[0-9]{4}$" },
            "user_message": { "type": "string" },
            "request_id": { "type": "string" }
        }
    })
}

/// A path parameter's schema: an `{id}` is a plain positive decimal
/// (anything else is `FG-WEB-0006`); every other parameter a string.
fn param_schema(name: &str) -> Value {
    if name == "id" || name.ends_with("_id") {
        json!({ "type": "integer", "format": "int64", "minimum": 1 })
    } else {
        json!({ "type": "string" })
    }
}

/// A document-unique OpenAPI operationId from the method and the path
/// (`GET /api/v1/projects/{id}` → `get_api_v1_projects_id`); (verb, path)
/// is unique among the fixed routes, so the id is too.
fn operation_id(verb: Verb, path: &str) -> String {
    let slug: String = path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    format!("{}_{slug}", verb.as_str().to_ascii_lowercase())
}

fn path_params(path: &str) -> impl Iterator<Item = &str> {
    path.split('/')
        .filter_map(|seg| seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string());
    s.push('\n');
    s
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
