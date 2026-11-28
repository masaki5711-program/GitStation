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
    /// Document revisions and edit claims (plan 13 stage 4 part A; set
    /// with the checks by [`AppState::with_checks`]; without it those
    /// routes answer `FG-SYS-0001 reason=checks_unconfigured`, and opening
    /// a merge request skips the one-revision rule).
    pub doc_revisions: Option<Arc<forge_checks::DocRevisions>>,
    /// CI pipelines (M4 C2; `forge serve` always sets them; without them
    /// those routes answer `FG-SYS-0001 reason=pipelines_unconfigured`).
    pub pipelines: Option<Arc<forge_pipelines::Pipelines>>,
    /// Deployments (M4 C11; `forge serve` always sets them; without them
    /// those routes answer `FG-SYS-0001 reason=deploy_unconfigured`).
    pub deploy: Option<Arc<forge_deploy::Deploy>>,
    /// The stored incident diagnoses (plan 19 L5-7b; `forge serve` always
    /// sets them; without them the route answers `FG-SYS-0001
    /// reason=diagnoses_unconfigured`).
    pub diagnoses: Option<Arc<forge_llm::diagnoses::Diagnoses>>,
    /// The agent marketplace (plan 16 A5; `forge serve` always sets it;
    /// without it those routes answer `FG-SYS-0001
    /// reason=marketplace_unconfigured`).
    pub marketplace: Option<Arc<forge_marketplace::Marketplace>>,
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
    /// Simple editor E1: single-file commits per caller (review E1).
    pub file_commit_limiter: Arc<routes::namespaces::CallerLimiter>,
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
    /// The SSH git transport as clients see it (M4 C13, ADR-0033): the
    /// clone host and the host key fingerprints the SSH key page shows.
    /// `None` while `git.ssh.enabled` is off.
    pub ssh_info: Option<Arc<crate::ssh::SshPublicInfo>>,
    /// The PFS portal's widget authenticator (ADR-0036; `forge serve` sets
    /// it when `portal.enabled` and `auth.oidc` are configured; without it
    /// a widget route answers `FG-AUTH-0026 reason=portal_disabled`).
    pub portal: Option<Arc<dyn forge_core::portal::PortalAuthenticator>>,
    /// ADR-0036 review S3: the widget routes' gate — the requests running
    /// at once and per minute, all persons together (`FG-WEB-0010
    /// route=portal_widgets`), taken before the token is verified.
    pub portal_gate: Arc<routes::portal_widgets::WidgetGate>,
    /// The `gstn` binaries this server distributes (`clients.dir`;
    /// `forge serve` always sets it; without it the client routes answer
    /// `FG-SYS-0001 reason=clients_unconfigured`).
    pub clients: Option<Arc<routes::clients::ClientDist>>,
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
            doc_revisions: None,
            pipelines: None,
            deploy: None,
            diagnoses: None,
            marketplace: None,
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
            file_commit_limiter: Arc::new(routes::file_commit::limiter()),
            spec_refs_budget: routes::spec_refs::BUDGET,
            spec_index_limiter: Arc::new(routes::specs::limiter()),
            spec_index_in_flight: Arc::default(),
            ci_output_limiter: Arc::new(routes::pipelines::output_limiter()),
            ci_output_in_flight: Arc::new(routes::specs::InFlight::new(
                routes::pipelines::OUTPUT_MAX_IN_FLIGHT,
            )),
            ssh_info: None,
            portal: None,
            portal_gate: Arc::default(),
            clients: None,
        }
    }

    /// The `gstn` distribution directory (runbook cli-distribution).
    #[must_use]
    pub fn with_clients(mut self, clients: routes::clients::ClientDist) -> Self {
        self.clients = Some(Arc::new(clients));
        self
    }

    /// The PFS portal's widget authenticator (ADR-0036).
    #[must_use]
    pub fn with_portal(mut self, portal: Arc<dyn forge_core::portal::PortalAuthenticator>) -> Self {
        self.portal = Some(portal);
        self
    }

    /// The SSH transport's public facts (M4 C13; `forge serve` sets them
    /// when `git.ssh.enabled`).
    #[must_use]
    pub fn with_ssh_info(mut self, info: crate::ssh::SshPublicInfo) -> Self {
        self.ssh_info = Some(Arc::new(info));
        self
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
        self.doc_revisions = Some(Arc::new(checks.doc_revisions()));
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

    /// Deployments (M4 C11).
    #[must_use]
    pub fn with_deploy(mut self, deploy: forge_deploy::Deploy) -> Self {
        self.deploy = Some(Arc::new(deploy));
        self
    }

    /// The stored incident diagnoses (plan 19 L5-7b).
    #[must_use]
    pub fn with_diagnoses(mut self, diagnoses: forge_llm::diagnoses::Diagnoses) -> Self {
        self.diagnoses = Some(Arc::new(diagnoses));
        self
    }

    /// The agent marketplace (plan 16 A5).
    #[must_use]
    pub fn with_marketplace(mut self, marketplace: forge_marketplace::Marketplace) -> Self {
        self.marketplace = Some(Arc::new(marketplace));
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
    /// `web.base_url` is plain `http` off the loopback
    /// (`web.allow_insecure_http`, a trial server only): the session and
    /// sign-in cookies are issued and read under their plain-HTTP,
    /// non-`Secure` names `gstn_<port>_…` with this port of the base URL
    /// (`crate::plain_http_cookies`). `None` (the default) installs nothing.
    pub plain_http_port: Option<u16>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            git_rpc_timeout: Duration::from_secs(3600),
            body_limit_bytes: 1024 * 1024,
            body_read_timeout: Duration::from_secs(10),
            hsts: None,
            plain_http_port: None,
        }
    }
}

/// Actions that have permission-matrix rows but no route yet (API
/// resources: phase 8b).
/// Explicit, so the `authz-coverage` gate can tell "not built yet" from a
/// stale row; an action leaves this list in the commit that routes it.
pub const PLANNED: &[Action] = &[
    Action::UserCreate,
    Action::AuditRead,
    Action::Metrics,
    // Plan 16 §4 (A4): the lesson injection lands with A10 (the
    // builder's routes with A7, the marketplace's with A5).
    Action::RuleInjectLesson,
];

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
    // Plan 16 A7: the recommendation reads the default tip as a whole copy
    // does, decided in the route after the project-level `RepoRead`
    // (`forge_auth::authorize_agent_recommend`, ADR-0032 Amendment A4 5).
    (
        Action::AgentBuilderRecommend,
        routes::agent_builder::RECOMMEND,
    ),
    // M4 C12 review: the second decision of a manual job's start when its
    // pipeline is protected (decided in the transaction).
    (Action::PipelinePlayProtected, routes::pipelines::PLAY),
];

/// The words whose page is the SPA shell on a project (ADR-0015 "ルートの
/// 系統"). `raw` and `archive` are `RepoRead` content routes
/// (`routes::content`, M2 MR7). `pulls` / `pull` are the merge request
/// screens (MR B0; their data is the `MergeRequestList` / `MergeRequestRead`
/// API). `documents` is the document screens (plan 13 stage 3; their data
/// is the document API, `RepoBrowse`). `pipelines` is the CI screens (plan 14
/// M4; their data is the pipeline API, project readers). `webhooks` is the
/// webhook screens (M3 I2; their data is the webhook API, Maintainer).
/// `edit` / `new` are the simple editor (E2; its data is the browse API and
/// the single-file commit, `RepoPush`).
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
    // M4 C8: a target's merge queue (its data: the merge queue API).
    RouteWord::MergeQueue,
    RouteWord::Webhooks,
    RouteWord::Edit,
    RouteWord::New,
    // Plan 20 W4: the project's wiki (its data: `GET …/wiki` and the
    // wiki project's document API).
    RouteWord::Wiki,
    // Plan 19 L5-6: the deployments and the approval screen (its data: the
    // deployment API and `…/approval-summary`, project readers).
    RouteWord::Deployments,
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
        // Plan 20 K3 (ADR-0040): the cross search; each hit decided by
        // authorize() (ProjectRead, MergeRequestList).
        .route(
            Verb::Get,
            routes::search::PATH,
            Action::SearchQuery,
            "Search projects and merge requests the caller may read (ranked, no count, opaque cursor)",
            routes::search::search,
        )
        .responds::<routes::search::SearchPage>(Verb::Get, routes::search::PATH)
        .queries(Verb::Get, routes::search::PATH, routes::search::QUERY)
        // Plan 19 §3.3 (L5-9): kanflow's read-only tool pack; each tool is
        // a route of its own, decided there acting for the person.
        .route(
            Verb::Get,
            routes::ai_tools::PATH,
            Action::AiToolList,
            "Kanflow's read-only tool pack (plan 02 §6 F-18): each tool's name, description, JSON Schema arguments and route, called acting for the person who asked (Forge-Act-As, no assertion), and the plain words of every state name the tools answer (read_api; never acting for a person)",
            routes::ai_tools::list,
        )
        .responds::<routes::ai_tools::AiToolPackBody>(Verb::Get, routes::ai_tools::PATH)
        .route(
            Verb::Get,
            routes::users::PATH,
            Action::UserSelfRead,
            "The caller's own account (M1 phase 8b MR6)",
            routes::users::me,
        )
        .responds::<routes::users::UserSelf>(Verb::Get, routes::users::PATH)
        // The `gstn` CLI distribution (runbook cli-distribution).
        .route(
            Verb::Get,
            routes::clients::LIST,
            Action::ClientDownload,
            "The gstn binaries this server distributes (only the targets present), with version, SHA-256, size and download path",
            routes::clients::list,
        )
        .responds::<routes::clients::ClientList>(Verb::Get, routes::clients::LIST)
        .route(
            Verb::Get,
            routes::clients::BINARY,
            Action::ClientDownload,
            "Download one gstn binary (target windows-x86_64 or linux-x86_64; application/octet-stream, X-Checksum-Sha256; 404 FG-WEB-0012 when not shipped)",
            routes::clients::binary,
        )
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
    let all = autofix_routes(llm_routes(ai_assets_routes(schedule_routes(
        auto_merge_routes(deploy_routes(user_activity_routes(
            user_merge_request_routes(runner_routes(pipeline_routes(merge_queue_routes(
                doc_lint_routes(binding_routes(act_as_routes(mirror_routes(
                    webhook_routes(notification_routes(check_routes(dev_status_routes(
                        feed_routes(integration_routes(service_account_routes(account_routes(
                            history_routes(browse_routes(git_routes(shell_routes(
                                merge_request_routes(org_binding_routes(member_routes(
                                    move_routes(hosting_routes(fixed)),
                                ))),
                            )))),
                        )))),
                    )))),
                )))),
            )))),
        ))),
    ))));
    // ADR-0036: the PFS portal's home widgets; plan 20 W2: the wiki;
    // plan 20 N1: notes; plan 16 A5: the agent marketplace.
    agent_builder_routes(agent_part_routes(notes_routes(wiki_routes(
        portal_widget_routes(all),
    ))))
}

