//! Wiring: the registered routes, the dispatcher (the fallback for every
//! path no fixed route matches), and the middleware stack (outermost
//! first): request id + access log → 401 challenges
//! (`error::challenge_layer`, M2 MR3) → request timeout (git POSTs: their
//! own `git_rpc_timeout`, M1 phase 9 MR3) → panic catcher → the browser
//! origin check of state-changing requests (`csrf::browser_origin`, M2 MR3) →
//! authenticator extensions → body limit → routes (non-public fixed routes:
//! authentication (token or session cookie, `crate::auth`) → guard → load →
//! prove → handler; scoped routes: the dispatcher's seven steps,
//! `crate::dispatch`).

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
use forge_core::web_path::{parse_web_path, GitRoute, InstanceRoute, ParsedPath, RouteWord};
use forge_core::{
    log_safe_path, Authenticator, NamespaceResolver, ProjectAccessLoader, RequestId,
    SessionAuthenticator,
};
use forge_errors::{ErrorCode, ForgeError};
use futures_util::FutureExt;

use crate::auth::{AuthenticatorExt, SessionsExt};
use crate::context::{self, Ctx};
use crate::csrf::ExpectedOrigin;
use crate::error::ApiError;
use crate::registry::{DispatchState, Registry, RouteScope, ScopedRoute, Verb};
use crate::routes;
use crate::security_headers::SecurityHeaders;

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
    /// git, the storage root and the `git.http.*` limits for the Smart
    /// HTTP routes (`forge serve` always sets it; without it a git route
    /// answers `FG-SYS-0001 reason=smart_http_unconfigured`).
    pub smart_http: Option<Arc<routes::git::SmartHttp>>,
    /// The group / project write commands and the group loader (M1 phase
    /// 8b MR7a; `forge serve` always sets it; without it those routes
    /// answer `FG-SYS-0001 reason=hosting_unconfigured`).
    pub hosting: Option<Arc<forge_hosting::Hosting>>,
    /// The merge request commands and reads (M3 R1a, ADR-0024; `forge
    /// serve` always sets them; without them those routes answer
    /// `FG-SYS-0001 reason=review_unconfigured`).
    pub review: Option<Arc<forge_review::Review>>,
    /// The check run commands and reads (M3 C1, ADR-0024 §10; `forge
    /// serve` always sets them; without them those routes answer
    /// `FG-SYS-0001 reason=checks_unconfigured`).
    pub checks: Option<Arc<forge_checks::Checks>>,
    /// The document lint's migration setting (plan 13 Q13; set with the
    /// checks by [`AppState::with_checks`]; without it those routes answer
    /// `FG-SYS-0001 reason=checks_unconfigured`).
    pub doc_lint_migrations: Option<Arc<forge_checks::DocLintMigrations>>,
    /// CI pipelines (M4 C2; `forge serve` always sets them; without them
    /// those routes answer `FG-SYS-0001 reason=pipelines_unconfigured`).
    pub pipelines: Option<Arc<forge_pipelines::Pipelines>>,
    /// The integrations' event feed (M3 I1c, ADR-0016 §7; `forge serve`
    /// always sets it; without it the feed routes answer `FG-SYS-0001
    /// reason=feed_unconfigured`).
    pub feed: Option<Arc<forge_integ::Feed>>,
    /// Project webhooks (M3 I2, ADR-0016 §8; `forge serve` always sets
    /// them; without them those routes answer `FG-SYS-0001
    /// reason=webhooks_unconfigured`).
    pub webhooks: Option<Arc<forge_integ::webhooks::Webhooks>>,
    /// The push mirror's administration (M3 O2, ADR-0024 Amendment O2;
    /// `forge serve` always sets it with `mirror.*`; without it those
    /// routes answer `FG-SYS-0001 reason=push_mirrors_unconfigured`).
    pub push_mirrors: Option<Arc<forge_integ::mirror::PushMirrors>>,
    /// The caller's notification settings (M3 N1, ADR-0016 Amendment N1;
    /// `forge serve` always sets it; without it those routes answer
    /// `FG-SYS-0001 reason=notifications_unconfigured`).
    pub notifications: Option<Arc<forge_notify::Settings>>,
    /// The caller's in-app notifications (M3 U4; set with the settings by
    /// [`AppState::with_notifications`]; without it those routes answer
    /// `FG-SYS-0001 reason=in_app_notifications_unconfigured`).
    pub inbox: Option<Arc<forge_notify::Inbox>>,
    /// The list routes' page loaders and cursor keys (M1 phase 8b MR7b;
    /// `forge serve` always sets them, refusing to start without
    /// `api.cursor_keys_file`; without them a list answers `FG-SYS-0001
    /// reason=lists_unconfigured`).
    pub lists: Option<Arc<routes::lists::Lists>>,
    /// Resolves the web session cookie `__Host-gstn_session` (M2 MR3,
    /// ADR-0021 §5; `forge serve` passes `forge_db::sessions::SessionStore`
    /// behind the token permits). Without it a request carrying the cookie
    /// answers `FG-SYS-0001 reason=sessions_unconfigured`.
    pub sessions: Option<Arc<dyn SessionAuthenticator>>,
    /// The origin of `web.base_url` (`scheme://host[:port]`): what a
    /// browser's state-changing request must send as `Origin` (ADR-0021
    /// §6). `forge serve` requires it; without it every such request is
    /// refused (`FG-AUTH-0016 origin_mismatch base_url=unset`).
    pub web_origin: Option<Arc<str>>,
    /// The OIDC sign-in (M2 MR4, ADR-0021 §3; `forge serve` sets it when
    /// `auth.oidc` is configured). Without it `/-/auth/login` and
    /// `/-/auth/callback` answer `FG-AUTH-0018`.
    pub oidc: Option<Arc<forge_oidc::Oidc>>,
    /// The login start's rate limit per peer (`FG-WEB-0010`).
    pub login_limiter: Arc<routes::auth::LoginLimiter>,
    /// The callback's rate limit per peer (`FG-WEB-0010`, counted apart).
    pub callback_limiter: Arc<routes::auth::LoginLimiter>,
    /// `resolve?prefix=true`'s rate limit per caller (M2 MR6, `FG-WEB-0010`).
    pub resolve_limiter: Arc<routes::namespaces::PrefixLimiter>,
    /// The user search's rate limit per caller (M2 MR6, `FG-WEB-0010`).
    pub search_limiter: Arc<routes::namespaces::CallerLimiter>,
    /// Acting for a person (M3 S2, ADR-0025; `forge serve` always sets it,
    /// with or without `auth.act_as_hmac_keys_file`; without it every
    /// act-as request is `FG-AUTH-0003 reason=act_as_not_configured`).
    pub act_as: Option<Arc<forge_integ::ActAs>>,
    /// Failed resolutions of `Forge-Act-As` per service account (60 a
    /// minute), confirmations per person (30 in 10 minutes), claims per
    /// integration's service account (120 a minute): `FG-WEB-0010`.
    pub act_as_failures: Arc<routes::namespaces::CallerLimiter>,
    pub act_as_issue_limiter: Arc<routes::namespaces::CallerLimiter>,
    /// Repository searches acting for a person, per integration (M3 X3
    /// review): `FG-WEB-0010` beyond.
    pub act_as_search_limiter: Arc<routes::namespaces::CallerLimiter>,
    pub act_as_claim_limiter: Arc<routes::namespaces::CallerLimiter>,
    /// M3 X1: the spec references followed per caller (`FG-WEB-0010`).
    pub spec_refs_limiter: Arc<routes::namespaces::CallerLimiter>,
    /// M3 X1: one budget for a whole spec-refs request (`FG-WEB-0002
    /// operation=spec_refs`).
    pub spec_refs_budget: Duration,
    /// M3 X4: the spec API's requests per caller (`FG-WEB-0010`).
    pub spec_index_limiter: Arc<routes::namespaces::CallerLimiter>,
    /// M3 X4 review: the spec API's requests running per caller.
    pub spec_index_in_flight: Arc<routes::specs::InFlight>,
    /// M4 C5 review: the CI log and diagnosis reads per caller
    /// (`FG-WEB-0010 route=ci_output`) and those running per caller at
    /// once (`reason=in_flight`).
    pub ci_output_limiter: Arc<routes::namespaces::CallerLimiter>,
    pub ci_output_in_flight: Arc<routes::specs::InFlight>,
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
            smart_http: None,
            hosting: None,
            review: None,
            checks: None,
            doc_lint_migrations: None,
            pipelines: None,
            feed: None,
            webhooks: None,
            push_mirrors: None,
            notifications: None,
            inbox: None,
            lists: None,
            sessions: None,
            web_origin: None,
            oidc: None,
            login_limiter: Arc::default(),
            callback_limiter: Arc::default(),
            resolve_limiter: Arc::default(),
            search_limiter: Arc::new(routes::directory::search_limiter()),
            act_as: None,
            act_as_failures: Arc::new(crate::act_as::failure_limiter()),
            act_as_issue_limiter: Arc::new(routes::act_as::issue_limiter()),
            act_as_search_limiter: Arc::new(routes::integration_bindings::act_as_search_limiter()),
            act_as_claim_limiter: Arc::new(routes::act_as::claim_limiter()),
            spec_refs_limiter: Arc::new(routes::spec_refs::limiter()),
            spec_refs_budget: routes::spec_refs::BUDGET,
            spec_index_limiter: Arc::new(routes::specs::limiter()),
            spec_index_in_flight: Arc::default(),
            ci_output_limiter: Arc::new(routes::pipelines::output_limiter()),
            ci_output_in_flight: Arc::new(routes::specs::InFlight::new(
                routes::pipelines::OUTPUT_MAX_IN_FLIGHT,
            )),
        }
    }

    /// Acting for a person (M3 S2).
    #[must_use]
    pub fn with_act_as(mut self, act_as: forge_integ::ActAs) -> Self {
        self.act_as = Some(Arc::new(act_as));
        self
    }

    /// The spec-refs limiter (M3 X1; tests set a small one).
    #[must_use]
    pub fn with_spec_refs_limiter(mut self, limiter: routes::namespaces::CallerLimiter) -> Self {
        self.spec_refs_limiter = Arc::new(limiter);
        self
    }

    /// The spec API's limiter (M3 X4; tests set a small one).
    #[must_use]
    pub fn with_spec_index_limiter(mut self, limiter: routes::namespaces::CallerLimiter) -> Self {
        self.spec_index_limiter = Arc::new(limiter);
        self
    }

    /// The spec-refs request budget (M3 X1; tests set a small one).
    #[must_use]
    pub fn with_spec_refs_budget(mut self, budget: Duration) -> Self {
        self.spec_refs_budget = budget;
        self
    }

    /// The OIDC sign-in (M2 MR4).
    #[must_use]
    pub fn with_oidc(mut self, oidc: Arc<forge_oidc::Oidc>) -> Self {
        self.oidc = Some(oidc);
        self
    }

    /// The web session authenticator (M2 MR3).
    #[must_use]
    pub fn with_sessions(mut self, sessions: Arc<dyn SessionAuthenticator>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// The origin of `web.base_url` (`forge_core::config::BaseUrl::origin`).
    #[must_use]
    pub fn with_web_origin(mut self, origin: &str) -> Self {
        self.web_origin = Some(Arc::from(origin));
        self
    }

    #[must_use]
    pub fn with_hosting(mut self, hosting: forge_hosting::Hosting) -> Self {
        self.hosting = Some(Arc::new(hosting));
        self
    }

    /// The merge request commands (M3 R1a).
    #[must_use]
    pub fn with_review(mut self, review: forge_review::Review) -> Self {
        self.review = Some(Arc::new(review));
        self
    }

    /// The check run commands (M3 C1).
    #[must_use]
    pub fn with_checks(mut self, checks: forge_checks::Checks) -> Self {
        self.doc_lint_migrations = Some(Arc::new(checks.doc_lint_migrations()));
        self.checks = Some(Arc::new(checks));
        self
    }

    /// The notification settings commands (M3 N1) and the in-app
    /// notifications (M3 U4) on the same database.
    #[must_use]
    pub fn with_notifications(mut self, settings: forge_notify::Settings) -> Self {
        self.inbox = Some(Arc::new(forge_notify::Inbox::new(settings.db())));
        self.notifications = Some(Arc::new(settings));
        self
    }

    /// The integrations' event feed (M3 I1c).
    #[must_use]
    pub fn with_feed(mut self, feed: forge_integ::Feed) -> Self {
        self.feed = Some(Arc::new(feed));
        self
    }

    /// Project webhooks (M3 I2).
    #[must_use]
    pub fn with_webhooks(mut self, webhooks: forge_integ::webhooks::Webhooks) -> Self {
        self.webhooks = Some(Arc::new(webhooks));
        self
    }

    /// CI pipelines (M4 C2).
    #[must_use]
    pub fn with_pipelines(mut self, pipelines: forge_pipelines::Pipelines) -> Self {
        self.pipelines = Some(Arc::new(pipelines));
        self
    }

    /// The push mirror's administration (M3 O2).
    #[must_use]
    pub fn with_push_mirrors(mut self, mirrors: forge_integ::mirror::PushMirrors) -> Self {
        self.push_mirrors = Some(Arc::new(mirrors));
        self
    }

    #[must_use]
    pub fn with_smart_http(mut self, smart_http: routes::git::SmartHttp) -> Self {
        self.smart_http = Some(Arc::new(smart_http));
        self
    }

    #[must_use]
    pub fn with_lists(mut self, lists: routes::lists::Lists) -> Self {
        self.lists = Some(Arc::new(lists));
        self
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
            .field("smart_http", &self.smart_http.as_ref().map(|s| s.limits))
            .field("lists", &self.lists.is_some())
            .field("sessions", &self.sessions.is_some())
            .field("web_origin", &self.web_origin)
            .field("oidc", &self.oidc.is_some())
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
    /// Time until the response headers are produced (not body streaming).
    pub request_timeout: Duration,
    /// Replaces `request_timeout` for the git POSTs (`git-upload-pack`,
    /// `git-receive-pack`): they read the whole request before answering,
    /// under their own idle / size limits (`git.http.timeout_ms`, 1 h).
    pub git_rpc_timeout: Duration,
    /// Request body limit for extractors and the registry's JSON bodies (a
    /// route's own cap is at most 64 KiB; raw git streams set their own).
    pub body_limit_bytes: usize,
    /// Total time for a JSON body to arrive (`server.body_read_timeout_ms`).
    pub body_read_timeout: Duration,
    /// `Strict-Transport-Security: max-age=…` on every non-git answer
    /// (ADR-0022 §3, `web.hsts`); `None` (the default) sends none, since
    /// TLS may end in front of forge.
    pub hsts: Option<Duration>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            git_rpc_timeout: Duration::from_secs(3600),
            body_limit_bytes: 1024 * 1024,
            body_read_timeout: Duration::from_secs(10),
            hsts: None,
        }
    }
}

/// Actions that have permission-matrix rows but no route yet (API
/// resources: phase 8b).
/// Explicit, so the `authz-coverage` gate can tell "not built yet" from a
/// stale row; an action leaves this list in the commit that routes it.
pub const PLANNED: &[Action] = &[Action::UserCreate, Action::AuditRead, Action::Metrics];

/// Actions decided inside another route's handler, never a route's own
/// (M2 MR4): `SessionCreate` is decided in `GET /-/auth/callback`
/// (ADR-0021 §3), with its own allow and deny rows. `SessionContinue` (M2
/// MR5, ADR-0021 §7) is decided by the session authentication of every
/// cookie-authenticated route when the 5-minute refresh check is due;
/// `GET /api/v1/session` (which the page reads first) stands for them.
/// `WebhookDeliver` (M3 I2, ADR-0016 §8) is decided in `POST
/// …/webhooks` (and a `PATCH` changing the event types) for each class the
/// creator names, and again by the worker for the owner before every
/// delivery (`forge_auth::prove_webhook_delivery`).
pub const DECIDED_IN_ROUTE: &[(Action, &str)] = &[
    (Action::SessionCreate, routes::auth::CALLBACK_PATH),
    (Action::SessionContinue, routes::session::PATH),
    (Action::WebhookDeliver, routes::webhooks::COLLECTION),
];

/// The words whose page is the SPA shell on a project (ADR-0015 "ルートの
/// 系統"). `raw` and `archive` are `RepoRead` content routes
/// (`routes::content`, M2 MR7). `pulls` / `pull` are the merge request
/// screens (MR B0; their data is the `MergeRequestList` / `MergeRequestRead`
/// API). `documents` is the document screens (plan 13 stage 3; their data
/// is the document API, `RepoBrowse`). `pipelines` is the CI screens (plan 14
/// M4; their data is the pipeline API, project readers). `webhooks` is the
/// webhook screens (M3 I2; their data is the webhook API, Maintainer).
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
    RouteWord::Pulls,
    RouteWord::Pull,
    RouteWord::Documents,
    RouteWord::Pipelines,
    RouteWord::Webhooks,
];

/// The words whose page is the SPA shell on a group (no merge request
/// screens: those are a project's).
pub const GROUP_SHELL_WORDS: &[RouteWord] = &[RouteWord::Settings, RouteWord::Members];

/// Every route of the product but the content routes raw / archive (the
/// dispatcher's tests mount their own echoing content route there).
pub(crate) fn registry_without_content() -> Registry<AppState> {
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
            routes::namespaces::PATH,
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
        )
        .responds::<routes::projects::ProjectBody>(Verb::Get, routes::projects::PATH)
        // Lists (M1 phase 8b MR7b): each row decided by authorize().
        .route(
            Verb::Get,
            routes::lists::PROJECTS_PATH,
            Action::ProjectList,
            "List the projects the caller may read (keyset, opaque cursor)",
            routes::lists::projects,
        )
        .responds::<routes::lists::ProjectPage>(Verb::Get, routes::lists::PROJECTS_PATH)
        .queries(
            Verb::Get,
            routes::lists::PROJECTS_PATH,
            routes::lists::PROJECTS_QUERY,
        )
        .route(
            Verb::Get,
            routes::lists::GROUPS_PATH,
            Action::GroupList,
            "List the groups the caller may read (keyset, opaque cursor)",
            routes::lists::groups,
        )
        .responds::<routes::lists::GroupPage>(Verb::Get, routes::lists::GROUPS_PATH)
        .queries(
            Verb::Get,
            routes::lists::GROUPS_PATH,
            routes::lists::GROUPS_QUERY,
        )
        .route(
            Verb::Get,
            routes::users::PATH,
            Action::UserSelfRead,
            "The caller's own account (M1 phase 8b MR6)",
            routes::users::me,
        )
        .responds::<routes::users::UserSelf>(Verb::Get, routes::users::PATH)
        // M2 MR3: the browser session and its CSRF token (sessions only).
        .route(
            Verb::Get,
            routes::session::PATH,
            Action::SessionRead,
            "The caller's own web session and its CSRF token (sessions only, ADR-0021)",
            routes::session::read,
        )
        .responds::<routes::session::SessionBody>(Verb::Get, routes::session::PATH)
        // M2 MR4: the OIDC sign-in (public; admission is SessionCreate inside).
        .route(
            Verb::Get,
            routes::auth::LOGIN_PATH,
            Action::WebLogin,
            "Start the browser sign-in: 302 to the identity provider (ADR-0021)",
            routes::auth::login,
        )
        .route(
            Verb::Get,
            routes::auth::CALLBACK_PATH,
            Action::WebLogin,
            "Finish the browser sign-in: verify, admit (SessionCreate), set the session cookie, 302 to next",
            routes::auth::callback,
        )
        // M2 MR5: the logouts (ADR-0021 §8).
        .route(
            Verb::Post,
            routes::auth::LOGOUT_PATH,
            Action::SessionLogout,
            "End the caller's own web session and 303 to the identity provider's logout (form post, sessions only)",
            routes::auth::logout,
        )
        .route(
            Verb::Post,
            routes::auth::BACKCHANNEL_LOGOUT_PATH,
            Action::OidcBackchannelLogout,
            "OIDC Back-Channel Logout: the identity provider ends sessions with a signed logout token",
            routes::auth::backchannel_logout,
        )
        .responds::<routes::namespaces::Resolution>(Verb::Get, routes::namespaces::PATH)
        .queries(
            Verb::Get,
            routes::namespaces::PATH,
            routes::namespaces::QUERY,
        );
    user_activity_routes(user_merge_request_routes(runner_routes(pipeline_routes(
        doc_lint_routes(binding_routes(act_as_routes(mirror_routes(
            webhook_routes(notification_routes(check_routes(dev_status_routes(
                feed_routes(integration_routes(service_account_routes(account_routes(
                    history_routes(browse_routes(git_routes(shell_routes(
                        merge_request_routes(member_routes(move_routes(hosting_routes(fixed)))),
                    )))),
                )))),
            )))),
        )))),
    ))))
}

/// The merge requests of the caller's own work across projects (plan 15
/// MR 6): one page of a scope and the counts of every scope; each row is
/// decided again on `MergeRequestRead` (forge-review `user_lists`).
fn user_merge_request_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::user_merge_requests as u;
    registry
        .route_with(
            Verb::Get,
            u::LIST,
            Action::UserMergeRequestList,
            "The caller's own open merge requests of one scope (authored, approvable, attention), newest first; only rows whose merge request the caller may read now (an unknown or missing scope is FG-WEB-0006)",
            u::OwnMergeRequests,
            u::list,
        )
        .responds::<u::UserMergeRequestList>(Verb::Get, u::LIST)
        .queries(Verb::Get, u::LIST, u::LIST_QUERY)
        .route_with(
            Verb::Get,
            u::COUNTS,
            Action::UserMergeRequestList,
            "The number of the caller's merge requests in every scope (visible rows only; each at most 100, capped when 100 were found, truncated when the scan bound was reached with candidates left)",
            u::OwnMergeRequests,
            u::counts,
        )
        .responds::<u::UserMergeRequestCounts>(Verb::Get, u::COUNTS)
}

/// The recent events of the projects the caller is involved in (plan 15
/// MR 8); each event is decided again on `ActivityRead` per project and
/// visibility class (forge-review `activity`).
fn user_activity_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::user_activity as a;
    registry
        .route_with(
            Verb::Get,
            a::ACTIVITY,
            Action::ActivityRead,
            "The recent events of the projects the caller is involved in (a role on the project, directly or through a group), newest first; only the shown types (project.created, repository.pushed and merge request events) with their allowed fields, only events the caller may read now (people only; a bad parameter is FG-WEB-0006)",
            a::OwnActivity,
            a::list,
        )
        .responds::<a::UserActivityList>(Verb::Get, a::ACTIVITY)
        .queries(Verb::Get, a::ACTIVITY, a::ACTIVITY_QUERY)
}

/// CI runners (M4 C3, plan 14 §4.2, ADR-0029 §1): the administration (an
/// administrator's web session) and the runner API (a runner's token).
fn runner_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::runners as r;
    registry
        .route_with(
            Verb::Get,
            r::ADMIN,
            Action::RunnerManage,
            "Every CI runner: name, tags, enabled, last contact (never a token; an administrator's web session)",
            r::OnInstance,
            r::list,
        )
        .responds::<r::RunnerList>(Verb::Get, r::ADMIN)
        .route_with_body(
            Verb::Post,
            r::ADMIN,
            Action::RunnerRegister,
            "Register a CI runner ({\"name\", \"tags\"?, \"description\"?}): 201 the runner and its token fgr_..., shown once (an administrator's web session)",
            r::MAX_BODY,
            r::OnInstance,
            r::register,
        )
        .responds_created::<r::RegisteredRunnerBody>(Verb::Post, r::ADMIN)
        .route_with_body(
            Verb::Patch,
            r::ADMIN_ONE,
            Action::RunnerManage,
            "Enable or disable a CI runner, change its tags or description; a disabled runner's token is refused at once; 404 FG-REPO-0005 without such a runner (an administrator's web session)",
            r::MAX_BODY,
            r::OnInstance,
            r::update,
        )
        .responds::<r::RunnerBody>(Verb::Patch, r::ADMIN_ONE)
        .route_with_body(
            Verb::Post,
            r::REQUEST,
            Action::RunnerJobRequest,
            "A runner's long-poll for a job ({\"wait_s\"?} 0..=20): 200 {\"job\": ...} with the job's token fgj_... (shown once), or {\"job\": null} when none came (a runner's token only)",
            r::MAX_BODY,
            r::OnInstance,
            r::request_job,
        )
        .responds::<r::JobRequestAnswer>(Verb::Post, r::REQUEST)
        .route_with_body(
            Verb::Post,
            r::HEARTBEAT,
            Action::RunnerJobHeartbeat,
            "Extend the lease of a job the runner holds ({\"attempt\"}): running with the new lease end, or canceled (stop and complete); 409 FG-CI-0004 when the lease was lost (the holding runner only)",
            r::MAX_BODY,
            r::LEASE_HEARTBEAT,
            r::heartbeat,
        )
        .responds::<r::HeartbeatAnswer>(Verb::Post, r::HEARTBEAT)
        .route_with_body(
            Verb::Post,
            r::LOG,
            Action::RunnerLogAppend,
            "Append a log chunk to a job the runner holds ({\"attempt\", \"seq\", \"text\"}; append only, a resent seq is a no-op); 413 FG-CI-0006 past the job's limit; 409 FG-CI-0004 when the lease was lost (the holding runner only)",
            r::MAX_LOG_BODY,
            r::LEASE_LOG,
            r::append_log,
        )
        .responds::<r::LogAnswer>(Verb::Post, r::LOG)
        .route_with_body(
            Verb::Post,
            r::COMPLETE,
            Action::RunnerJobComplete,
            "Report a job's result ({\"attempt\", \"result\": succeeded|failed, \"exit_code\"?}) or acknowledge its cancel: the token is revoked, waiting jobs start; 409 FG-CI-0004 when the lease was lost (the holding runner only)",
            r::MAX_BODY,
            r::LEASE_COMPLETE,
            r::complete,
        )
        .responds::<r::CompleteAnswer>(Verb::Post, r::COMPLETE)
        // M4 C4 (ADR-0029 §3): the job's fetch of its pinned commit.
        .route_with(
            Verb::Get,
            routes::job_fetch::INFO_REFS,
            Action::RepoRead,
            "Smart HTTP for a CI job (?service=git-upload-pack): the advertisement of refs/pipelines/<pipeline_id>/head only (a live job's token fgj_... of that project and commit; 404 FG-REPO-0005 otherwise, 401 FG-AUTH-0001 once the job ended)",
            routes::job_fetch::COPY_INFO_REFS,
            routes::job_fetch::advertise,
        )
        .route_with(
            Verb::Post,
            routes::job_fetch::UPLOAD_PACK,
            Action::RepoRead,
            "Smart HTTP for a CI job: upload-pack; every want checked reachable from the pipeline's pinned commit (a live job's token fgj_... of that project and commit)",
            routes::job_fetch::COPY_UPLOAD_PACK,
            routes::job_fetch::upload_pack,
        )
}