/// A person's notes (plan 20 N1, ADR-0040 §6): create them, read the
/// pointer. A quick note is the simple editor's commit on the notes.
fn notes_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::notes as n;
    registry
        .route(
            Verb::Get,
            n::PATH,
            Action::ProjectList,
            "The caller's notes — the private project <user>/notes, proved ProjectRead with the caller's own credential: {project_id, path, owner, default_branch}; 404 FG-REPO-0005 without notes, 404 FG-KNOW-0005 when the notes project was moved out of <user>/notes or is pending deletion (create them again)",
            n::read,
        )
        .responds::<n::NotesBody>(Verb::Get, n::PATH)
        .route_with(
            Verb::Post,
            n::PATH,
            Action::PersonalNotesCreate,
            "Create the caller's notes in one step: the private project <user>/notes in their own namespace (also ProjectCreate there), its first commit on main (.forge/doctypes/note.yml with owner_kind: user, README.md), then the pointer; a stale pointer (FG-KNOW-0005) is replaced. The person only (never an agent, a service account, acting for a person or CI), api; 409 FG-KNOW-0006 refused (already exists, name taken), 500 FG-KNOW-0007 when a step after the creation failed (no pointer written)",
            n::OwnNamespace,
            n::create,
        )
        .responds_created::<n::CreatedNotesBody>(Verb::Post, n::PATH)
}

/// The agent marketplace (plan 16 §6 A5, ADR-0032 Amendment A5): the
/// catalog and the public keys for every signed-in account; publishing,
/// promoting and revoking in a person's web session.
fn agent_part_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::agent_parts as m;
    registry
        .route_with(
            Verb::Get,
            m::LIST,
            Action::AgentPartList,
            "The agent parts catalog by slug (kind, channel, q): each part with its newest version that is not revoked (every signed-in account)",
            m::Catalog,
            m::list,
        )
        .responds::<m::AgentPartListBody>(Verb::Get, m::LIST)
        .queries(Verb::Get, m::LIST, m::LIST_QUERY)
        .route_with(
            Verb::Get,
            m::ONE,
            Action::AgentPartRead,
            "One agent part: every published version, newest first (channel, state, source commit, digest, signature, key id, capabilities); 404 FG-REPO-0005 without the part (every signed-in account)",
            m::Catalog,
            m::read,
        )
        .responds::<m::AgentPartBody>(Verb::Get, m::ONE)
        .route_with(
            Verb::Get,
            m::VERSION,
            Action::AgentPartRead,
            "One published agent part version with its files (path, mode, sha256, size); 404 FG-REPO-0005 without it (every signed-in account)",
            m::Catalog,
            m::read_version,
        )
        .responds::<m::AgentPartVersionDetailBody>(Verb::Get, m::VERSION)
        .route_with_body(
            Verb::Post,
            m::VERSIONS,
            Action::AgentPartPublish,
            "Publish the tag <slug>/v<version> of the parts repository ({\"tag\"}): the commit the tag names now is read with git, linted and scanned (forge-agent), must be on the default branch with its newest pipeline succeeded (409 FG-AGENT-0004), signed with the server key and frozen as a beta version (201); 409 FG-AGENT-0012 when the version exists. A Maintainer of the publishing group in a web session, a person who did not push, tag or author it (403 FG-AGENT-0009); a part using the network: an administrator",
            m::MAX_BODY,
            m::ToPublish,
            m::publish,
        )
        .responds_created::<m::AgentPartVersionDetailBody>(Verb::Post, m::VERSIONS)
        .route_with(
            Verb::Post,
            m::PROMOTE,
            Action::AgentPartPromote,
            "Move a published version from beta to stable (200, also when it was stable); 409 FG-AGENT-0006 when it is revoked. The same people as publishing, decided on the facts the publication recorded",
            m::TO_PROMOTE,
            m::promote,
        )
        .responds::<m::AgentPartPromoteBody>(Verb::Post, m::PROMOTE)
        .route_with_body(
            Verb::Post,
            m::REVOKE,
            Action::AgentPartRevoke,
            "Revoke a published version ({\"reason\"}, 1..=500 characters): final, 409 FG-AGENT-0006 when it was revoked already (a Maintainer of the publishing group or an administrator, a person in a web session)",
            m::MAX_BODY,
            m::TO_REVOKE,
            m::revoke,
        )
        .responds::<m::AgentPartVersionDetailBody>(Verb::Post, m::REVOKE)
        .route_with(
            Verb::Get,
            m::KEYS,
            Action::AgentKeyRead,
            "The public keys that sign agent parts (kid, base64 public key, created, retired, revoked): the agent-keys.json gstn agent verify reads (every signed-in account)",
            m::Catalog,
            m::keys,
        )
        .responds::<m::AgentKeyListBody>(Verb::Get, m::KEYS)
}

/// The agent builder (plan 16 §6 A7, ADR-0032 Amendment A7): a project's
/// Claude Code configuration from signed marketplace parts.
fn agent_builder_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::agent_builder as b;
    registry
        .route_with(
            Verb::Get,
            b::STATUS,
            Action::AgentBuilderDryRun,
            "The project's newest agent builds (at most 20, newest first): the selection, the lock digest, Dry-Run or the merge request opened, the state and the error code (a Developer, an agent's token included)",
            b::STATUS_OF,
            b::status,
        )
        .responds::<b::BuildStatusBody>(Verb::Get, b::STATUS)
        .route_with_body(
            Verb::Post,
            b::RECOMMEND,
            Action::RepoRead,
            "The rules-only recommendation of agent parts ({}): the base model (the lock's adapter at the default branch, else claude-code-v2), the lock's parts and what they require, each at its newest stable version, with selected_by=rule and the reason; source=rules. Decided as AgentBuilderRecommend on a whole copy of the default tip (an AI agent audience only when the tip is open, 403 FG-AUTH-0022); 422 FG-AGENT-0008 when the base model or a requirement has no stable version",
            b::MAX_BODY,
            b::RECOMMEND_OF,
            b::recommend,
        )
        .responds::<b::RecommendBody>(Verb::Post, b::RECOMMEND)
        .route_with_body(
            Verb::Post,
            b::PREVIEW,
            Action::AgentBuilderDryRun,
            "The Dry-Run of an agent build ({\"selection\": [{\"slug\", \"version\", \"selected_by\"?, \"reason\"?}]}): the published versions are verified (digest, signature) and built (forge-agent compile: one adapter, requirements met, .claude/settings.json and .mcp.json merged), compared with the default branch's tip: the files added, modified and removed, each part's capabilities and the lock. Writes nothing but the build's record. 422 FG-AGENT-0008 for a selection that cannot be built, 409 FG-AGENT-0006 for a revoked version (a Developer, an agent's token included)",
            b::MAX_BODY,
            b::PREVIEW_OF,
            b::preview,
        )
        .responds::<b::BuildPreviewBody>(Verb::Post, b::PREVIEW)
        .route_with_body(
            Verb::Post,
            b::COMPILE,
            Action::AgentBuilderCompile,
            "Build the agent configuration ({\"selection\", \"title\"?}) as the Dry-Run would, write it as one commit of the builder bot on a new branch agent-builder/<lock digest> from the default branch's tip (the ref update decided like the caller's push, the secret rules on every file, the push recorded for the caller) and open the merge request for the caller (201). forge/agent-lock verifies it and auto-merge waits for a Maintainer's approval. A person only (never an agent, a service account or a bot), Developer; 409 FG-GIT-0027 reason=branch_exists when the same build was asked for, 409 FG-GIT-0031 when nothing changes",
            b::MAX_BODY,
            b::COMPILE_OF,
            b::compile,
        )
        .responds_created::<b::BuildCompiledBody>(Verb::Post, b::COMPILE)
        // Plan 16 A9: the update and the generation migration.
        .route_with(
            Verb::Get,
            b::UPDATES,
            Action::AgentBuilderDryRun,
            "Whether the agent configuration at the default branch's tip has an update: each locked part's version (and whether it is revoked) and the version the update picks (the newest stable version that is not revoked, never down; a revoked one without a replacement is dropped), with the update's Dry-Run; available=false with reason no_lock or up_to_date otherwise (a Developer, an agent's token included)",
            b::UPDATES_OF,
            b::updates,
        )
        .responds::<b::UpdatesBody>(Verb::Get, b::UPDATES)
        .route_with_body(
            Verb::Post,
            b::UPDATE,
            Action::AgentBuilderCompile,
            "Queue the agent configuration's update merge request ({\"adapter\"?}: another base model, the generation migration): planned now, then the job agent.update_merge_request writes the builder bot's commit on agent-builder/<lock digest> and opens the merge request for the caller (202; queued=false with reason already_open and the merge request's iid, or already_asked). 409 FG-AGENT-0016 reason=closed_by_person when a person closed the same update's merge request. forge/agent-lock verifies it and auto-merge waits for a Maintainer's approval. A person only, Developer; 409 FG-AGENT-0015 when there is nothing to update, 422 FG-AGENT-0008 when the update cannot be built",
            b::MAX_BODY,
            b::UPDATE_OF,
            b::update,
        )
        .responds_accepted::<b::UpdateQueuedBody>(Verb::Post, b::UPDATE)
}

/// A project's wiki (plan 20 W2, ADR-0040 §5): create and link, unlink;
/// read the link (plan 20 W4).
fn wiki_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::wiki as k;
    registry
        .route_with(
            Verb::Get,
            k::ONE,
            Action::ProjectRead,
            "The project's linked wiki (or, asked on a wiki, that wiki) when the caller may also read the wiki project: {wiki_project_id, wiki_path, display_name, default_branch}; 404 FG-REPO-0005 without a wiki or when the wiki is not readable (never leaked)",
            k::WikiOwner,
            k::read,
        )
        .responds::<k::WikiLinkBody>(Verb::Get, k::ONE)
        .route_with_body(
            Verb::Post,
            k::ONE,
            Action::ProjectWikiLink,
            "Create the project's wiki and link it in one step ({\"path\"?, \"display_name\"?}): an ordinary project <path>-wiki in the same namespace (also ProjectCreate there), its first commit on main (.forge/doctypes/wiki.page.yml, WIKI-0001.md), main protected (Developer may push, no force push), then the link; the owner project's members inherit their role on it (one way, one hop). People only, Maintainer of the owner; 409 FG-KNOW-0003 refused (already linked, chain, name taken), 500 FG-KNOW-0004 when a step after the creation failed (no link written)",
            k::MAX_BODY,
            k::WikiOwner,
            k::create,
        )
        .responds_created::<k::WikiBody>(Verb::Post, k::ONE)
        .route_with(
            Verb::Delete,
            k::ONE,
            Action::ProjectWikiLink,
            "Unlink the project's wiki: the wiki project and its history stay, the owner's members lose the inherited role (people only, Maintainer of the owner); 409 FG-KNOW-0003 reason=not_linked without a wiki",
            k::WikiOwner,
            k::unlink,
        )
        .responds::<k::WikiUnlinkedBody>(Verb::Delete, k::ONE)
}

/// The deterministic autofix (M4 C14, ADR-0017 Amendment C14): a
/// project's switch. The merge request's own line is in
/// `merge_request_routes`; the bot itself has no route (it is decided in
/// its own arm of `authorize()`, `RepoPush` on `Resource::Autofix`).
fn autofix_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::autofix as f;
    registry
        .route_with(
            Verb::Get,
            f::ONE,
            Action::ProjectRead,
            "A project's deterministic autofix switch (off by default), who changed it last and when; a hint whether the caller may change it",
            f::AutofixOf,
            f::read,
        )
        .responds::<f::AutofixBody>(Verb::Get, f::ONE)
        .route_with_body(
            Verb::Put,
            f::ONE,
            Action::AutofixSettingsManage,
            "Turn a project's deterministic autofix on or off (people only; Maintainer; the caller becomes the enabler the bot's pushes are decided with); audited",
            f::MAX_BODY,
            f::AutofixOf,
            f::put,
        )
        .responds::<f::AutofixBody>(Verb::Put, f::ONE)
}

/// A project's `ai_assets.enabled` (plan 17 P17-3, ADR-0035).
fn ai_assets_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::ai_assets as a;
    registry
        .route_with(
            Verb::Get,
            a::ONE,
            Action::ProjectRead,
            "A project's ai_assets.enabled: whether the AI asset lint, the forge/risk rules ai_case_raw_data (stop) and ai_asset (warn) and asset releases apply (off by default), who changed it last and when",
            a::AiAssetsOf,
            a::read,
        )
        .responds::<a::AiAssetsBody>(Verb::Get, a::ONE)
        .route_with_body(
            Verb::Put,
            a::ONE,
            Action::AiAssetsSettingManage,
            "Turn a project's ai_assets.enabled on or off (people only; Maintainer and above in a web session, never an agent, a bot, a service account or the auto-merge principal); audited. Off stops the lint, the two rules and new releases only",
            a::MAX_BODY,
            a::AiAssetsOf,
            a::put,
        )
        .responds::<a::AiAssetsBody>(Verb::Put, a::ONE)
}

/// The local LLM's settings (plan 19 §8.2): a project's `llm.review` and
/// the gateway's pause. The rerun of a merge request's first pass is in
/// `merge_request_routes` (next to its read).
fn llm_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::llm as l;
    registry
        .route_with(
            Verb::Get,
            l::ONE,
            Action::ProjectRead,
            "A project's llm.review: off (the default: no AI first pass), advisory (the local LLM's first pass of each open merge request as advice, never a failure) or required (critical / high findings make forge/ai-review a failure, which blocks a merge only if a branch rule names it); who changed it last and when",
            l::LlmOf,
            l::read,
        )
        .responds::<l::LlmSettingBody>(Verb::Get, l::ONE)
        .route_with_body(
            Verb::Put,
            l::ONE,
            Action::LlmSettingsManage,
            "Set a project's llm.review ({\"review\": \"off\" | \"advisory\" | \"required\"}; people only: Maintainer and above in a web session, never a token, an agent, a bot, a service account or the auto-merge principal); audited. Off removes no advice or run",
            l::MAX_BODY,
            l::LlmOf,
            l::put,
        )
        .responds::<l::LlmSettingBody>(Verb::Put, l::ONE)
        .route(
            Verb::Post,
            l::PAUSE,
            Action::LlmPause,
            "Pause the local LLM gateway (people only; an administrator, a token needs admin; never an agent's token): no new inference until resumed, what runs finishes; the worker follows within 10 s; audited. In production only as an approved runbook step (llm-gateway)",
            l::pause,
        )
        .responds::<l::LlmGatewayBody>(Verb::Post, l::PAUSE)
        .route(
            Verb::Post,
            l::RESUME,
            Action::LlmResume,
            "Resume the paused local LLM gateway (people only; an administrator, a token needs admin); the worker follows within 10 s (llm.paused in the config still pauses it); audited",
            l::resume,
        )
        .responds::<l::LlmGatewayBody>(Verb::Post, l::RESUME)
}