/// A project's CI pipelines (M4 C2, plan 14 §4.2): read (metadata only),
/// run by hand, cancel and retry.
fn pipeline_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::pipelines as p;
    registry
        .route_with(
            Verb::Get,
            p::LIST,
            Action::PipelineRead,
            "A project's CI pipelines, newest first (ref, before, limit 1..=100, default 20): source, ref, sha, status, error code and problems of an invalid definition, the confidential mark at the commit (project readers; metadata only)",
            p::OF_LIST,
            p::list,
        )
        .responds::<p::PipelinePage>(Verb::Get, p::LIST)
        .queries(Verb::Get, p::LIST, p::LIST_QUERY)
        .route_with_body(
            Verb::Post,
            p::LIST,
            Action::PipelineCreate,
            "Run a pipeline by hand on a branch's tip ({\"ref\": \"main\"}): 201 the pipeline (pending, skipped or invalid; a manual run is never protected, so a job with secrets is invalid); 404 FG-REPO-0023 without the branch or .forge-ci.yml; 503 FG-CI-0002 when the definition could not be read (Developer)",
            p::MAX_BODY,
            p::OF_LIST,
            p::run,
        )
        .responds_created::<p::PipelineBody>(Verb::Post, p::LIST)
        .route_with(
            Verb::Get,
            p::ONE,
            Action::PipelineRead,
            "One CI pipeline with its jobs (name, stage, wave, needs, image, timeout, tags, secret names, status, attempt); 404 FG-REPO-0005 when the project has no such pipeline (project readers; metadata only)",
            p::OF_ONE,
            p::one,
        )
        .responds::<p::PipelineBody>(Verb::Get, p::ONE)
        .route_with(
            Verb::Post,
            p::CANCEL,
            Action::PipelineCancel,
            "Cancel a pending or running pipeline and its unfinished jobs; 409 FG-CI-0003 when it is finished (Developer)",
            p::OF_CANCEL,
            p::cancel,
        )
        .responds::<p::PipelineBody>(Verb::Post, p::CANCEL)
        .route_with(
            Verb::Post,
            p::RETRY,
            Action::PipelineRetry,
            "Retry a failed or cancelled pipeline: its failed and cancelled jobs get a new attempt; 409 FG-CI-0003 in any other state (Developer)",
            p::OF_RETRY,
            p::retry,
        )
        .responds::<p::PipelineBody>(Verb::Post, p::RETRY)
        .route_with(
            Verb::Get,
            p::LOG,
            Action::CiLogRead,
            "One page of a CI job attempt's log (attempt: default the current one; from: the first chunk, 0-based): whole chunks, at most 256 / 1 MiB, secrets masked as [MASKED]; next_from names the next page, complete says the attempt is over; 404 FG-REPO-0005 without such a pipeline, job or attempt; 403 FG-AUTH-0022 to an AI agent audience unless the pipeline's commit and the default branch's tip are both measured open (project readers)",
            p::OF_LOG,
            p::log,
        )
        .responds::<p::JobLogPage>(Verb::Get, p::LOG)
        .queries(Verb::Get, p::LOG, p::LOG_QUERY)
        .route_with(
            Verb::Get,
            p::DIAGNOSIS,
            Action::PipelineDiagnosisRead,
            "A pipeline's deterministic diagnosis (plan 04 P-9): each failed job's fingerprint, error codes, masked excerpt with its log lines, occurrences and known fixes (a merge request whose pipeline later passed); pending while the worker has not fingerprinted it; 404 FG-REPO-0005 without such a pipeline; 403 FG-AUTH-0022 to an AI agent audience unless both marks are open (project readers)",
            p::OF_DIAGNOSIS,
            p::diagnosis,
        )
        .responds::<p::DiagnosisBody>(Verb::Get, p::DIAGNOSIS)
}

/// A project's document lint migration setting (plan 13 Q13, ADR-0026):
/// broken document references lowered to warnings until a date.
fn doc_lint_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::doc_lint_migration as d;
    registry
        .route_with(
            Verb::Get,
            d::ONE,
            Action::ProjectRead,
            "A project's document lint migration: the date until which broken document references (ref_broken) are warnings in forge/doc-lint, whether it is in force, and the database's date and the latest date allowed (project read)",
            d::MigrationOf,
            d::read,
        )
        .responds::<d::DocLintMigrationBody>(Verb::Get, d::ONE)
        .route_with_body(
            Verb::Put,
            d::ONE,
            Action::DocLintMigrationManage,
            "Lower broken document references to warnings until warn_until (YYYY-MM-DD, after today and at most 180 days ahead by the database's date: 400 FG-DOC-0005); errors again from the next day by themselves (people only; Maintainer)",
            d::MAX_BODY,
            d::MigrationOf,
            d::set,
        )
        .responds::<d::DocLintMigrationBody>(Verb::Put, d::ONE)
        .route_with(
            Verb::Delete,
            d::ONE,
            Action::DocLintMigrationManage,
            "End the document lint migration now: broken document references are errors again (people only; Maintainer; 404 without a setting)",
            d::MigrationOf,
            d::clear,
        )
        .responds_no_content(Verb::Delete, d::ONE)
}

/// A kanflow project's repositories (M3 X3, plan 03 §7.1, ADR-0016
/// Amendment X3, ADR-0025 Amendment X3): the search of what the person may
/// bind, the integration's list, the binding writes (acting for a person
/// with an assertion, or a Maintainer's own) and the person's confirmation.
fn binding_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::integration_bindings as b;
    registry
        .route_with(
            Verb::Get,
            b::SEARCH,
            Action::RepositorySearch,
            "The repositories the caller (or the person acted for, no assertion) may bind, whose path or name contains q: each decided with authorize(), nothing said about the rest (read_api)",
            b::SearchOf,
            b::search,
        )
        .responds::<b::BindableRepositoryList>(Verb::Get, b::SEARCH)
        .queries(Verb::Get, b::SEARCH, b::SEARCH_QUERY)
        .route_with(
            Verb::Get,
            b::BINDINGS,
            Action::IntegrationBindingList,
            "The repositories bound to the integration, to one kanflow project when kanflow_project_id is given (the integration's own service account; read_api)",
            b::ListOf,
            b::list,
        )
        .responds::<b::RepositoryBindingList>(Verb::Get, b::BINDINGS)
        .queries(Verb::Get, b::BINDINGS, b::BINDINGS_QUERY)
        .route_with_body(
            Verb::Post,
            b::BINDINGS,
            Action::IntegrationBindingManage,
            "Bind a repository to a kanflow project of the integration: a repository with code to one kanflow project only, a shared spec repository (resolves_work_items false) to several (acting for a Maintainer with a binding_set assertion, or a Maintainer's own; api)",
            b::MAX_BODY,
            b::NewBindingOf,
            b::bind,
        )
        .responds::<b::RepositoryBindingOutcome>(Verb::Post, b::BINDINGS)
        .route_with_body(
            Verb::Patch,
            b::BINDING,
            Action::IntegrationBindingManage,
            "Change a binding's setting: resolves_work_items from false to true when it is the repository's only binding (acting for a Maintainer with a binding_set assertion, or a Maintainer's own; api)",
            b::MAX_BODY,
            b::BindingOf,
            b::change,
        )
        .responds::<b::RepositoryBindingOutcome>(Verb::Patch, b::BINDING)
        .route_with(
            Verb::Delete,
            b::BINDING,
            Action::IntegrationBindingRemove,
            "Remove a binding: its work item links end, the membership it granted moves to the integration's next binding of the repository or is removed (acting for a Maintainer with a binding_remove assertion, or a Maintainer's own; api)",
            b::BindingOf,
            b::remove,
        )
        .responds_no_content(Verb::Delete, b::BINDING)
        .route_with_body(
            Verb::Post,
            b::CONFIRM,
            Action::ActAsAssertionIssue,
            "The person confirms binding_set (with the setting) or binding_remove of a repository for one kanflow project: a pending assertion for the integration to claim (a person's web session, Maintainer; CSRF; 30 per 10 minutes)",
            b::MAX_BODY,
            b::ConfirmOf,
            b::confirm,
        )
        .responds_created::<b::BindingAssertionIssued>(Verb::Post, b::CONFIRM)
        .route_with(
            Verb::Get,
            b::CONFIRMATION,
            Action::ActAsConfirmView,
            "What the binding confirmation screen shows: the repository, the kanflow project id the integration named, the setting, whether it is bound now, the receiving integration (a person's web session; Maintainer, the intent's action allowed for them)",
            b::BindingConfirmationOf,
            b::confirmation,
        )
        .responds::<b::BindingConfirmationBody>(Verb::Get, b::CONFIRMATION)
        .queries(Verb::Get, b::CONFIRMATION, b::CONFIRMATION_QUERY)
}

/// Every route of the product. The `gen-authz` binary and the tests read the
/// same list.
pub fn registry() -> Registry<AppState> {
    content_routes(registry_without_content())
}

/// The product's routes without raw / archive, for integration tests that
/// mount an echoing content route there (`tests/namespace_tombstone_routes.rs`).
/// Not compiled into the server.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn registry_without_content_routes() -> Registry<AppState> {
    registry_without_content()
}

/// Tokens, sessions, the user search and the user administration (M2 MR6,
/// plan 10 §5, ADR-0021 §17).
fn account_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::{directory as d, tokens as t, user_sessions as s};
    registry
        .route_with(
            Verb::Get,
            t::COLLECTION,
            Action::TokenList,
            "The caller's personal access tokens (never a secret)",
            t::OwnTokens,
            t::list,
        )
        .responds::<t::TokenList>(Verb::Get, t::COLLECTION)
        .route_with_body(
            Verb::Post,
            t::COLLECTION,
            Action::TokenCreate,
            "Create a personal access token for the caller (never with a token; the secret is shown once)",
            t::MAX_BODY,
            t::NewToken,
            t::create,
        )
        .responds_created::<t::CreatedToken>(Verb::Post, t::COLLECTION)
        .route_with(
            Verb::Delete,
            t::ONE,
            Action::TokenRevoke,
            "Revoke one of the caller's tokens",
            t::TokenToRevoke,
            t::revoke,
        )
        .responds_no_content(Verb::Delete, t::ONE)
        .route_with(
            Verb::Get,
            s::OWN,
            Action::SessionList,
            "The caller's active web sessions (sessions only)",
            s::OwnSessions,
            s::list,
        )
        .responds::<s::SessionItems>(Verb::Get, s::OWN)
        .route_with(
            Verb::Delete,
            s::OWN_ONE,
            Action::SessionRevoke,
            "End one of the caller's web sessions (sessions only)",
            s::OwnSessionToRevoke,
            s::revoke,
        )
        .responds_no_content(Verb::Delete, s::OWN_ONE)
        .route_with(
            Verb::Get,
            s::ADMIN,
            Action::AdminSessionList,
            "A user's active web sessions (administrators)",
            s::UserSessions,
            s::list,
        )
        .responds::<s::SessionItems>(Verb::Get, s::ADMIN)
        .route_with(
            Verb::Delete,
            s::ADMIN,
            Action::AdminSessionRevoke,
            "End every web session of a user (administrators, never an agent)",
            s::UserSessions,
            s::revoke_all,
        )
        .responds::<s::SessionsRevoked>(Verb::Delete, s::ADMIN)
        .route(
            Verb::Get,
            d::SEARCH,
            Action::UserSearch,
            "Find active users by username prefix or display name (names only, at most 20)",
            d::search,
        )
        .responds::<d::UserNames>(Verb::Get, d::SEARCH)
        .queries(Verb::Get, d::SEARCH, d::SEARCH_QUERY)
        .route(
            Verb::Get,
            d::ADMIN_USERS,
            Action::UserAdminList,
            "Every user with its state (administrators; keyset, opaque cursor)",
            d::admin_users,
        )
        .responds::<d::AdminUserPage>(Verb::Get, d::ADMIN_USERS)
        .queries(Verb::Get, d::ADMIN_USERS, d::ADMIN_QUERY)
        .route_with(
            Verb::Post,
            d::DISABLE,
            Action::UserDisable,
            "Disable a user (administrators, never an agent; the administration guard always holds)",
            d::UserToDisable,
            d::disable,
        )
        .responds::<d::Disabled>(Verb::Post, d::DISABLE)
}