/// Auto-merge (M4 C9, ADR-0017 Amendment 2026-11-13): a project's
/// `merge.auto`, its emergency stop and the instance's. The merge request's
/// own state is in `merge_request_routes`.
fn auto_merge_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::auto_merge as a;
    registry
        .route_with(
            Verb::Get,
            a::ONE,
            Action::MergeQueueRead,
            "A project's auto-merge: its merge.auto (off, reserve, conditional, full; a new project is full, projects older than auto-merge start off), the project's and the instance's emergency stop with who and when (as the repository browse API)",
            a::AutoMergeOf,
            a::read,
        )
        .responds::<a::AutoMergeBody>(Verb::Get, a::ONE)
        .route_with_body(
            Verb::Put,
            a::ONE,
            Action::AutoMergeSettingsManage,
            "Change a project's merge.auto (people only; the Owner in a web session); audited",
            a::MAX_BODY,
            a::AutoMergeOf,
            a::put,
        )
        .responds::<a::AutoMergeBody>(Verb::Put, a::ONE)
        .route_with(
            Verb::Post,
            a::STOP,
            Action::AutoMergeStopToggle,
            "Emergency stop of the project's auto-merge: nothing is added to the merge queue automatically and queued auto-merge entries wait; people still merge (people only; Maintainer); audited",
            a::AutoMergeOf,
            a::stop,
        )
        .responds::<a::AutoMergeBody>(Verb::Post, a::STOP)
        .route_with(
            Verb::Post,
            a::RESUME,
            Action::AutoMergeStopToggle,
            "Resume the project's auto-merge after an emergency stop (people only; Maintainer); audited",
            a::AutoMergeOf,
            a::resume,
        )
        .responds::<a::AutoMergeBody>(Verb::Post, a::RESUME)
        .route(
            Verb::Post,
            a::ADMIN_STOP,
            Action::AutoMergeInstanceStopToggle,
            "Emergency stop of auto-merge on the whole instance (people only; an administrator, a token needs admin); audited",
            a::instance_stop,
        )
        .responds::<a::InstanceAutoMergeBody>(Verb::Post, a::ADMIN_STOP)
        .route(
            Verb::Post,
            a::ADMIN_RESUME,
            Action::AutoMergeInstanceStopToggle,
            "Resume auto-merge on the instance (people only; an administrator, a token needs admin); audited",
            a::instance_resume,
        )
        .responds::<a::InstanceAutoMergeBody>(Verb::Post, a::ADMIN_RESUME)
        .route_with_body(
            Verb::Put,
            a::ADMIN_FULL_AUTO,
            Action::AutoMergeFullAutoManage,
            "The instance's merge.allow_full_auto ({\"allowed\": bool}): while not allowed, no project can choose full (FG-REVIEW-0032) and projects set to full merge only by a person (people only; an administrator's web session); audited",
            a::MAX_BODY,
            crate::routes::runners::OnInstance,
            a::put_full_auto,
        )
        .responds::<a::FullAutoBody>(Verb::Put, a::ADMIN_FULL_AUTO)
}

/// M4 C12: a project's pipeline schedules and the start of a manual job.
fn schedule_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::pipeline_schedules as s;
    use routes::pipelines as p;
    registry
        .route_with(
            Verb::Post,
            p::PLAY,
            Action::PipelineCreate,
            "Start a `when: manual` job: 200 the pipeline (the job pending for a runner, a finished pipeline running again); 404 FG-REPO-0005 without the pipeline or job; 409 FG-CI-0003 operation=play when the job is not waiting for a person (not_manual), the pipeline is cancelled, invalid or error (pipeline_state), or a job it needs has not succeeded (needs_not_succeeded). Secrets reach it only as its pipeline allows (Developer)",
            p::OF_PLAY,
            p::play,
        )
        .responds::<p::PipelineBody>(Verb::Post, p::PLAY)
        .route_with(
            Verb::Get,
            s::LIST,
            Action::PipelineRead,
            "A project's pipeline schedules (at most 20): branch, cron and UTC offset, active, owner (whose rights it runs with), next time, last run and its outcome (project readers)",
            s::OF_LIST,
            s::list,
        )
        .responds::<s::PipelineScheduleList>(Verb::Get, s::LIST)
        .route_with_body(
            Verb::Post,
            s::LIST,
            Action::PipelineScheduleManage,
            "Create a pipeline schedule ({\"description\", \"ref\", \"cron\", \"utc_offset_minutes\", \"active\"?}): 201; the caller becomes its owner and it runs with the owner's rights at each run, never with secrets; 400 FG-WEB-0006 (param, reason) for a bad value; 409 FG-CI-0027 past 20 per project (people only; Maintainer)",
            s::MAX_BODY,
            s::OF_LIST,
            s::create,
        )
        .responds::<s::PipelineScheduleBody>(Verb::Post, s::LIST)
        .route_with_body(
            Verb::Put,
            s::ONE,
            Action::PipelineScheduleManage,
            "Replace a pipeline schedule's settings (the same body): 200; the caller becomes its owner when the branch, rule or offset changes or after the owner was refused (a description or pause/resume edit keeps the owner); 404 FG-REPO-0005 without it (people only; Maintainer)",
            s::MAX_BODY,
            s::OF_ONE,
            s::update,
        )
        .responds::<s::PipelineScheduleBody>(Verb::Put, s::ONE)
        .route_with(
            Verb::Delete,
            s::ONE,
            Action::PipelineScheduleManage,
            "Delete a pipeline schedule: 204; 404 FG-REPO-0005 without it (people only; Maintainer)",
            s::OF_ONE,
            s::delete,
        )
        .responds_no_content(Verb::Delete, s::ONE)
}

/// The PFS portal's home widgets (ADR-0036): `PortalWidgetRead` with the
/// portal's credential only; every project and merge request shown is
/// decided again for the person (`routes::portal_widgets`).
fn portal_widget_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::portal_widgets as p;
    registry
        .route_with(
            Verb::Get,
            p::MY_REVIEWS,
            Action::PortalWidgetRead,
            "Portal widget my-reviews (list): merge requests waiting for the person's review, their own, stopped auto-merges or specs awaiting their approval (PFS portal contract v1)",
            p::PortalWidgets,
            p::my_reviews,
        )
        .queries(Verb::Get, p::MY_REVIEWS, p::MY_REVIEWS_QUERY)
        .route_with(
            Verb::Get,
            p::MY_REVIEWS_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget my-reviews: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::PIPELINES,
            Action::PortalWidgetRead,
            "Portal widget pipelines (status): the main pipeline or the last deploy of each chosen project, at most 24",
            p::PortalWidgets,
            p::pipelines,
        )
        .queries(Verb::Get, p::PIPELINES, p::PIPELINES_QUERY)
        .route_with(
            Verb::Get,
            p::PIPELINES_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget pipelines: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::STOPPED_COUNT,
            Action::PortalWidgetRead,
            "Portal widget stopped-count (kpi): the person's merge requests whose auto-merge stopped (the standard is 0)",
            p::PortalWidgets,
            p::stopped_count,
        )
        .queries(Verb::Get, p::STOPPED_COUNT, p::STOPPED_COUNT_QUERY)
        .route_with(
            Verb::Get,
            p::ATTENTION_COUNT,
            Action::PortalWidgetRead,
            "Portal widget attention-count (kpi): what waits for the person - reviews, stopped changes, specs awaiting approval, or projects whose main build failed (the standard is 0), or the deployments the person can approve now (scope=deploy)",
            p::PortalWidgets,
            p::attention_count,
        )
        .queries(Verb::Get, p::ATTENTION_COUNT, p::ATTENTION_COUNT_QUERY)
        .route_with(
            Verb::Get,
            p::ATTENTION_COUNT_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget attention-count: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::INBOX,
            Action::PortalWidgetRead,
            "Portal widget inbox (kpi): the person's unread notifications they may see now",
            p::PortalWidgets,
            p::inbox,
        )
        .queries(Verb::Get, p::INBOX, p::INBOX_QUERY)
        .route_with(
            Verb::Get,
            p::MENU,
            Action::PortalWidgetRead,
            "Portal widget menu (links): the links of a purpose (developer, reviewer, spec, operations, manager), a chosen project's screens when the person may read it",
            p::PortalWidgets,
            p::menu,
        )
        .queries(Verb::Get, p::MENU, p::MENU_QUERY)
        .route_with(
            Verb::Get,
            p::MENU_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget menu: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::MERGE_QUEUE,
            Action::PortalWidgetRead,
            "Portal widget merge-queue (status): the merge queues of the chosen projects the person may read - removed changes, paused, waiting or running, at most 24",
            p::PortalWidgets,
            p::merge_queue,
        )
        .queries(Verb::Get, p::MERGE_QUEUE, p::MERGE_QUEUE_QUERY)
        .route_with(
            Verb::Get,
            p::MERGE_QUEUE_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget merge-queue: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::SPEC_STATUS,
            Action::PortalWidgetRead,
            "Portal widget spec-status (status): per specification project the person may read - changes awaiting approval, undecided items or check findings, at most 24",
            p::PortalWidgets,
            p::spec_status,
        )
        .queries(Verb::Get, p::SPEC_STATUS, p::SPEC_STATUS_QUERY)
        .route_with(
            Verb::Get,
            p::SPEC_STATUS_OPTIONS,
            Action::PortalWidgetRead,
            "Portal widget spec-status: the project options (projects the person holds a role on and may read, at most 200)",
            p::PortalWidgets,
            p::project_options,
        )
        .route_with(
            Verb::Get,
            p::DEPLOY_APPROVALS,
            Action::PortalWidgetRead,
            "Portal widget deploy-approvals (list): the deployments waiting for an approval the person can give now (decided as the approval route decides: approvers, selectors, delegation), oldest first, with an active freeze shown",
            p::PortalWidgets,
            p::deploy_approvals,
        )
        .queries(Verb::Get, p::DEPLOY_APPROVALS, p::DEPLOY_APPROVALS_QUERY)
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
        // Plan 19 L5-7b: a stored incident diagnosis (`forge diagnose`).
        .route_with(
            Verb::Get,
            routes::diagnoses::ONE,
            Action::OpsDiagnosisRead,
            "A stored incident first diagnosis (forge diagnose): the local LLM's advice, labelled with its model, prompt version and time - at most three cause candidates, each citing the fact it comes from and a read-only command to look at next; never a step that changes anything. A confidential diagnosis is withheld from an agent (state withheld). 404 FG-REPO-0005 without such a diagnosis or for any other advice (administrators; a token needs admin)",
            r::OnInstance,
            routes::diagnoses::one,
        )
        .responds::<routes::diagnoses::DiagnosisBody>(Verb::Get, routes::diagnoses::ONE)
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
        // M4 C6: a job's artifacts, with the job's own token.
        .route_with(
            Verb::Put,
            routes::ci_artifacts::UPLOAD,
            Action::JobArtifactUpload,
            "Upload one file of a job's artifacts (the raw bytes; ?path= the file's path in the work tree) with the job's own token fgj_..., while the job runs: 201 {id, path, size_bytes, sha256}; 413 FG-CI-0019 past 100 MiB per file or 512 MiB / 1000 files per attempt; 409 FG-CI-0004 once the job ended; 400 FG-WEB-0006 for a bad or repeated path",
            routes::ci_artifacts::OF_UPLOAD,
            routes::ci_artifacts::upload,
        )
        .queries(Verb::Put, routes::ci_artifacts::UPLOAD, routes::ci_artifacts::UPLOAD_QUERY)
        .responds_created::<routes::ci_artifacts::UploadedArtifactBody>(
            Verb::Put,
            routes::ci_artifacts::UPLOAD,
        )
        .route_with(
            Verb::Get,
            routes::ci_artifacts::NEEDS,
            Action::JobArtifactDownload,
            "The artifacts of the jobs a running job needs (their current, succeeded attempt, same pipeline), with the job's own token fgj_...",
            routes::ci_artifacts::OF_NEEDS,
            routes::ci_artifacts::needs,
        )
        .responds::<routes::ci_artifacts::ArtifactList>(Verb::Get, routes::ci_artifacts::NEEDS)
        .route_with(
            Verb::Get,
            routes::ci_artifacts::NEED_ONE,
            Action::JobArtifactDownload,
            "One artifact of a job's needs: its bytes (application/octet-stream), with the job's own token fgj_...; 404 FG-REPO-0005 for any other artifact",
            routes::ci_artifacts::OF_NEEDS,
            routes::ci_artifacts::need_one,
        )
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

/// Deployments (M4 C11, plan 14 §4.2, ADR-0031): environments, targets,
/// requests and approvals.
fn deploy_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::deploy as d;
    ai_asset_release_routes(registry)
        .route_with(
            Verb::Get,
            d::ENVIRONMENTS,
            Action::DeployRead,
            "A project's deployment environments (dev, stg, prod) with their approval rule, approval window, freeze windows (and whether one is in force now), automatic rollback and deployment, and their targets with the pinned host key fingerprint; never a key (project readers)",
            d::OF_PROJECT,
            d::environments,
        )
        .responds::<d::EnvironmentList>(Verb::Get, d::ENVIRONMENTS)
        .route_with_body(
            Verb::Put,
            d::ENVIRONMENT,
            Action::DeployEnvironmentManage,
            "Create or change a deployment environment ({\"requires_approval\", \"auto_rollback\", \"approval_ttl_minutes\"?, \"auto_deploy_ref\"?, \"freeze_windows\"?}): 201 created or 200 changed; prod always requires an approval and never rolls back or deploys by itself (400 FG-WEB-0006); audited (people only; Maintainer)",
            d::MAX_BODY,
            d::OF_PROJECT,
            d::put_environment,
        )
        .responds::<d::EnvironmentBody>(Verb::Put, d::ENVIRONMENT)
        .route_with_body(
            Verb::Put,
            d::TARGET,
            Action::DeployTargetManage,
            "Register or change a deployment target ({\"host\", \"port\"?, \"user\", \"host_key_type\", \"host_key\", \"script\", \"enabled\"?}): the host key is pinned (StrictHostKeyChecking=yes against it alone) and only the named script runs on the host; 404 FG-REPO-0005 without the environment; audited (people only; administrators)",
            d::MAX_BODY,
            d::OF_PROJECT,
            d::put_target,
        )
        .responds::<d::TargetBody>(Verb::Put, d::TARGET)
        .route_with(
            Verb::Get,
            d::DEPLOYMENTS,
            Action::DeployRead,
            "A project's deployments, newest first (at most 100): environment, sha, status, who requested and approved, the previous version, error code and failed target and as_of (project readers; also acting for a person: kanflow's tool, without the commit author's name and address)",
            d::OF_DEPLOYMENTS,
            d::deployments,
        )
        .responds::<d::DeploymentList>(Verb::Get, d::DEPLOYMENTS)
        .route_with_body(
            Verb::Post,
            d::DEPLOYMENTS,
            Action::DeployCreate,
            "Request a deployment ({\"environment\": \"stg\", \"sha\"}): 201 pending_approval (the environment requires an approval; always for prod) or queued; 409 FG-DEPLOY-0002 in a freeze window; 422 FG-DEPLOY-0011 unless a protected branch holds the commit (prod: and its push pipeline succeeded); 404 FG-REPO-0005 without the environment; 400 FG-WEB-0006 without an enabled target (Developer)",
            d::MAX_BODY,
            d::OF_PROJECT,
            d::request,
        )
        .responds_created::<d::DeploymentBody>(Verb::Post, d::DEPLOYMENTS)
        .route_with(
            Verb::Post,
            d::APPROVE,
            Action::DeployApprove,
            "Approve a deployment waiting for one: 200 queued until approval_expires_at once the environment's approvals_required is reached (pending_approval with approvals until then); 403 FG-DEPLOY-0009 for its requester or anyone behind the sha (merge request authors, pushers, the commit author and committer); 403 FG-DEPLOY-0014 when the environment names approvers and the caller is not one; ?on_behalf_of= for a delegator who holds the role the approval needs (403 FG-AUTH-0003 otherwise; 403 FG-REVIEW-0033 without an active delegation, told only to a Reporter or above); a web session only (403 FG-AUTH-0002 for a token); 409 FG-DEPLOY-0010 when it is not waiting (people only; Maintainer, or a named approver)",
            d::OF_APPROVAL,
            d::approve,
        )
        .responds::<d::DeploymentBody>(Verb::Post, d::APPROVE)
        .queries(Verb::Post, d::APPROVE, d::APPROVE_QUERY)
        // M5 L5-8 (plan 19 §3.4): the approval's facts and the LLM's summary.
        .route_with(
            Verb::Get,
            d::APPROVAL_SUMMARY,
            Action::DeployRead,
            "A deployment's approval facts, made by code and always listed in full (the merge requests since the environment's last succeeded deployment, migrations, protected paths, CI and deploy configuration, permission code, forge/risk, failed checks, freeze windows), and apart from them the local LLM's summary as advice: state none (the LLM is off, down or has not answered), shown, or withheld (a confidential summary read by an agent); stale when made for other facts or a newer commit was requested. Approving never depends on it; 404 FG-REPO-0005 without the deployment; an act-as request is refused (project readers)",
            d::OF_PROJECT,
            d::approval_summary,
        )
        .responds::<d::ApprovalSummaryBody>(Verb::Get, d::APPROVAL_SUMMARY)
        // ADR-0038 §3.4–§3.6 (step C): the environments' approvers.
        .route_with(
            Verb::Get,
            d::APPROVERS,
            Action::DeployRead,
            "Each deployment environment's approvers: named users, groups, organisation selectors, how many approvals a deployment needs and whether a delegate may approve; unrestricted = any Maintainer (project readers)",
            d::OF_PROJECT,
            d::approvers,
        )
        .responds::<d::ApproversList>(Verb::Get, d::APPROVERS)
        .route_with_body(
            Verb::Put,
            d::ENVIRONMENT_APPROVERS,
            Action::DeployApproversManage,
            "Set who may approve an environment's deployments ({\"users\", \"groups\", \"selectors\", \"approvals_required\", \"allow_delegation\"?}): named users approve from Reporter, group members and people a selector finds from Maintainer; nobody else once any is named (403 FG-DEPLOY-0014); 404 FG-REPO-0005 without the environment; 400 FG-WEB-0006 for an unknown user or group, an invalid selector or too many; audited (people only; Owner)",
            d::MAX_BODY,
            d::OF_PROJECT,
            d::put_approvers,
        )
        .responds::<d::ApproversBody>(Verb::Put, d::ENVIRONMENT_APPROVERS)
}