/// Service accounts and their tokens (M3 S0, plan 11 §5 S0, ADR-0019
/// Addendum S0): administrators; create and issue in a web session only.
fn service_account_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::service_accounts as sa;
    registry
        .route(
            Verb::Get,
            sa::COLLECTION,
            Action::ServiceAccountList,
            "Every service account with its owner and token summary (administrators)",
            sa::list,
        )
        .responds::<sa::ServiceAccountList>(Verb::Get, sa::COLLECTION)
        .route_with_body(
            Verb::Post,
            sa::COLLECTION,
            Action::ServiceAccountCreate,
            "Create a service account owned by a person (an administrator's web session only)",
            sa::MAX_BODY,
            sa::NewAccount,
            sa::create,
        )
        .responds_created::<sa::ServiceAccountBody>(Verb::Post, sa::COLLECTION)
        .route_with(
            Verb::Get,
            sa::ONE,
            Action::ServiceAccountRead,
            "One service account (administrators; another kind of user is not found)",
            sa::ACCOUNT,
            sa::read,
        )
        .responds::<sa::ServiceAccountBody>(Verb::Get, sa::ONE)
        .route_with_body(
            Verb::Patch,
            sa::ONE,
            Action::ServiceAccountUpdate,
            "Change a service account's display name, purpose or owner (administrators, people only)",
            sa::MAX_BODY,
            sa::ACCOUNT,
            sa::update,
        )
        .responds::<sa::UpdatedServiceAccount>(Verb::Patch, sa::ONE)
        .route_with(
            Verb::Get,
            sa::TOKENS,
            Action::ServiceAccountTokenList,
            "A service account's tokens with their issuer (administrators; never a secret)",
            sa::ACCOUNT_TOKENS,
            sa::tokens,
        )
        .responds::<sa::ServiceAccountTokenList>(Verb::Get, sa::TOKENS)
        .route_with_body(
            Verb::Post,
            sa::TOKENS,
            Action::ServiceAccountTokenIssue,
            "Issue a token for an active service account (an administrator's web session only; never admin; the secret is shown once)",
            sa::MAX_BODY,
            sa::TokenRequest,
            sa::issue,
        )
        .responds_created::<sa::IssuedServiceAccountToken>(Verb::Post, sa::TOKENS)
        .route_with(
            Verb::Delete,
            sa::TOKEN,
            Action::ServiceAccountTokenRevoke,
            "Revoke a service account's token (administrators, an admin-scoped token included)",
            sa::TokenToRevoke,
            sa::revoke,
        )
        .responds_no_content(Verb::Delete, sa::TOKEN)
}

/// The caller's own notification settings (M3 N1, ADR-0016 Amendment N1).
fn notification_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::in_app_notifications as i;
    use routes::notifications as n;
    registry
        .route_with(
            Verb::Get,
            n::PREFERENCES,
            Action::NotificationPreferencesRead,
            "The caller's own notification settings: mail on or off, the language of the mail, the kinds muted (nobody else's, administrators included)",
            n::OwnPreferences,
            n::read,
        )
        .responds::<n::PreferencesBody>(Verb::Get, n::PREFERENCES)
        .route_with_body(
            Verb::Put,
            n::PREFERENCES,
            Action::NotificationPreferencesWrite,
            "Replace the caller's own notification settings (an unknown language or kind is FG-WEB-0006)",
            n::MAX_BODY,
            n::OwnPreferencesChange,
            n::replace,
        )
        .responds::<n::PreferencesBody>(Verb::Put, n::PREFERENCES)
        .route_with(
            Verb::Get,
            i::LIST,
            Action::NotificationInboxRead,
            "The caller's own in-app notifications, newest first; only items whose merge request the caller may read now (decided again on every read)",
            i::OwnInbox,
            i::list,
        )
        .responds::<i::InAppNotificationList>(Verb::Get, i::LIST)
        .queries(Verb::Get, i::LIST, i::LIST_QUERY)
        .route_with(
            Verb::Get,
            i::UNREAD_COUNT,
            Action::NotificationInboxRead,
            "The number of the caller's unread in-app notifications they may still see (visible items only; at most 100, capped when 100 were found)",
            i::OwnInbox,
            i::unread_count,
        )
        .responds::<i::UnreadCountBody>(Verb::Get, i::UNREAD_COUNT)
        .route_with(
            Verb::Post,
            i::READ_ONE,
            Action::NotificationInboxUpdate,
            "Mark one of the caller's in-app notifications read (another person's or an absent one is not found)",
            i::ItemToMark,
            i::mark_read,
        )
        .responds::<i::MarkedRead>(Verb::Post, i::READ_ONE)
        .route_with_body(
            Verb::Post,
            i::READ_ALL,
            Action::NotificationInboxUpdate,
            "Mark every unread in-app notification of the caller read, up to up_to when given",
            i::MAX_BODY,
            i::OwnInbox,
            i::mark_all_read,
        )
        .responds_no_content(Verb::Post, i::READ_ALL)
}

/// The Checks API (M3 C1, ADR-0024 §10, docs/api/checks.md): a commit's
/// check runs, and the administrators' grants of a check namespace to a
/// service account.
fn check_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::checks as c;
    registry
        .route_with_body(
            Verb::Post,
            c::CHECKS,
            Action::CheckWrite,
            "Report a check run on a commit; the same name again updates it (ci/, kanflow/: a service account holding the namespace, Developer member; forge/ and ext/ are refused)",
            c::MAX_BODY,
            c::ReportOf,
            c::report,
        )
        .responds_created::<c::CheckRunBody>(Verb::Post, c::CHECKS)
        .route_with(
            Verb::Get,
            c::CHECKS,
            Action::CheckRead,
            "List a commit's check runs by name (as the repository browse API; a commit no branch or tag reaches is not found)",
            c::ChecksOf,
            c::list,
        )
        .responds::<c::CheckRunList>(Verb::Get, c::CHECKS)
        .route_with(
            Verb::Get,
            c::GRANTS,
            Action::ServiceAccountRead,
            "The check namespace grants of a service account, by project (administrators)",
            c::GrantsOf,
            c::grants,
        )
        .responds::<c::CheckGrants>(Verb::Get, c::GRANTS)
        .route_with(
            Verb::Put,
            c::GRANT,
            Action::CheckNamespaceGrantManage,
            "Let a service account report a check namespace on one project (ci, kanflow; administrators, people only)",
            c::GrantOf,
            c::grant,
        )
        .responds::<c::CheckGrantChange>(Verb::Put, c::GRANT)
        .route_with(
            Verb::Delete,
            c::GRANT,
            Action::CheckNamespaceGrantManage,
            "Stop a service account reporting a check namespace on one project (administrators, people only)",
            c::GrantOf,
            c::revoke,
        )
        .responds::<c::CheckGrantChange>(Verb::Delete, c::GRANT)
}

/// Integrations and project bindings (M3 I1a, plan 11 §5 I1, ADR-0016
/// Amendment I1a): the administration's routes and a project's binding.
fn integration_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::integrations as i;
    registry
        .route(
            Verb::Get,
            i::COLLECTION,
            Action::IntegrationRead,
            "Every integration with its service account and binding count (administrators, people only)",
            i::list,
        )
        .responds::<i::IntegrationList>(Verb::Get, i::COLLECTION)
        .route_with_body(
            Verb::Post,
            i::COLLECTION,
            Action::IntegrationManage,
            "Create a kanflow integration acting as a service account (an administrator's web session only)",
            i::MAX_BODY,
            i::NewIntegration,
            i::create,
        )
        .responds_created::<i::IntegrationBody>(Verb::Post, i::COLLECTION)
        .route_with(
            Verb::Get,
            i::ONE,
            Action::IntegrationRead,
            "One integration (administrators, people only)",
            i::INTEGRATION,
            i::read,
        )
        .responds::<i::IntegrationBody>(Verb::Get, i::ONE)
        .route_with_body(
            Verb::Patch,
            i::ONE,
            Action::IntegrationManage,
            "Enable or disable an integration or replace its event type allowlist (an administrator's web session only)",
            i::MAX_BODY,
            i::INTEGRATION,
            i::update,
        )
        .responds::<i::UpdatedIntegration>(Verb::Patch, i::ONE)
        .route_with(
            Verb::Get,
            i::BINDINGS,
            Action::IntegrationRead,
            "The projects bound to an integration (administrators, people only)",
            i::INTEGRATION_BINDINGS,
            i::bindings,
        )
        .responds::<i::IntegrationBindingList>(Verb::Get, i::BINDINGS)
        .route_with(
            Verb::Get,
            i::PROJECT_BINDINGS,
            Action::ProjectRead,
            "A project's integration binding, none or one (as reading the project)",
            i::BindingsOf,
            i::project_bindings,
        )
        .responds::<i::IntegrationBindingList>(Verb::Get, i::PROJECT_BINDINGS)
        .route_with_body(
            Verb::Put,
            i::PROJECT_BINDING,
            Action::IntegrationBindingManage,
            "Bind a project to an external (kanflow) project of an integration; its service account becomes a Reporter (people only; Maintainer)",
            i::MAX_BODY,
            i::BindingOf,
            i::bind,
        )
        .responds::<i::BoundProject>(Verb::Put, i::PROJECT_BINDING)
        .route_with(
            Verb::Delete,
            i::PROJECT_BINDING,
            Action::IntegrationBindingManage,
            "Unbind a project from an integration; the membership the binding granted is removed (people only; Maintainer)",
            i::BindingOf,
            i::unbind,
        )
        .responds_no_content(Verb::Delete, i::PROJECT_BINDING)
}