/// Plan 17 P17-7 (ADR-0031 Amendment P17-7): signed `pfs-ai-bundle`
/// releases, their stage deployments and the rollback.
fn ai_asset_release_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::ai_asset_releases as r;
    registry
        .route_with(
            Verb::Get,
            r::RELEASES,
            Action::DeployRead,
            "A project's pfs-ai-bundle releases, newest first (at most 100): version, tag, the pinned commit, digest, key and signature, without the manifest; each with its withdrawal, a revoked key, its state on shadow / canary / prod and the stage a request would be accepted on now; per stage the running release and the rollback target; and what the caller may press (project readers)",
            r::OF_PROJECT,
            r::list,
        )
        .responds::<r::ReleaseList>(Verb::Get, r::RELEASES)
        .route_with_body(
            Verb::Post,
            r::RELEASES,
            Action::AiAssetReleaseCreate,
            "Make a signed pfs-ai-bundle release from a tag ({\"tag\": \"assets/v2026.48.1\"}): the commit the tag names now is pinned (a moved tag has no effect), it must be on a protected branch with a green forge/ai-eval bench, its skills/playbooks/agents/tools/models/bench files are linted and listed with their SHA-256 in a manifest signed with the agent signing key; 201; 422 FG-AIASSET-0008 (ai_assets disabled, tag, already released, bench, lint); audited (people only; Maintainer, a session or an api token)",
            r::MAX_BODY,
            r::OF_PROJECT,
            r::create,
        )
        .responds_created::<r::ReleaseBody>(Verb::Post, r::RELEASES)
        .route_with(
            Verb::Get,
            r::RELEASE,
            Action::DeployRead,
            "One pfs-ai-bundle release with its signed manifest (files and SHA-256, models, bench), digest, key id and signature: what the PFS loader verifies; 404 FG-REPO-0005 without it (project readers)",
            r::OF_PROJECT,
            r::one,
        )
        .responds::<r::ReleaseBody>(Verb::Get, r::RELEASE)
        .route_with_body(
            Verb::Post,
            r::RELEASE_DEPLOYMENTS,
            Action::DeployCreate,
            "Request a release on a stage ({\"environment\": \"pfs-ai-shadow\" | \"pfs-ai-canary\" | \"pfs-ai-prod\"}, the dev / stg / prod environments): 201 pending_approval (every stage needs a person's approval in a session); 409 FG-AIASSET-0009 out of stage order (shadow, canary, prod), without the release or on an environment that needs no approval; 422 FG-DEPLOY-0011 off the protected branches (Developer)",
            r::MAX_BODY,
            r::OF_PROJECT,
            r::request_deployment,
        )
        .responds_created::<routes::deploy::DeploymentBody>(Verb::Post, r::RELEASE_DEPLOYMENTS)
        .route_with_body(
            Verb::Post,
            r::ROLLBACKS,
            Action::DeployCreate,
            "Roll a stage back ({\"environment\"}): 201 a deployment of the release last approved before the running one (older by release order, succeeded there with an approval), waiting for a person's approval; 409 FG-AIASSET-0009 no_rollback_target (Developer)",
            r::MAX_BODY,
            r::OF_PROJECT,
            r::rollback,
        )
        .responds_created::<r::RollbackBody>(Verb::Post, r::ROLLBACKS)
        .route_with_body(
            Verb::Post,
            r::WITHDRAWAL,
            Action::AiAssetReleaseWithdraw,
            "Mark a bad pfs-ai-bundle release withdrawn, for good, with a reason ({\"reason\"}: 1 to 500 characters, one line): it is never requested on a stage, approved, run or gone back to again (a fixed bundle is a new release); 201; 409 FG-AIASSET-0010 (no such release, withdrawn already, the reason); audited (people only; Maintainer, a session or an api token)",
            r::MAX_WITHDRAW_BODY,
            r::OF_PROJECT,
            r::withdraw,
        )
        .responds_created::<r::WithdrawalBody>(Verb::Post, r::WITHDRAWAL)
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
            "A project's CI pipelines, newest first (ref, before, limit 1..=100, default 20): source, ref, sha, status, error code and problems of an invalid definition, the confidential mark at the commit and as_of (project readers, also acting for a person: kanflow's tool; metadata only)",
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
            "One CI pipeline with its jobs (name, stage, wave, needs, image, timeout, tags, secret names, status, attempt) and as_of; 404 FG-REPO-0005 when the project has no such pipeline (project readers, also acting for a person: kanflow's tool; metadata only)",
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
        // M4 C6: a job's artifacts, decided like its log.
        .route_with(
            Verb::Get,
            p::ARTIFACTS,
            Action::CiArtifactRead,
            "A CI job's artifacts (every attempt's): path, size, sha256, attempt, keep, expiry (30 days unless kept for a deployment); 404 FG-REPO-0005 without such a pipeline or job; 403 FG-AUTH-0022 to an AI agent audience unless the pipeline's commit and the default branch's tip are both measured open (project readers)",
            p::OF_ARTIFACTS,
            p::artifacts,
        )
        .responds::<routes::ci_artifacts::ArtifactList>(Verb::Get, p::ARTIFACTS)
        .route_with(
            Verb::Get,
            p::ARTIFACT,
            Action::CiArtifactRead,
            "One CI artifact's bytes (application/octet-stream, attachment, X-Forge-Sha256); 404 FG-REPO-0005 without it (or expired); 403 FG-AUTH-0022 as for the list (project readers)",
            p::OF_ARTIFACT,
            p::artifact,
        )
        // M4 C6: a project's CI variables (secrets; ADR-0029 decision 8).
        .route_with(
            Verb::Get,
            routes::ci_variables::LIST,
            Action::CiVariableManage,
            "A project's CI variables: name, protected_only, masked, value length, who set them and when; never a value (people only; Maintainer)",
            routes::ci_variables::OF_LIST,
            routes::ci_variables::list,
        )
        .responds::<routes::ci_variables::CiVariableList>(Verb::Get, routes::ci_variables::LIST)
        .route_with_body(
            Verb::Put,
            routes::ci_variables::ONE,
            Action::CiVariableManage,
            "Set a CI variable ({\"value\", \"protected_only\"?, \"masked\"?}, both default true): 201 created or 200 replaced, the metadata only (the value is never answered again); 400 FG-WEB-0006 for a bad name or value (param and reason, never the value); 503 FG-CI-0016 without a usable ci.secret_keys_file. Values reach only jobs of push pipelines of protected branches (people only; Maintainer)",
            routes::ci_variables::MAX_BODY,
            routes::ci_variables::OF_ONE,
            routes::ci_variables::put,
        )
        .responds::<routes::ci_variables::CiVariableBody>(Verb::Put, routes::ci_variables::ONE)
        .route_with(
            Verb::Delete,
            routes::ci_variables::ONE,
            Action::CiVariableManage,
            "Delete a CI variable: 204; 404 FG-REPO-0005 without it (people only; Maintainer)",
            routes::ci_variables::OF_ONE,
            routes::ci_variables::delete,
        )
        .responds_no_content(Verb::Delete, routes::ci_variables::ONE)
}