/// Project webhooks (M3 I2, ADR-0016 §8, docs/api/webhooks.md): a
/// project's Maintainers read them; managing and redelivering are people's.
fn webhook_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::webhooks as h;
    registry
        .route_with(
            Verb::Get,
            h::COLLECTION,
            Action::WebhookRead,
            "A project's webhooks, never a secret (Maintainer)",
            h::WebhooksOf,
            h::list,
        )
        .responds::<h::WebhookList>(Verb::Get, h::COLLECTION)
        .route_with(
            Verb::Get,
            h::EVENT_TYPES,
            Action::WebhookRead,
            "The event types a webhook may name, each with its class, and per class whether the caller may subscribe to it (Maintainer)",
            h::EventTypesOf,
            h::event_types,
        )
        .responds::<h::EventTypeList>(Verb::Get, h::EVENT_TYPES)
        .route_with_body(
            Verb::Post,
            h::COLLECTION,
            Action::WebhookManage,
            "Create a webhook owned by the caller; its URL must be on the operator's allowlist (400 FG-INT-0010); the secret is shown once (people only; Maintainer; at most 20)",
            h::MAX_BODY,
            h::WebhooksOf,
            h::create,
        )
        .responds_created::<h::CreatedWebhook>(Verb::Post, h::COLLECTION)
        .route_with(
            Verb::Get,
            h::ONE,
            Action::WebhookRead,
            "One webhook of the project (Maintainer)",
            h::WEBHOOK,
            h::read,
        )
        .responds::<h::WebhookBody>(Verb::Get, h::ONE)
        .route_with_body(
            Verb::Patch,
            h::ONE,
            Action::WebhookManage,
            "Change a webhook's URL, event types or enabled flag; enabling clears its failure count (people only; Maintainer)",
            h::MAX_BODY,
            h::WEBHOOK,
            h::update,
        )
        .responds::<h::UpdatedWebhook>(Verb::Patch, h::ONE)
        .route_with(
            Verb::Delete,
            h::ONE,
            Action::WebhookManage,
            "Delete a webhook and its delivery history (people only; Maintainer)",
            h::WEBHOOK,
            h::delete,
        )
        .responds_no_content(Verb::Delete, h::ONE)
        .route_with(
            Verb::Post,
            h::SECRET,
            Action::WebhookManage,
            "Replace a webhook's secret, shown once; the previous one still signs for 24 hours (people only; Maintainer)",
            h::WEBHOOK_SECRET,
            h::rotate,
        )
        .responds::<h::RotatedWebhookSecret>(Verb::Post, h::SECRET)
        .route_with(
            Verb::Get,
            h::DELIVERIES,
            Action::WebhookDeliveryRead,
            "A webhook's delivery history, newest first: state, attempts, the last status and reason (never a body; Maintainer)",
            h::WEBHOOK_DELIVERIES,
            h::deliveries,
        )
        .responds::<h::DeliveryList>(Verb::Get, h::DELIVERIES)
        .queries(Verb::Get, h::DELIVERIES, h::DELIVERIES_QUERY)
        .route_with(
            Verb::Post,
            h::REDELIVER,
            Action::WebhookRedeliver,
            "Deliver one delivery again with the same delivery id (people only; Maintainer; 429 while pending or within 60 s of its last attempt)",
            h::RedeliveryOf,
            h::redeliver,
        )
        .responds_accepted::<h::RedeliveryQueued>(Verb::Post, h::REDELIVER)
}

/// A project's push mirror to GitLab (M3 O2, ADR-0024 Amendment O2,
/// runbook gitlab-mirror): administrators only, people only.
fn mirror_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::push_mirror as m;
    registry
        .route_with(
            Verb::Get,
            m::ONE,
            Action::PushMirrorRead,
            "A project's push mirror to GitLab and its progress: destination, credential name (never its content), the waiting and mirrored pushes, the last failure (administrators; a token needs admin)",
            m::MIRROR,
            m::read,
        )
        .responds::<m::PushMirrorBody>(Verb::Get, m::ONE)
        .route_with_body(
            Verb::Put,
            m::ONE,
            Action::PushMirrorManage,
            "Set a project's push mirror (201 created, 200 changed): an https URL on mirror.allowed_hosts and a usable credential file of mirror.credentials_dir (400 FG-INT-0021); a new or re-enabled mirror is pushed at once (administrators, people only)",
            m::MAX_BODY,
            m::MIRROR,
            m::set,
        )
        .responds::<m::PushMirrorBody>(Verb::Put, m::ONE)
        .route_with(
            Verb::Delete,
            m::ONE,
            Action::PushMirrorManage,
            "Remove a project's push mirror and its progress; GitLab keeps what it has (administrators, people only)",
            m::MIRROR,
            m::delete,
        )
        .responds_no_content(Verb::Delete, m::ONE)
        .route_with(
            Verb::Post,
            m::SYNC,
            Action::PushMirrorManage,
            "Queue one push of every branch and tag to the mirror now (administrators, people only; 404 without an enabled mirror)",
            m::MIRROR_SYNC,
            m::sync,
        )
        .responds_accepted::<m::PushMirrorSyncQueued>(Verb::Post, m::SYNC)
}

/// The integrations' event feed (M3 I1c, ADR-0016 §7, plan 02 §11.2,
/// docs/api/integration-feed.md): only the integration's own service
/// account reads it.
fn feed_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::integration_feed as f;
    registry
        .route_with(
            Verb::Get,
            f::EVENTS,
            Action::IntegrationEventRead,
            "The integration's event feed after a cursor: every integration-class event of its bound projects in order, each with its work items (the integration's own service account; read_api; 410 FG-INT-0007 with reason when the cursor cannot continue)",
            f::FEED_EVENTS,
            f::events,
        )
        .responds::<f::FeedPageBody>(Verb::Get, f::EVENTS)
        .queries(Verb::Get, f::EVENTS, f::EVENTS_QUERY)
        .route_with(
            Verb::Get,
            f::CURSOR,
            Action::IntegrationEventCursorRead,
            "The feed's head as a cursor (a resync takes it first, then the development status, then reads from it)",
            f::FEED_CURSOR,
            f::cursor,
        )
        .responds::<f::FeedCursorBody>(Verb::Get, f::CURSOR)
}

/// The development status of an integration's work items (M3 I1d, plan
/// 02 §5.2, §11.2, docs/api/dev-status.md): decided like the feed (only
/// the integration's own service account), on the feed's visible set.
fn dev_status_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::integration_dev_status as d;
    registry
        .route_with(
            Verb::Get,
            d::WORK_ITEM,
            Action::IntegrationDevStatusRead,
            "One work item's development status: the merge requests linked to PBI-<n> across the integration's bound projects, open, merged and closed (state=all by default), with approvals, the head's checks and as_of_seq / feed_cursor (the integration's own service account; read_api; FG-REPO-0005 for a reference of no bound kanflow project)",
            d::DEV_STATUS_WORK_ITEM,
            d::work_item,
        )
        .responds::<d::DevStatusBody>(Verb::Get, d::WORK_ITEM)
        .queries(Verb::Get, d::WORK_ITEM, d::WORK_ITEM_QUERY)
        .route_with(
            Verb::Get,
            d::ALL,
            Action::IntegrationDevStatusRead,
            "Every work item's development status, page by page (the resync after a 410: take GET …/events/cursor, read these pages, then the feed from that cursor or feed_cursor; FG-INT-0007 410 when the walk cannot continue)",
            d::DEV_STATUS_ALL,
            d::all,
        )
        .responds::<d::DevStatusPageBody>(Verb::Get, d::ALL)
        .queries(Verb::Get, d::ALL, d::ALL_QUERY)
}

/// Acting for a person (M3 S2, ADR-0025 §5): the confirmation's facts and
/// the person's press (a web session), and the integration's claim.
fn act_as_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::act_as as a;
    registry
        .route_with(
            Verb::Get,
            a::CONFIRMATION,
            Action::ActAsConfirmView,
            "What the act-as confirmation screen shows: the merge request, its head and the target's tip, whether sha is the head, the receiving integration (a person's web session; the intent's action allowed for them)",
            a::ConfirmationOf,
            a::confirmation,
        )
        .responds::<a::ConfirmationBody>(Verb::Get, a::CONFIRMATION)
        .queries(Verb::Get, a::CONFIRMATION, a::CONFIRMATION_QUERY)
        .route_with_body(
            Verb::Post,
            a::ISSUE,
            Action::ActAsAssertionIssue,
            "The person confirms an act-as approve or unapprove: a pending assertion bound to the merge request, its head and target, for the integration to claim (a person's web session; CSRF; 30 per 10 minutes)",
            a::MAX_BODY,
            a::ToIssue,
            a::issue,
        )
        .responds_created::<a::IssuedBody>(Verb::Post, a::ISSUE)
        .route_with_body(
            Verb::Post,
            a::CLAIM,
            Action::ActAsAssertionClaim,
            "The integration claims a confirmed assertion by its state: the assertion is created and returned once (the integration's own service account; act_as; 120 a minute)",
            a::MAX_BODY,
            a::ClaimOf,
            a::claim,
        )
        .responds::<a::ClaimBody>(Verb::Post, a::CLAIM)
}

/// The repository browse API (`RepoBrowse`, `read_api`; M2 MR7, ADR-0015
/// Amendment M2): git only through forge-git's browse API, after
/// `authorize()`.
fn browse_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::browse as b;
    registry
        .route_with(
            Verb::Get,
            b::REFS,
            Action::RepoBrowse,
            "List a repository's branches and tags (Reporter; internal: every signed-in user)",
            b::REFS_OF,
            b::refs,
        )
        .responds::<b::RefsBody>(Verb::Get, b::REFS)
        .queries(Verb::Get, b::REFS, b::REFS_QUERY)
        .route_with(
            Verb::Get,
            b::RESOLVE,
            Action::RepoBrowse,
            "Split <ref>/<path> into a revision and a path (commit id, longest branch, longest tag)",
            b::RESOLVE_OF,
            b::resolve,
        )
        .responds::<b::ResolveBody>(Verb::Get, b::RESOLVE)
        .queries(Verb::Get, b::RESOLVE, b::RESOLVE_QUERY)
        .route_with(
            Verb::Get,
            b::TREE,
            Action::RepoBrowse,
            "One tree level of a revision (capped; truncated past the cap)",
            b::TREE_OF,
            b::tree,
        )
        .responds::<b::TreeBody>(Verb::Get, b::TREE)
        .queries(Verb::Get, b::TREE, b::TREE_QUERY)
        .route_with(
            Verb::Get,
            b::BLOB,
            Action::RepoBrowse,
            "One file's head as text (at most 1 MiB), or binary without content",
            b::BLOB_OF,
            b::blob,
        )
        .responds::<b::BlobBody>(Verb::Get, b::BLOB)
        .queries(Verb::Get, b::BLOB, b::TREE_QUERY)
        // Plan 13 stage 3: the document view as JSON (the blob API's rule);
        // a confidential document is withheld from an AI agent (plan 06 D-5).
        .route_with(
            Verb::Get,
            routes::documents::LIST,
            Action::RepoBrowse,
            "A revision's documents in the plan 13 format: id, title, type, path, classification (paged; an agent's list leaves confidential ones out)",
            routes::documents::LIST_OF,
            routes::documents::list,
        )
        .responds::<routes::documents::DocumentListBody>(Verb::Get, routes::documents::LIST)
        .queries(
            Verb::Get,
            routes::documents::LIST,
            routes::documents::LIST_QUERY,
        )
        .route_with(
            Verb::Get,
            routes::documents::ONE,
            Action::RepoBrowse,
            "One document's view: front matter, sections of text and blocks, references resolved (a more confidential target: its id only)",
            routes::documents::DocumentOf,
            routes::documents::one,
        )
        .responds::<routes::documents::DocumentBody>(Verb::Get, routes::documents::ONE)
        .queries(
            Verb::Get,
            routes::documents::ONE,
            routes::documents::ONE_QUERY,
        )
        // Plan 13 §8.2: who refers to a document, and one block alone
        // (computed on read; an agent is given no confidential one).
        .route_with(
            Verb::Get,
            routes::documents::REFERENCED_BY,
            Action::RepoBrowse,
            "The documents that refer to one document (or one of its blocks), by id, paged (a more confidential referrer: its id only; an agent's answer leaves confidential ones out uncounted)",
            routes::documents::DocumentOf,
            routes::documents::referenced_by,
        )
        .responds::<routes::documents::ReferrersBody>(
            Verb::Get,
            routes::documents::REFERENCED_BY,
        )
        .queries(
            Verb::Get,
            routes::documents::REFERENCED_BY,
            routes::documents::REFERENCED_BY_QUERY,
        )
        .route_with(
            Verb::Get,
            routes::documents::BLOCK,
            Action::RepoBrowse,
            "One block of a document alone, with the references inside it resolved (a missing block answers as a missing document)",
            routes::documents::DocumentOf,
            routes::documents::block,
        )
        .responds::<routes::documents::DocumentBlockAloneBody>(
            Verb::Get,
            routes::documents::BLOCK,
        )
        .queries(
            Verb::Get,
            routes::documents::BLOCK,
            routes::documents::BLOCK_QUERY,
        )
        // M3 X1: `.forge/repo.yml` and its spec references; every referenced
        // project decided again with the reader's own principal.
        .route_with(
            Verb::Get,
            routes::spec_refs::SPEC_REFS,
            Action::SpecRefRead,
            "A revision's .forge/repo.yml and its pinned spec references, each resolved for the reader (unreadable ones are unavailable)",
            routes::spec_refs::SPEC_REFS_OF,
            routes::spec_refs::spec_refs,
        )
        .responds::<routes::spec_refs::SpecRefsBody>(Verb::Get, routes::spec_refs::SPEC_REFS)
        .queries(
            Verb::Get,
            routes::spec_refs::SPEC_REFS,
            routes::spec_refs::SPEC_REFS_QUERY,
        )
        // M3 X4: the spec API (plan 03 §7.2); a trace's references decided
        // again with the reader's own principal.
        .route_with(
            Verb::Get,
            routes::specs::REQUIREMENTS,
            Action::SpecRead,
            "A revision's own specification: its documents, requirements (searchable) and document problems",
            routes::specs::REQUIREMENTS_OF,
            routes::specs::requirements,
        )
        .responds::<routes::specs::SpecRequirementsBody>(Verb::Get, routes::specs::REQUIREMENTS)
        .queries(
            Verb::Get,
            routes::specs::REQUIREMENTS,
            routes::specs::REQUIREMENTS_QUERY,
        )
        .route_with(
            Verb::Get,
            routes::specs::VERSIONS,
            Action::SpecRead,
            "The specification's versions (version tags), newest first",
            routes::specs::VERSIONS_OF,
            routes::specs::versions,
        )
        .responds::<routes::specs::SpecVersionsBody>(Verb::Get, routes::specs::VERSIONS)
        .route_with(
            Verb::Get,
            routes::specs::DIFF,
            Action::SpecRead,
            "The requirements added, changed, removed or deprecated between two revisions",
            routes::specs::DIFF_OF,
            routes::specs::diff_route,
        )
        .responds::<routes::specs::SpecDiffBody>(Verb::Get, routes::specs::DIFF)
        .queries(Verb::Get, routes::specs::DIFF, routes::specs::DIFF_QUERY)
        .route_with(
            Verb::Get,
            routes::specs::TRACE,
            Action::SpecRead,
            "Each requirement (own and pinned references, followed for the reader) with the @req tests naming it",
            routes::specs::TRACE_OF,
            routes::specs::trace,
        )
        .responds::<routes::specs::SpecTraceBody>(Verb::Get, routes::specs::TRACE)
        .queries(Verb::Get, routes::specs::TRACE, routes::specs::TRACE_QUERY)
        // M3 X5: the context API (plan 03 §6); what is marked confidential
        // is withheld from an AI agent (plan 06 D-5).
        .route_with(
            Verb::Get,
            routes::context::WORK_ITEM,
            Action::ContextRead,
            "A kanflow work item's context: the project's linked merge requests and the @req tests of its acceptance criteria, each with its source",
            routes::context::WORK_ITEM_OF,
            routes::context::work_item,
        )
        .responds::<routes::context::ContextWorkItemBody>(Verb::Get, routes::context::WORK_ITEM)
        .queries(
            Verb::Get,
            routes::context::WORK_ITEM,
            routes::context::WORK_ITEM_QUERY,
        )
        .route_with(
            Verb::Get,
            routes::context::REQUIREMENT,
            Action::ContextRead,
            "A requirement's context: its text and document, the @req tests naming it and its state in the newest versions, each with its source",
            routes::context::REQUIREMENT_OF,
            routes::context::requirement,
        )
        .responds::<routes::context::ContextRequirementBody>(
            Verb::Get,
            routes::context::REQUIREMENT,
        )
        .queries(
            Verb::Get,
            routes::context::REQUIREMENT,
            routes::context::REQUIREMENT_QUERY,
        )
}

/// The history half of the browse API (`RepoBrowse`, `read_api`; M2 MR8):
/// commits, one commit with its diff, compare and blame.
fn history_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::history as h;
    registry
        .route_with(
            Verb::Get,
            h::COMMITS,
            Action::RepoBrowse,
            "A page of a revision's history, newest first (keyset cursor; optionally one path)",
            h::COMMITS_OF,
            h::commits,
        )
        .responds::<h::CommitsBody>(Verb::Get, h::COMMITS)
        .queries(Verb::Get, h::COMMITS, h::COMMITS_QUERY)
        .route_with(
            Verb::Get,
            h::COMMIT,
            Action::RepoBrowse,
            "One commit: message and the diff against its first parent (capped; truncated past the caps)",
            h::CommitOf,
            h::commit,
        )
        .responds::<h::CommitDetailBody>(Verb::Get, h::COMMIT)
        .route_with(
            Verb::Get,
            h::COMPARE,
            Action::RepoBrowse,
            "Compare two revisions: the commits to has and from does not, and the tree diff (capped)",
            h::COMPARE_OF,
            h::compare,
        )
        .responds::<h::CompareBody>(Verb::Get, h::COMPARE)
        .queries(Verb::Get, h::COMPARE, h::COMPARE_QUERY)
        .route_with(
            Verb::Get,
            h::BLAME,
            Action::RepoBrowse,
            "Who last changed each line of a file (hunks; capped at 10 000 lines; binary files have none)",
            h::BLAME_OF,
            h::blame,
        )
        .responds::<h::BlameBody>(Verb::Get, h::BLAME)
        .queries(Verb::Get, h::BLAME, h::BLAME_QUERY)
}

/// The content routes raw / archive (`RepoRead`, `read_repository`; M2
/// MR7, ADR-0015 "応答の安全"), `GET` and `HEAD`.
fn content_routes(mut registry: Registry<AppState>) -> Registry<AppState> {
    for verb in [Verb::Get, Verb::Head] {
        registry = registry
            .scoped(
                RouteScope::Project,
                verb,
                ScopedRoute::Word(RouteWord::Raw),
                None,
                Action::RepoRead,
                "A file as it is (typed by content; SVG / HTML / PDF only as downloads)",
                routes::content::raw,
            )
            .scoped(
                RouteScope::Project,
                verb,
                ScopedRoute::Word(RouteWord::Archive),
                None,
                Action::RepoRead,
                "An archive of a revision (zip or tar.gz, streamed)",
                routes::content::archive,
            );
    }
    registry
}

/// Members and protected refs (M1 phase 8b MR8): the lists decided on the
/// project or group, every change proved on the member's direct role or
/// the rule's level as loaded, and decided again inside its transaction
/// (forge-hosting) on what it locks.
fn member_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::{members as m, protected_ref_merge_rules as pmr, protected_refs as pr};
    registry
        .route_with(
            Verb::Get,
            m::PROJECT_MEMBERS,
            Action::MemberList,
            "List a project's direct members (Reporter; internal: every signed-in user)",
            m::ProjectMembersOf,
            m::list_project,
        )
        .responds::<m::MemberPage>(Verb::Get, m::PROJECT_MEMBERS)
        .queries(Verb::Get, m::PROJECT_MEMBERS, m::QUERY)
        .route_with_body(
            Verb::Post,
            m::PROJECT_MEMBERS,
            Action::MemberManage,
            "Add a direct project member (Maintainer; at most the caller's role)",
            m::MAX_BODY,
            m::NewProjectMember,
            m::add_project,
        )
        .responds_created::<m::MemberBody>(Verb::Post, m::PROJECT_MEMBERS)
        .route_with_body(
            Verb::Patch,
            m::PROJECT_MEMBER,
            Action::MemberManage,
            "Change a direct project member's role (Maintainer; never above the caller's role)",
            m::MAX_BODY,
            m::ProjectMemberToChange,
            m::change_project,
        )
        .responds::<m::MemberBody>(Verb::Patch, m::PROJECT_MEMBER)
        .route_with(
            Verb::Delete,
            m::PROJECT_MEMBER,
            Action::MemberManage,
            "Remove a direct project member (Maintainer; never the last Owner)",
            m::ProjectMemberToRemove,
            m::remove_project,
        )
        .responds_no_content(Verb::Delete, m::PROJECT_MEMBER)
        .route_with(
            Verb::Get,
            m::GROUP_MEMBERS,
            Action::MemberList,
            "List a group's direct members (every signed-in user)",
            m::GroupMembersOf,
            m::list_group,
        )
        .responds::<m::MemberPage>(Verb::Get, m::GROUP_MEMBERS)
        .queries(Verb::Get, m::GROUP_MEMBERS, m::QUERY)
        .route_with_body(
            Verb::Post,
            m::GROUP_MEMBERS,
            Action::MemberManage,
            "Add a direct group member (Maintainer; at most the caller's role)",
            m::MAX_BODY,
            m::NewGroupMember,
            m::add_group,
        )
        .responds_created::<m::MemberBody>(Verb::Post, m::GROUP_MEMBERS)
        .route_with_body(
            Verb::Patch,
            m::GROUP_MEMBER,
            Action::MemberManage,
            "Change a direct group member's role (Maintainer; never above the caller's role)",
            m::MAX_BODY,
            m::GroupMemberToChange,
            m::change_group,
        )
        .responds::<m::MemberBody>(Verb::Patch, m::GROUP_MEMBER)
        .route_with(
            Verb::Delete,
            m::GROUP_MEMBER,
            Action::MemberManage,
            "Remove a direct group member (Maintainer; a top-level group keeps an Owner)",
            m::GroupMemberToRemove,
            m::remove_group,
        )
        .responds_no_content(Verb::Delete, m::GROUP_MEMBER)
        .route_with(
            Verb::Get,
            pr::COLLECTION,
            Action::ProtectedRefList,
            "List a project's protected ref rules (Reporter; internal: every signed-in user)",
            pr::RulesOf,
            pr::list,
        )
        .responds::<pr::ProtectedRefRules>(Verb::Get, pr::COLLECTION)
        .route_with_body(
            Verb::Post,
            pr::COLLECTION,
            Action::ProtectedRefManage,
            "Add a protected ref rule (Maintainer; at most the caller's role)",
            pr::MAX_BODY,
            pr::NewRule,
            pr::create,
        )
        .responds_created::<pr::ProtectedRefBody>(Verb::Post, pr::COLLECTION)
        .route_with_body(
            Verb::Patch,
            pr::ONE,
            Action::ProtectedRefManage,
            "Change a rule's minimum push role (Maintainer; never above the caller's role)",
            pr::MAX_BODY,
            pr::RuleToChange,
            pr::change,
        )
        .responds::<pr::ProtectedRefBody>(Verb::Patch, pr::ONE)
        .route_with(
            Verb::Delete,
            pr::ONE,
            Action::ProtectedRefManage,
            "Delete a protected ref rule (Maintainer; never above the caller's role)",
            pr::RuleToDelete,
            pr::delete,
        )
        .responds_no_content(Verb::Delete, pr::ONE)
        // M3 R6 (ADR-0024 §12): a rule's merge conditions.
        .route_with_body(
            Verb::Put,
            pmr::MERGE_RULE,
            Action::ProtectedRefManage,
            "Set a protected branch rule's merge conditions: minimum approvals, required checks, resolved discussions, the direct-push ban (people only; Maintainer; never above the caller's role)",
            pmr::MAX_BODY,
            pmr::MergeRuleOf,
            pmr::put,
        )
        .responds::<pr::ProtectedRefBody>(Verb::Put, pmr::MERGE_RULE)
        .route_with(
            Verb::Delete,
            pmr::MERGE_RULE,
            Action::ProtectedRefManage,
            "Remove a protected branch rule's merge conditions (people only; Maintainer; never above the caller's role)",
            pmr::MergeRuleOf,
            pmr::delete,
        )
        .responds_no_content(Verb::Delete, pmr::MERGE_RULE)
}