/// The merge queue (M4 C8, ADR-0030): a target's queue, its emergency
/// stop and its restart. The merge request's own routes (enqueue, dequeue,
/// its entry) are in `merge_request_routes`.
fn merge_queue_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::merge_queue as q;
    registry
        .route_with(
            Verb::Get,
            q::QUEUE,
            Action::MergeQueueRead,
            "A target branch's merge queue (target required): paused or not, the next batch bound, the batch being tested (base, train, deadline), the live entries in order and the latest merged or removed ones with their reason and code (as the repository browse API)",
            q::QueueOf,
            q::read,
        )
        .responds::<q::MergeQueueBody>(Verb::Get, q::QUEUE)
        .queries(Verb::Get, q::QUEUE, q::QUEUE_QUERY)
        .route_with_body(
            Verb::Post,
            q::PAUSE,
            Action::MergeQueuePause,
            "Pause a target branch's merge queue (the emergency stop): no new batch is built and no batch lands until it is resumed; merge requests may still be added (people only; Developer)",
            q::MAX_BODY,
            q::QueueOf,
            q::pause,
        )
        .responds::<q::MergeQueueBody>(Verb::Post, q::PAUSE)
        .route_with_body(
            Verb::Post,
            q::RESUME,
            Action::MergeQueueManage,
            "Resume a paused merge queue (people only; Maintainer)",
            q::MAX_BODY,
            q::QueueOf,
            q::resume,
        )
        .responds::<q::MergeQueueBody>(Verb::Post, q::RESUME)
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
    use routes::approval_delegation as ad;
    use routes::{
        directory as d, org_profiles as o, ssh_keys as k, tokens as t, user_sessions as s,
    };
    registry
        // ADR-0038 §3.5 (step C): the caller's delegation of approvals.
        .route_with(
            Verb::Get,
            ad::PATH,
            Action::ApprovalDelegationRead,
            "The caller's delegation of approvals (who approves for them and when; null without one) and the people who delegate to the caller and whose period has not ended",
            ad::OwnDelegation,
            ad::get,
        )
        .responds::<ad::DelegationState>(Verb::Get, ad::PATH)
        .route_with_body(
            Verb::Put,
            ad::PATH,
            Action::ApprovalDelegationManage,
            "Set the caller's delegation ({\"delegate\", \"starts_at\", \"ends_at\"}: an active person, not oneself, at most 30 days; replaces any): the delegate may approve merge requests for the caller, and deployments where the environment allows it (web sessions only; audited)",
            ad::MAX_BODY,
            ad::OwnDelegation,
            ad::put,
        )
        .responds::<ad::DelegationBody>(Verb::Put, ad::PATH)
        .route_with(
            Verb::Delete,
            ad::PATH,
            Action::ApprovalDelegationManage,
            "Remove the caller's delegation of approvals (204, also when there was none; web sessions only; audited)",
            ad::OwnDelegation,
            ad::delete,
        )
        .responds_no_content(Verb::Delete, ad::PATH)
        // M4 C13 (ADR-0033 §2): the caller's SSH keys.
        .route_with(
            Verb::Get,
            k::COLLECTION,
            Action::SshKeyList,
            "The caller's SSH public keys and the SSH server's host and host key fingerprints",
            k::OwnSshKeys,
            k::list,
        )
        .responds::<k::SshKeyList>(Verb::Get, k::COLLECTION)
        .route_with_body(
            Verb::Post,
            k::COLLECTION,
            Action::SshKeyAdd,
            "Add an SSH public key for the caller (web sessions only; Ed25519 / ECDSA)",
            k::MAX_BODY,
            k::OwnSshKeys,
            k::add,
        )
        .responds_created::<k::SshKeyBody>(Verb::Post, k::COLLECTION)
        .route_with(
            Verb::Delete,
            k::ONE,
            Action::SshKeyDelete,
            "Delete one of the caller's SSH keys",
            k::SshKeyToDelete,
            k::delete,
        )
        .responds_no_content(Verb::Delete, k::ONE)
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
        // ADR-0021 amendment 2026-11-20: the org claims' snapshot.
        .route(
            Verb::Get,
            o::OWN,
            Action::UserSelfRead,
            "The caller's own org claims snapshot (departments, positions, rank, plants)",
            o::own,
        )
        .responds::<o::UserOrgProfile>(Verb::Get, o::OWN)
        .route_with(
            Verb::Get,
            o::ADMIN,
            Action::UserOrgAdminRead,
            "A user's org claims snapshot (administrators, people only)",
            o::OrgProfileTarget,
            o::admin,
        )
        .responds::<o::UserOrgProfile>(Verb::Get, o::ADMIN)
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
        // Simple editor E1 (2026-11-12): one text file committed like a
        // push (the project-level RepoPush here, the ref decided by
        // authorize_ref_updates on the push's protection snapshot).
        .route_with_body(
            Verb::Post,
            routes::file_commit::PATH,
            Action::RepoPush,
            "Create or replace one text file (UTF-8, no NUL, at most 512 KiB) and commit it like a push: on a new branch (create_branch; by default a merge request into branch is opened) or directly on branch when the push rules allow it (protected-ref rules, direct-push ban, secret scan FG-GIT-0022); start_sha is the commit the file was read at: a file changed since is 409 FG-GIT-0027, never overwritten (Developer; a token needs write_repository)",
            routes::file_commit::MAX_BODY,
            routes::file_commit::SingleFileOf,
            routes::file_commit::commit,
        )
        .responds_created::<routes::file_commit::SingleFileCommitBody>(
            Verb::Post,
            routes::file_commit::PATH,
        )
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
        // Plan 13 stage 4 part A (§7.5, ADR-0027): a document's open
        // revisions, edit claim and versions.
        .route_with(
            Verb::Get,
            routes::document_revisions::STATUS,
            Action::RepoBrowse,
            "A document's open revisions (the merge requests revising it; the earliest per language holds it) and its live edit claim",
            routes::document_revisions::DocumentRevisionOf,
            routes::document_revisions::status,
        )
        .responds::<routes::document_revisions::DocumentStatusBody>(
            Verb::Get,
            routes::document_revisions::STATUS,
        )
        .route_with(
            Verb::Get,
            routes::document_revisions::VERSIONS,
            Action::RepoBrowse,
            "A document's versions on the default branch: each applied merge that changed its blob (derived from git and the merges)",
            routes::document_revisions::DocumentRevisionOf,
            routes::document_revisions::versions,
        )
        .responds::<routes::document_revisions::DocumentVersionsBody>(
            Verb::Get,
            routes::document_revisions::VERSIONS,
        )
        .route_with_body(
            Verb::Put,
            routes::document_revisions::CLAIM,
            Action::DocumentEditClaim,
            "Take or extend the caller's edit claim on a document for 30 minutes (409 FG-DOC-0007 while another person's is live; people only; Developer)",
            routes::document_revisions::MAX_BODY,
            routes::document_revisions::DocumentRevisionOf,
            routes::document_revisions::take,
        )
        .responds::<routes::document_revisions::EditClaimTakenBody>(
            Verb::Put,
            routes::document_revisions::CLAIM,
        )
        .route_with(
            Verb::Delete,
            routes::document_revisions::CLAIM,
            Action::DocumentEditClaim,
            "Release the caller's own edit claim on a document (409 FG-DOC-0007 for another person's; 404 without a live claim; people only; Developer)",
            routes::document_revisions::DocumentRevisionOf,
            routes::document_revisions::release,
        )
        .responds_no_content(Verb::Delete, routes::document_revisions::CLAIM)
        .route_with(
            Verb::Post,
            routes::document_revisions::REVOKE,
            Action::DocumentEditClaimRevoke,
            "Revoke the live edit claim on a document, whoever holds it (audited; 404 without a live claim; people only; Maintainer)",
            routes::document_revisions::DocumentRevisionOf,
            routes::document_revisions::revoke,
        )
        .responds_no_content(Verb::Post, routes::document_revisions::REVOKE)
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