/// Merge requests (M3 R1a, ADR-0024): the list decided on the project,
/// every other route proved on the merge request as loaded (or the one
/// about to be opened) and, for a write, decided again inside its
/// transaction (forge-review).
fn merge_request_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::merge_requests as mr;
    registry
        .route_with(
            Verb::Get,
            mr::COLLECTION,
            Action::MergeRequestList,
            "List a project's merge requests, newest first (as the repository browse API)",
            mr::MergeRequestsOf,
            mr::list,
        )
        .responds::<mr::MergeRequestPage>(Verb::Get, mr::COLLECTION)
        .queries(Verb::Get, mr::COLLECTION, mr::QUERY)
        .route_with_body(
            Verb::Post,
            mr::COLLECTION,
            Action::MergeRequestCreate,
            "Open a merge request between two existing branches (Developer)",
            mr::MAX_BODY,
            mr::NewMergeRequest,
            mr::create,
        )
        .responds_created::<mr::MergeRequestBody>(Verb::Post, mr::COLLECTION)
        .route_with(
            Verb::Get,
            mr::ONE,
            Action::MergeRequestRead,
            "Read a merge request (as the repository browse API)",
            mr::MergeRequestOf,
            mr::view,
        )
        .responds::<mr::MergeRequestBody>(Verb::Get, mr::ONE)
        .route_with_body(
            Verb::Patch,
            mr::ONE,
            Action::MergeRequestUpdate,
            "Change a merge request's title, description, draft flag or target (the author or Developer)",
            mr::MAX_BODY,
            mr::MergeRequestToUpdate,
            mr::update,
        )
        .responds::<mr::MergeRequestBody>(Verb::Patch, mr::ONE)
        .route_with(
            Verb::Post,
            mr::CLOSE,
            Action::MergeRequestClose,
            "Close a merge request (the author or Developer)",
            mr::MergeRequestToClose,
            mr::close,
        )
        .responds::<mr::MergeRequestBody>(Verb::Post, mr::CLOSE)
        .route_with(
            Verb::Post,
            mr::REOPEN,
            Action::MergeRequestReopen,
            "Reopen a closed merge request (the author or Developer; never a merged one)",
            mr::MergeRequestToReopen,
            mr::reopen,
        )
        .responds::<mr::MergeRequestBody>(Verb::Post, mr::REOPEN)
        // M3 R1b (ADR-0024 §2): secret removal, a human administrator's
        // session only (every token is refused before the lookup).
        .route_with_body(
            Verb::Post,
            mr::PURGE,
            Action::MergeRequestVersionRefsPurge,
            "Remove a merge request's hidden version refs (secret removal; an administrator's web session only)",
            mr::PURGE_MAX_BODY,
            mr::MergeRequestToPurge,
            mr::purge_version_refs,
        )
        .responds::<mr::VersionRefsPurged>(Verb::Post, mr::PURGE)
        // M3 R2 (ADR-0024 §1–§3): versions, their diffs and the
        // mergeability, read like the merge request itself.
        .route_with(
            Verb::Get,
            mr::VERSIONS,
            Action::MergeRequestRead,
            "List a merge request's versions, oldest first (as the repository browse API)",
            mr::VERSIONS_OF,
            mr::versions,
        )
        .responds::<mr::VersionsBody>(Verb::Get, mr::VERSIONS)
        .route_with(
            Verb::Get,
            mr::VERSION_DIFF,
            Action::MergeRequestRead,
            "One version's diff, merge base to head (as the repository browse API)",
            mr::VersionDiffOf,
            mr::version_diff,
        )
        .responds::<mr::VersionDiffBody>(Verb::Get, mr::VERSION_DIFF)
        .route_with(
            Verb::Get,
            mr::COMPARE,
            Action::MergeRequestRead,
            "Compare two versions of a merge request, head to head (as the repository browse API)",
            mr::COMPARE_OF,
            mr::compare,
        )
        .responds::<mr::VersionCompareBody>(Verb::Get, mr::COMPARE)
        .queries(Verb::Get, mr::COMPARE, mr::COMPARE_QUERY)
        .route_with(
            Verb::Get,
            mr::MERGEABILITY,
            Action::MergeRequestRead,
            "Whether a merge request can be merged, as last checked (as the repository browse API)",
            mr::MERGEABILITY_OF,
            mr::mergeability,
        )
        .responds::<mr::MergeabilityBody>(Verb::Get, mr::MERGEABILITY)
        // M3 X6 (plan 04 C-2): predicted conflicts with other open merge
        // requests, read like the merge request itself.
        .route_with(
            Verb::Get,
            mr::CONFLICTS,
            Action::MergeRequestRead,
            "A merge request's predicted conflicts with other open merge requests on its target, as last scanned (as the repository browse API)",
            mr::CONFLICTS_OF,
            mr::conflicts,
        )
        .responds::<mr::ConflictsBody>(Verb::Get, mr::CONFLICTS)
        // M3 X5 (plan 03 §6): a merge request's review context, read like
        // the merge request itself.
        .route_with(
            Verb::Get,
            routes::context::MERGE_REQUEST,
            Action::MergeRequestRead,
            "A merge request's review context: its changed files, the requirements it changes, its work items and the check runs on its head, each with its source (as the repository browse API)",
            routes::context::MERGE_REQUEST_OF,
            routes::context::merge_request,
        )
        .responds::<routes::context::ContextMergeRequestBody>(
            Verb::Get,
            routes::context::MERGE_REQUEST,
        )
        // M3 R3 (ADR-0024 §6, §12): discussions — read like the merge
        // request, started and answered as `MergeRequestComment`, resolved
        // as `DiscussionResolve` on the thread.
        .route_with(
            Verb::Get,
            mr::DISCUSSIONS,
            Action::MergeRequestRead,
            "List a merge request's discussions with their notes and whether a later version changed their line (as the repository browse API)",
            mr::DISCUSSIONS_OF,
            mr::discussions,
        )
        .responds::<mr::DiscussionsBody>(Verb::Get, mr::DISCUSSIONS)
        .route_with_body(
            Verb::Post,
            mr::DISCUSSIONS,
            Action::MergeRequestComment,
            "Start a discussion, general or on a line of a version's diff (Reporter; agents' tokens too)",
            mr::DISCUSSION_MAX_BODY,
            mr::DiscussionToStart,
            mr::start_discussion,
        )
        .responds_created::<mr::DiscussionBody>(Verb::Post, mr::DISCUSSIONS)
        .route_with_body(
            Verb::Post,
            mr::NOTES,
            Action::MergeRequestComment,
            "Reply to a discussion (Reporter; agents' tokens too)",
            mr::DISCUSSION_MAX_BODY,
            mr::DiscussionToReply,
            mr::reply_to_discussion,
        )
        .responds_created::<mr::DiscussionBody>(Verb::Post, mr::NOTES)
        .route_with(
            Verb::Post,
            mr::RESOLVE,
            Action::DiscussionResolve,
            "Resolve a discussion (people only: its starter, or a Developer who is not the merge request's author)",
            mr::TO_RESOLVE,
            mr::resolve_discussion,
        )
        .responds::<mr::DiscussionBody>(Verb::Post, mr::RESOLVE)
        .route_with(
            Verb::Post,
            mr::UNRESOLVE,
            Action::DiscussionResolve,
            "Unresolve a discussion (as resolving)",
            mr::TO_UNRESOLVE,
            mr::unresolve_discussion,
        )
        .responds::<mr::DiscussionBody>(Verb::Post, mr::UNRESOLVE)
        // M3 R4 (ADR-0024 §4): queue a merge (202, the job runs it) and read
        // the latest attempt.
        .route_with_body(
            Verb::Post,
            mr::MERGE,
            Action::MergeRequestMerge,
            "Merge a merge request (people only; the target branch's push role; a token needs merge_execute). Queues the merge job",
            mr::MERGE_MAX_BODY,
            mr::MergeRequestToMerge,
            mr::merge,
        )
        .responds_accepted::<mr::MergeAttemptBody>(Verb::Post, mr::MERGE)
        .route_with(
            Verb::Get,
            mr::MERGE,
            Action::MergeRequestRead,
            "A merge request's latest merge attempt (as the repository browse API)",
            mr::MergeStatusOf,
            mr::merge_status,
        )
        .responds::<mr::MergeStatusBody>(Verb::Get, mr::MERGE)
        // M3 R5 (ADR-0024 §8, §9): approvals of the head the caller saw,
        // their evaluation, and the project's approval rules.
        .route_with_body(
            Verb::Post,
            mr::APPROVE,
            Action::MergeRequestApprove,
            "Approve a merge request's head (people only; Developer; never the author; a token needs merge_approve)",
            mr::APPROVAL_MAX_BODY,
            mr::TO_APPROVE,
            mr::approve,
        )
        .responds::<mr::ApprovalAnswerBody>(Verb::Post, mr::APPROVE)
        .route_with(
            Verb::Post,
            mr::UNAPPROVE,
            Action::MergeRequestUnapprove,
            "Withdraw one's own approval of a merge request (as approving)",
            mr::TO_UNAPPROVE,
            mr::unapprove,
        )
        .responds::<mr::ApprovalAnswerBody>(Verb::Post, mr::UNAPPROVE)
        // M3 I1b (plan 02 §2, §5.7): a merge request's work item links,
        // detected and a person's.
        .route_with(
            Verb::Get,
            mr::WORK_ITEM_LINKS,
            Action::MergeRequestRead,
            "A merge request's work item links, open and ended, oldest first (as the repository browse API)",
            mr::LinksOf,
            mr::work_item_links,
        )
        .responds::<mr::WorkItemLinksBody>(Verb::Get, mr::WORK_ITEM_LINKS)
        .route_with(
            Verb::Put,
            mr::WORK_ITEM_LINK,
            Action::WorkItemLink,
            "Link a work item (PBI-<n> of the project's kanflow binding) to a merge request by hand (Developer)",
            mr::LinkOf,
            mr::link_work_item,
        )
        .responds::<mr::WorkItemLinkAnswerBody>(Verb::Put, mr::WORK_ITEM_LINK)
        .route_with(
            Verb::Delete,
            mr::WORK_ITEM_LINK,
            Action::WorkItemLink,
            "Unlink a work item from a merge request; the link stays in its history and is not detected again (Developer)",
            mr::LinkOf,
            mr::unlink_work_item,
        )
        .responds_no_content(Verb::Delete, mr::WORK_ITEM_LINK)
        .route_with(
            Verb::Get,
            mr::APPROVALS,
            Action::MergeRequestRead,
            "Evaluate a merge request's approvals now: how each counts, the rules, the CODEOWNERS rules its changes need, and what blocks the merge (as the repository browse API)",
            mr::APPROVALS_OF,
            mr::approvals,
        )
        .responds::<mr::ApprovalStateBody>(Verb::Get, mr::APPROVALS)
        .route_with(
            Verb::Get,
            mr::APPROVAL_RULES,
            Action::ProjectRead,
            "List a project's approval rules (as reading the project)",
            mr::RulesProject,
            mr::approval_rules,
        )
        .responds::<mr::ApprovalRulesBody>(Verb::Get, mr::APPROVAL_RULES)
        .route_with_body(
            Verb::Put,
            mr::APPROVAL_RULE,
            Action::ApprovalRuleManage,
            "Create or replace an approval rule (people only; Maintainer)",
            mr::APPROVAL_MAX_BODY,
            mr::RulesProject,
            mr::put_approval_rule,
        )
        .responds::<mr::ApprovalRuleBody>(Verb::Put, mr::APPROVAL_RULE)
        .route_with(
            Verb::Delete,
            mr::APPROVAL_RULE,
            Action::ApprovalRuleManage,
            "Delete an approval rule (people only; Maintainer)",
            mr::RulesProject,
            mr::delete_approval_rule,
        )
        .responds_no_content(Verb::Delete, mr::APPROVAL_RULE)
        // M3 R6 (ADR-0024 §7, §4.1, §8, §9): the merge status, the
        // project's merge settings and the time-boxed CODEOWNERS switch.
        .route_with(
            Verb::Get,
            mr::STATUS,
            Action::MergeRequestRead,
            "Evaluate whether a merge request may merge now: every blocking reason in a fixed order, the approvals, the required checks, the discussions (a stable contract, schema_version 1; as the repository browse API)",
            mr::STATUS_OF,
            mr::status,
        )
        .responds::<mr::MergeRequestStatusBody>(Verb::Get, mr::STATUS)
        .route_with(
            Verb::Get,
            mr::MERGE_SETTINGS,
            Action::ProjectRead,
            "Read a project's merge settings and CODEOWNERS enforcement (as reading the project)",
            mr::SettingsProject,
            mr::merge_settings,
        )
        .responds::<mr::MergeSettingsBody>(Verb::Get, mr::MERGE_SETTINGS)
        .route_with_body(
            Verb::Put,
            mr::MERGE_SETTINGS,
            Action::MergeSettingsManage,
            "Replace a project's merge settings: methods, default, source deletion, approvals on push, committers' approvals (people only; Maintainer)",
            mr::SETTINGS_MAX_BODY,
            mr::SettingsChangeOf,
            mr::put_merge_settings,
        )
        .responds::<mr::MergeSettingsBody>(Verb::Put, mr::MERGE_SETTINGS)
        .route_with_body(
            Verb::Put,
            mr::CODEOWNERS_ENFORCEMENT,
            Action::CodeownersEnforcementToggle,
            "Suspend CODEOWNERS enforcement for 1 to 1440 minutes or enforce it again (people only; Maintainer at or above every protection of the project)",
            mr::SETTINGS_MAX_BODY,
            mr::CodeownersOf,
            mr::put_codeowners_enforcement,
        )
        .responds::<mr::MergeSettingsBody>(Verb::Put, mr::CODEOWNERS_ENFORCEMENT)
}

/// Group and project writes and the group read (M1 phase 8b MR7a): every
/// body route reads its strict JSON first, the registry proves the action
/// on what the loader read, and the handler's command decides again inside
/// its transaction (forge-hosting).
fn hosting_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::{groups, project_writes as pw};
    registry
        .route_with_body(
            Verb::Post,
            groups::COLLECTION,
            Action::GroupCreateTopLevel,
            "Create a top-level group (administrators)",
            groups::MAX_BODY,
            groups::TopLevel,
            groups::create,
        )
        .responds_created::<groups::GroupBody>(Verb::Post, groups::COLLECTION)
        .route_with_body(
            Verb::Post,
            groups::SUBGROUPS,
            Action::SubgroupCreate,
            "Create a subgroup (group Owner)",
            groups::MAX_BODY,
            groups::ParentGroup,
            groups::create_subgroup,
        )
        .responds_created::<groups::GroupBody>(Verb::Post, groups::SUBGROUPS)
        .route_with(
            Verb::Get,
            groups::ONE,
            Action::GroupRead,
            "Read one group by id",
            groups::GroupById,
            groups::read,
        )
        .responds::<groups::GroupBody>(Verb::Get, groups::ONE)
        .route_with_body(
            Verb::Patch,
            groups::ONE,
            Action::GroupUpdate,
            "Change a group's display name / description (group Owner)",
            groups::MAX_BODY,
            groups::GroupToUpdate,
            groups::update,
        )
        .responds::<groups::GroupBody>(Verb::Patch, groups::ONE)
        .route_with_body(
            Verb::Post,
            pw::COLLECTION,
            Action::ProjectCreate,
            "Create a project in a group or a personal namespace",
            pw::MAX_BODY,
            pw::Parent,
            pw::create,
        )
        .responds_created::<routes::projects::ProjectBody>(Verb::Post, pw::COLLECTION)
        .route_with_body(
            Verb::Patch,
            routes::projects::PATH,
            Action::ProjectUpdate,
            "Change a project's display name / description (Maintainer)",
            pw::MAX_BODY,
            pw::ProjectToUpdate,
            pw::update,
        )
        .responds::<routes::projects::ProjectBody>(Verb::Patch, routes::projects::PATH)
        .route_with_body(
            Verb::Put,
            pw::VISIBILITY,
            Action::ProjectVisibilityChange,
            "Set a project's visibility (Owner)",
            pw::MAX_BODY,
            pw::VisibilityTarget,
            pw::set_visibility,
        )
        .responds::<routes::projects::ProjectBody>(Verb::Put, pw::VISIBILITY)
        .route_with(
            Verb::Delete,
            routes::projects::PATH,
            Action::ProjectDelete,
            "Delete a project: it stays restorable for the grace period (Owner)",
            routes::projects::ProjectById,
            pw::delete,
        )
        .responds::<pw::DeletionBody>(Verb::Delete, routes::projects::PATH)
        .route_with(
            Verb::Post,
            pw::RESTORE,
            Action::ProjectRestore,
            "Restore a project pending deletion (Owner)",
            pw::ProjectToRestore,
            pw::restore,
        )
        .responds::<routes::projects::ProjectBody>(Verb::Post, pw::RESTORE)
}

/// Namespace moves (step D5 MR5, ADR-0015 Amendment D5): a project's or a
/// group's move or rename, a user's rename and a tombstone's release, each
/// proved on what its loader read and decided again inside its transaction
/// (forge-hosting); agents are denied all of them.
fn move_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::moves as mv;
    registry
        .route_with_body(
            Verb::Post,
            mv::PROJECT_TRANSFER,
            Action::ProjectTransfer,
            "Move or rename a project (Owner; Maintainer on the target group, or the personal namespace's user)",
            mv::MAX_BODY,
            mv::ProjectToMove,
            mv::transfer_project,
        )
        .responds::<mv::MoveBody>(Verb::Post, mv::PROJECT_TRANSFER)
        .route_with_body(
            Verb::Post,
            mv::GROUP_TRANSFER,
            Action::GroupTransfer,
            "Move or rename a group and its subtree (Owner on both sides; the top level: administrators)",
            mv::MAX_BODY,
            mv::GroupToMove,
            mv::transfer_group,
        )
        .responds::<mv::MoveBody>(Verb::Post, mv::GROUP_TRANSFER)
        .route_with_body(
            Verb::Post,
            mv::USER_RENAME,
            Action::UserRename,
            "Rename a user and their personal namespace (administrators)",
            mv::MAX_BODY,
            mv::UserToRename,
            mv::rename_user,
        )
        .responds::<mv::MoveBody>(Verb::Post, mv::USER_RENAME)
        .route_with_body(
            Verb::Post,
            mv::TOMBSTONE_RELEASE,
            Action::NamespaceTombstoneRelease,
            "Release a namespace tombstone so its path can be used again (the target's Owner, or an administrator)",
            mv::MAX_BODY,
            mv::TombstoneAtPath,
            mv::release_tombstone,
        )
        .responds::<mv::ReleasedBody>(Verb::Post, mv::TOMBSTONE_RELEASE)
}

/// Smart HTTP (M1 phase 9): fetch (MR3) — the advertisement (`GET`, and
/// `HEAD` with the headers only) and the upload-pack RPC, `RepoRead`; push
/// (MR4) — the same for receive-pack, `RepoPush` (decided on the project
/// before the body is read; every ref again in the handler).
fn git_routes(registry: Registry<AppState>) -> Registry<AppState> {
    let advertise = Some(("service", "git-upload-pack"));
    let advertise_push = Some(("service", "git-receive-pack"));
    registry
        .scoped(
            RouteScope::ProjectGit,
            Verb::Get,
            ScopedRoute::Git(GitRoute::InfoRefsReceivePack),
            advertise_push,
            Action::RepoPush,
            "Smart HTTP: receive-pack advertisement (push)",
            routes::git::advertise_receive_pack,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Head,
            ScopedRoute::Git(GitRoute::InfoRefsReceivePack),
            advertise_push,
            Action::RepoPush,
            "Smart HTTP: receive-pack advertisement headers",
            routes::git::advertise_receive_pack,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Post,
            ScopedRoute::Git(GitRoute::ReceivePack),
            None,
            Action::RepoPush,
            "Smart HTTP: receive-pack (push); every ref decided before git runs",
            routes::git_push::receive_pack,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Get,
            ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
            advertise,
            Action::RepoRead,
            "Smart HTTP: upload-pack advertisement (clone / fetch)",
            routes::git::advertise,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Head,
            ScopedRoute::Git(GitRoute::InfoRefsUploadPack),
            advertise,
            Action::RepoRead,
            "Smart HTTP: upload-pack advertisement headers",
            routes::git::advertise,
        )
        .scoped(
            RouteScope::ProjectGit,
            Verb::Post,
            ScopedRoute::Git(GitRoute::UploadPack),
            None,
            Action::RepoRead,
            "Smart HTTP: upload-pack (clone / fetch); every want checked reachable",
            routes::git::upload_pack,
        )
}

/// The SPA shell (`WebShell`) for every page URL, `GET` and `HEAD`, and
/// the UI's files under `/-/assets/{file}` (public too, ADR-0015 Amendment
/// M2; unknown names are `FG-WEB-0001`).
fn shell_routes(mut registry: Registry<AppState>) -> Registry<AppState> {
    let shell = routes::shell::shell;
    for verb in [Verb::Get, Verb::Head] {
        registry = registry
            // The site root is the home page; no scoped route matches `/`
            // (found in the 2026-11-02 test-server rehearsal: it was a 404).
            .route(verb, "/", Action::WebShell, "SPA shell: home", shell)
            .scoped(
                RouteScope::Instance,
                verb,
                ScopedRoute::Instance(InstanceRoute::Assets),
                None,
                Action::WebShell,
                "Web UI asset (embedded build; content-hashed, immutable)",
                routes::assets::asset,
            )
            // The act-as confirmation popup (ADR-0025 §5.4, U3): the same
            // shell bytes; the headers (frame-ancestors, X-Frame-Options,
            // COOP) come from the one table in `security_headers`.
            .route(
                verb,
                routes::act_as::PAGE,
                Action::WebShell,
                "SPA shell: act-as confirmation popup",
                shell,
            )
            // The binding confirmation popup (ADR-0025 Amendment X3 §4).
            .route(
                verb,
                routes::integration_bindings::PAGE,
                Action::WebShell,
                "SPA shell: act-as binding confirmation popup",
                shell,
            )
            // The in-app notifications page (M3 U4).
            .route(
                verb,
                routes::in_app_notifications::PAGE,
                Action::WebShell,
                "SPA shell: the caller's in-app notifications",
                shell,
            )
            // The "My work" sidebar pages (plan 15 MR 1). Without a route a
            // reload or a pasted link answers 404.
            .route(
                verb,
                MY_PROJECTS_PAGE,
                Action::WebShell,
                "SPA shell: the caller's projects",
                shell,
            )
            .route(
                verb,
                MY_GROUPS_PAGE,
                Action::WebShell,
                "SPA shell: the groups the caller can see",
                shell,
            )
            .route(
                verb,
                MY_MERGE_REQUESTS_PAGE,
                Action::WebShell,
                "SPA shell: the caller's merge requests",
                shell,
            )
            .route(
                verb,
                MY_ACTIVITY_PAGE,
                Action::WebShell,
                "SPA shell: the caller's activity",
                shell,
            )
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
    // The shell's `form-action` also allows the identity provider's origin
    // when `auth.oidc` is configured: the logout form's 303 goes to its
    // `end_session_endpoint` (M2 MR5, ADR-0021 §8, ADR-0022 §3).
    let idp_origin = state.oidc.as_ref().and_then(|o| o.issuer_origin());
    let security = SecurityHeaders::new(config.hsts, idp_origin.as_deref())?;
    let origin = ExpectedOrigin(state.web_origin.clone()); // clone: shared by both checks
    let sessions = SessionsExt {
        sessions: state.sessions.clone(), // clone: the Arc is shared
        origin: origin.clone(),           // clone: the browser check keeps one
    };
    Ok(registry
        .into_router()?
        .method_not_allowed_fallback(method_not_allowed)
        // DefaultBodyLimit bounds axum's own body extractors (no route uses
        // one); the registry's JSON bodies read BodyLimit and answer
        // FG-WEB-0009 (`crate::body`).
        .layer(DefaultBodyLimit::max(config.body_limit_bytes))
        .layer(axum::Extension(crate::body::BodyLimit(
            config.body_limit_bytes,
        )))
        .layer(axum::Extension(crate::body::BodyReadTimeout(
            config.body_read_timeout,
        )))
        .layer(axum::Extension(authenticator))
        .layer(axum::Extension(sessions))
        .layer(axum::Extension(crate::act_as::ActAsExt {
            act_as: state.act_as.clone(), // clone: the Arc is shared
            failures: Arc::clone(&state.act_as_failures),
        }))
        // ADR-0021 §6 check 1: before any route, whatever the credentials.
        .layer(middleware::from_fn_with_state(
            origin,
            crate::csrf::browser_origin,
        ))
        .layer(middleware::from_fn(catch_panic))
        .layer(middleware::from_fn_with_state(
            (config.request_timeout, config.git_rpc_timeout),
            request_timeout,
        ))
        // ADR-0019 Amendment M2: every 401 gets its class's challenges.
        .layer(middleware::from_fn(crate::error::challenge_layer))
        .layer(middleware::from_fn(context::request_id))
        // A failed browser navigation of the sign-in flow is a page, not
        // the JSON body (`crate::error_page`); every failure, the timeout's
        // and the panic catcher's included, passes it.
        .layer(middleware::from_fn(crate::error_page::auth_error_page))
        // Outermost: every answer, the timeout's and the panic catcher's
        // included, gets its class's headers (ADR-0022 §3).
        .layer(middleware::from_fn_with_state(
            security,
            crate::security_headers::security_headers,
        ))
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

/// `POST {path}.git/git-upload-pack` / `git-receive-pack` (by the raw path,
/// as the dispatcher reads it): exempt from the request timeout.
pub(crate) fn is_git_rpc(req: &Request) -> bool {
    req.method() == axum::http::Method::POST
        && (routes::job_fetch::is_job_upload_pack(req.uri().path())
            || matches!(
                parse_web_path(req.uri().path(), req.uri().query()),
                Ok(ParsedPath::Git {
                    route: GitRoute::UploadPack | GitRoute::ReceivePack,
                    ..
                })
            ))
}

async fn request_timeout(
    State((request_limit, git_rpc_limit)): State<(Duration, Duration)>,
    req: Request,
    next: Next,
) -> Response {
    let limit = if is_git_rpc(&req) {
        git_rpc_limit
    } else {
        request_limit
    };
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

/// The "My work" sidebar's projects page (plan 15).
pub const MY_PROJECTS_PAGE: &str = "/-/projects";
/// The "My work" sidebar's groups page (plan 15).
pub const MY_GROUPS_PAGE: &str = "/-/groups";
/// The "My work" sidebar's merge requests page (plan 15 MR 7).
pub const MY_MERGE_REQUESTS_PAGE: &str = "/-/merge-requests";
/// The "My work" sidebar's activity page (plan 15 MR 9).
pub const MY_ACTIVITY_PAGE: &str = "/-/activity";