/// Org bindings (ADR-0038 §2, §5): read like the members (`MemberList`),
/// added and removed by the Owner in a web session (`OrgBindingManage`,
/// decided again in forge-hosting on the locked target), and the org
/// master's units to pick them from (`UserSearch`).
fn org_binding_routes(registry: Registry<AppState>) -> Registry<AppState> {
    use routes::org_bindings as o;
    registry
        .route_with(
            Verb::Get,
            o::PROJECT_COLLECTION,
            Action::MemberList,
            "A project's org bindings and the people they give a role to now (as the member list)",
            o::ProjectBindings,
            o::list_project,
        )
        .responds::<o::OrgBindingsBody>(Verb::Get, o::PROJECT_COLLECTION)
        .route_with_body(
            Verb::Post,
            o::PROJECT_COLLECTION,
            Action::OrgBindingManage,
            "Bind an org unit to a project as Reporter or Developer (never above; the Owner, people only, a web session); audited",
            o::MAX_BODY,
            o::ProjectBindings,
            o::create_project,
        )
        .responds_created::<o::OrgBindingBody>(Verb::Post, o::PROJECT_COLLECTION)
        .route_with(
            Verb::Delete,
            o::PROJECT_ONE,
            Action::OrgBindingManage,
            "Remove a project's org binding (the Owner, people only, a web session); audited",
            o::ProjectBinding,
            o::delete_project,
        )
        .responds_no_content(Verb::Delete, o::PROJECT_ONE)
        .route_with(
            Verb::Get,
            o::GROUP_COLLECTION,
            Action::MemberList,
            "A group's org bindings and the people they give a role to now (as the member list)",
            o::GroupBindings,
            o::list_group,
        )
        .responds::<o::OrgBindingsBody>(Verb::Get, o::GROUP_COLLECTION)
        .route_with_body(
            Verb::Post,
            o::GROUP_COLLECTION,
            Action::OrgBindingManage,
            "Bind an org unit to a group as Reporter or Developer (never above; inherited by everything below; the Owner, people only, a web session); audited",
            o::MAX_BODY,
            o::GroupBindings,
            o::create_group,
        )
        .responds_created::<o::OrgBindingBody>(Verb::Post, o::GROUP_COLLECTION)
        .route_with(
            Verb::Delete,
            o::GROUP_ONE,
            Action::OrgBindingManage,
            "Remove a group's org binding (the Owner, people only, a web session); audited",
            o::GroupBinding,
            o::delete_group,
        )
        .responds_no_content(Verb::Delete, o::GROUP_ONE)
        .route(
            Verb::Get,
            o::UNITS,
            Action::UserSearch,
            "The org master's units for the binding picker (names, level, parent; at most 50)",
            o::units,
        )
        .responds::<o::OrgUnitsBody>(Verb::Get, o::UNITS)
        .queries(Verb::Get, o::UNITS, o::UNITS_QUERY)
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
        // M5 L5-5 (plan 19 §3.1): the local LLM's first pass, read like the
        // merge request; a confidential advice is withheld from agents.
        .route_with(
            Verb::Get,
            mr::AI_REVIEW,
            Action::MergeRequestRead,
            "A merge request's AI first pass by the local LLM, as advice only (it never blocks or approves a merge): the project's llm.review (off, advisory, required), the forge/ai-review run on the current head (error when the LLM failed, never success) and the newest advice — summary, findings with their checked evidence, the deep-review mark (code rules first), model, prompt version and time; state none (no advice), shown, or withheld (a confidential advice read by an agent, an act-as request or Forge-Context-Audience: agent); stale when made for another head (as the merge request)",
            mr::AI_REVIEW_OF,
            mr::ai_review,
        )
        .responds::<mr::AiReviewBody>(Verb::Get, mr::AI_REVIEW)
        // M5 L5-6 (plan 19 §4, §8.2): a person's verdict on one finding.
        .route_with_body(
            Verb::Post,
            mr::AI_FEEDBACK,
            Action::AiAdviceFeedback,
            "Vote on one finding of a merge request's AI first pass: useful or wrong (people only: whoever reads the merge request, Reporter and above; an agent's token, a service account, a bot, act-as and Forge-Context-Audience: agent are refused, 403). One verdict per person and finding: a new one replaces theirs (outcome recorded, changed or unchanged). No such advice or finding, or an advice of another merge request: 404 FG-REPO-0005",
            mr::AI_FEEDBACK_MAX_BODY,
            mr::AI_FEEDBACK_OF,
            mr::ai_review_feedback,
        )
        .responds::<mr::AiAdviceVoteBody>(Verb::Post, mr::AI_FEEDBACK)
        // Plan 19 §8.2: asking the first pass again (3 an hour per MR).
        .route_with(
            Verb::Post,
            mr::AI_RERUN,
            Action::AiReviewRerun,
            "Ask a merge request's AI first pass again on its current head (people only: Developer and above; an agent's token, a service account, a bot, act-as and Forge-Context-Audience: agent are refused, 403): queued (the worker asks again on its next pass) or pending (the head still waits: nothing counted). Closed, without a head or llm.review off: 409 FG-LLM-0008; more than 3 in an hour per merge request: 429 FG-WEB-0010 with Retry-After",
            mr::AI_RERUN_OF,
            mr::ai_review_rerun,
        )
        .responds::<mr::AiReviewRerunBody>(Verb::Post, mr::AI_RERUN)
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
        // M4 C10 (plan 04 M-4, ADR-0024 Amendment C10): one action opens a
        // merge request reverting a merged one; decided as opening it
        // (MergeRequestCreate) and pushing its branch (RepoPush).
        .route_with(
            Verb::Post,
            mr::REVERT,
            Action::MergeRequestCreate,
            "Revert a merged merge request in one step: opens a merge request from the new branch revert/mr-<iid> (one revert commit on the target's tip) into the target. Decided as opening a merge request (Developer; a token needs api) and pushing the branch (a token needs write_repository; a protected revert/* refuses, FG-GIT-0008). 409 FG-REVIEW-0029 (not merged, branch exists, nothing to revert) or FG-REVIEW-0030 (the revert conflicts: nothing is opened)",
            mr::MergeRequestToRevert,
            mr::revert,
        )
        .responds_created::<mr::RevertBody>(Verb::Post, mr::REVERT)
        // M4 C8 (ADR-0030 §2, §10, §11): "merge when ready" — the merge
        // request in its target's merge queue.
        .route_with_body(
            Verb::Post,
            mr::MERGE_QUEUE,
            Action::MergeQueueEnqueue,
            "Add a merge request to its target branch's merge queue (merge when ready): decided as a merge (people only; the target branch's push role; a token needs merge_execute); everything a merge needs must hold now except ci/pipeline, which the queue proves on its train (FG-REVIEW-0007 otherwise); 409 FG-CI-0025 when already queued",
            mr::MERGE_QUEUE_MAX_BODY,
            mr::MergeRequestToQueue,
            mr::merge_queue_enqueue,
        )
        .responds_created::<mr::QueueEntryBody>(Verb::Post, mr::MERGE_QUEUE)
        .route_with(
            Verb::Delete,
            mr::MERGE_QUEUE,
            Action::MergeQueueDequeue,
            "Take a merge request out of the merge queue (as adding it); 409 FG-CI-0025 when it is not queued",
            mr::MergeRequestToDequeue,
            mr::merge_queue_dequeue,
        )
        .responds::<mr::QueueEntryBody>(Verb::Delete, mr::MERGE_QUEUE)
        .route_with(
            Verb::Get,
            mr::MERGE_QUEUE,
            Action::MergeRequestRead,
            "A merge request's latest merge queue entry (null: never queued) with its place among the live entries (as the repository browse API)",
            mr::QueueEntryOf,
            mr::merge_queue_entry,
        )
        .responds::<mr::QueueEntryStatusBody>(Verb::Get, mr::MERGE_QUEUE)
        // Plan 18 S5 (ADR-0037): the merge request pipeline on the current
        // target, decided as a manual pipeline run.
        .route_with(
            Verb::Post,
            mr::MR_PIPELINE,
            Action::PipelineCreate,
            "Run the merge request pipeline (ci/pipeline) on the merge result of the current target: 202 queued (its job was enqueued), exists (asked for already; a second press asks nothing more) or computing (the merge result of the current target is written first, then the pipeline runs on it). A target move alone starts no merge request pipeline (ci.mr_pipeline_on_target_move); the merge queue does not need this. 404 FG-REPO-0005 without the merge request; 409 FG-CI-0003 when it is not open, has no version or conflicts (Developer; a token needs api)",
            mr::MergeRequestToRunPipeline,
            mr::run_pipeline,
        )
        .responds_accepted::<mr::MergeRequestPipelineBody>(Verb::Post, mr::MR_PIPELINE)
        // M4 C9: what auto-merge did with the merge request (one line).
        .route_with(
            Verb::Get,
            mr::AUTO_MERGE,
            Action::MergeRequestRead,
            "What auto-merge did with a merge request: the project's mode, the emergency stop in effect, and its last state (queued, waiting, held, stopped) with the stop class, a stable reason, the forge/risk rule or check behind it and FG-REVIEW-0026 / 0027 (null: none for its current head) (as the repository browse API)",
            mr::AutoMergeStateOf,
            mr::auto_merge_state,
        )
        .responds::<mr::MergeRequestAutoMergeBody>(Verb::Get, mr::AUTO_MERGE)
        // M4 C14: what the deterministic autofix did (one line).
        .route_with(
            Verb::Get,
            mr::AUTOFIX,
            Action::MergeRequestRead,
            "What the deterministic autofix did with a merge request: whether the project's autofix is on and its latest attempt (trigger, fixer, state, a stable reason, the paths behind a refusal or proposal, the bot's commit, FG-GIT-0037 / 0038, FG-CI-0031, FG-REVIEW-0031, FG-AUTH-0024) (null: none) (as the repository browse API)",
            mr::AutofixStateOf,
            mr::autofix_state,
        )
        .responds::<mr::MergeRequestAutofixBody>(Verb::Get, mr::AUTOFIX)
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
        // Plan 15 MR 3: who may see a group (its Owner, in a session).
        .route_with_body(
            Verb::Patch,
            groups::VISIBILITY,
            Action::GroupVisibilityChange,
            "Change (or preview) a group's visibility (group Owner, web session)",
            groups::MAX_BODY,
            groups::GroupForVisibility,
            groups::change_visibility,
        )
        .responds::<groups::GroupVisibilityBody>(Verb::Patch, groups::VISIBILITY)
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
            .route(
                verb,
                ADMIN_PAGE,
                Action::WebShell,
                "SPA shell: the administrator settings (the portal menu's link)",
                shell,
            )
            // The search results (plan 20 K5): a shared or reloaded result
            // link needs the shell too.
            .route(
                verb,
                SEARCH_PAGE,
                Action::WebShell,
                "SPA shell: search results",
                shell,
            )
            // The caller's notes (plan 20 N2): the "My work" sidebar's
            // notes page, reloaded or opened from a bookmark.
            .route(
                verb,
                NOTES_PAGE,
                Action::WebShell,
                "SPA shell: the caller's notes",
                shell,
            )
            // The agent marketplace (plan 16 A8): the catalog and one
            // part, reloaded or opened from a shared link.
            .route(
                verb,
                MARKETPLACE_PAGE,
                Action::WebShell,
                "SPA shell: the agent marketplace",
                shell,
            )
            .route(
                verb,
                MARKETPLACE_PART_PAGE,
                Action::WebShell,
                "SPA shell: one agent part of the marketplace",
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
    let router = registry
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
        // ADR-0036: the widget routes' authenticator.
        .layer(axum::Extension(crate::auth::PortalExt {
            portal: state.portal.clone(), // clone: the Arc is shared
            gate: Arc::clone(&state.portal_gate),
        }))
        // ADR-0036: the widget routes answer failures in the portal's envelope.
        .layer(middleware::from_fn(routes::portal_widgets::envelope_errors))
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
        ));
    // A plain-HTTP trial server: outside everything, so every layer and
    // handler sees and issues only the `__Host-` names.
    let router = if let Some(port) = config.plain_http_port {
        router.layer(middleware::from_fn_with_state(
            crate::plain_http_cookies::PlainNames::for_port(port),
            crate::plain_http_cookies::plain_http_cookies,
        ))
    } else {
        router
    };
    Ok(router.with_state(state))
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
    // M4 C6: a job's artifact upload also reads its whole body before it
    // answers (its own idle bound and `UPLOAD_TIMEOUT`).
    let limit =
        if is_git_rpc(&req) || routes::ci_artifacts::is_upload(req.method(), req.uri().path()) {
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
/// The portal menu's administrator link (ADR-0021 amendment 2026-11-24):
/// the SPA sends the person on to their own administrator settings.
pub const ADMIN_PAGE: &str = "/-/admin";
/// The search results page (plan 20 K5; the API is `GET /api/v1/search`).
pub const SEARCH_PAGE: &str = "/-/search";
/// The "My work" sidebar's notes page (plan 20 N2; the API is
/// `GET`/`POST /api/v1/user/notes`).
pub const NOTES_PAGE: &str = "/-/notes";
/// The agent marketplace's catalog (plan 16 A8; the API is
/// `GET /api/v1/agent-parts`).
pub const MARKETPLACE_PAGE: &str = "/-/marketplace";
/// One agent part of the marketplace (plan 16 A8; the API is
/// `GET /api/v1/agent-parts/{slug}`).
pub const MARKETPLACE_PART_PAGE: &str = "/-/marketplace/{slug}";
