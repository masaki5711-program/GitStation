#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The PFS portal's widget routes (ADR-0036) through the product router
//! against the development database, with the real loaders, forge-review
//! and forge-pipelines. The portal's token check itself (JWT, JWKS, the
//! link rules) is forge-oidc's (`tests/portal.rs` there); here a stand-in
//! authenticator maps synthetic tokens to the facts it would produce.
//!
//! Covered: every widget type's shape in the contract's envelope, the
//! options, 403 without the access role / with another party / for a
//! disabled person, `FG-AUTH-0027` for an unlinked person, `FG-AUTH-0026`
//! for a bad token, a cookie, or the portal off, 422 for invalid settings
//! (unknown keys ignored), the limit, the locale, another person's and a
//! private project's rows never appearing, a portal token refused on a
//! route that is not a widget's, and the widget gate (429 with
//! `Retry-After` and `no-store`, before the token is checked). Synthetic
//! data only.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::Role;
use forge_core::portal::{PortalAuthFuture, PortalAuthenticator, PortalIdentity};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, UserId, UserKind};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter, Pipelines};
use forge_review::{BranchProbe, Review};
use forge_web::routes::portal_widgets::WidgetGate;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const REVIEWS: &str = "/api/portal-widgets/v1/my-reviews";
const REVIEW_OPTIONS: &str = "/api/portal-widgets/v1/my-reviews/options/project";
const PIPELINES: &str = "/api/portal-widgets/v1/pipelines";
const PIPELINE_OPTIONS: &str = "/api/portal-widgets/v1/pipelines/options/project";
const STOPPED: &str = "/api/portal-widgets/v1/stopped-count";
const ATTENTION: &str = "/api/portal-widgets/v1/attention-count";
const ATTENTION_OPTIONS: &str = "/api/portal-widgets/v1/attention-count/options/project";
const INBOX: &str = "/api/portal-widgets/v1/inbox";
const MENU: &str = "/api/portal-widgets/v1/menu";
const MENU_OPTIONS: &str = "/api/portal-widgets/v1/menu/options/project";

struct NoBranches;

impl BranchProbe for NoBranches {
    fn branch_head<'a>(
        &'a self,
        _repo: RepoId,
        _branch: &'a RefName,
        _caller: &'a str,
    ) -> forge_core::namespace_resolver::LookupFuture<'a, ObjectId> {
        Box::pin(async { Err(ForgeError::new(ErrorCode::Internal)) })
    }
}

struct OpenMarks;

impl MarkMeter for OpenMarks {
    fn measure<'a>(&'a self, _: RepoId, _: &'a ObjectId) -> MarkFuture<'a> {
        Box::pin(async { CopyMark::Open })
    }
}

/// What a synthetic token stands for.
#[derive(Clone)]
enum Stand {
    Is(PortalIdentity),
    Refused(ErrorCode, &'static str),
}

/// The stand-in for forge-oidc's `PortalWidgets`.
struct FakePortal(HashMap<&'static str, Stand>);

impl PortalAuthenticator for FakePortal {
    fn authenticate_portal<'a>(
        &'a self,
        _request_id: &'a RequestId,
        presented: &'a str,
    ) -> PortalAuthFuture<'a> {
        let stand = self.0.get(presented).cloned();
        Box::pin(async move {
            match stand {
                Some(Stand::Is(identity)) => Ok(identity),
                Some(Stand::Refused(code, reason)) => {
                    Err(ForgeError::new(code).with_context("reason", reason))
                }
                None => Err(ForgeError::new(ErrorCode::PortalTokenInvalid)
                    .with_context("reason", "signature")),
            }
        })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn identity(user: UserId) -> PortalIdentity {
    PortalIdentity {
        user_id: user,
        active: true,
        kind: UserKind::Human,
        portal_party: true,
        access_role: true,
    }
}

struct World {
    t: FreshDb,
    olga: UserId,
    dave: UserId,
    eve: UserId,
    web: ProjectId,
    secret: ProjectId,
    portal: Option<Arc<dyn PortalAuthenticator>>,
    /// The widget gate of every router built (`None`: the product's).
    gate: Option<Arc<WidgetGate>>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    git: Git,
    storage: StorageRoot,
}

async fn person(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

async fn project(t: &FreshDb, group: &str, owner: UserId, members: &[(UserId, Role)]) -> ProjectId {
    let g = groups::create(
        &t.db,
        &ctx(&format!("req-{group}")),
        &NewGroup {
            visibility: forge_core::Visibility::Private,
            segment: group.into(),
            parent: None,
            display_name: group.into(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group");
    let p = projects::create(
        &t.db,
        &ctx(&format!("req-{group}-web")),
        &NewProject::new("web", g.namespace_id, "Web", owner),
    )
    .await
    .expect("project")
    .project_id;
    for (who, role) in members {
        projects::add_member(&t.db, &ctx("req-member"), p, *who, *role)
            .await
            .expect("member");
    }
    p
}

fn head(n: i64) -> String {
    format!("{n:040x}")
}

/// Merge request `iid` of `project` by `author`; its id.
async fn mr(t: &FreshDb, project: ProjectId, iid: i64, author: UserId) -> i64 {
    let p = project.get();
    let counter = if iid == 1 {
        format!("INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)")
    } else {
        format!("UPDATE forge.merge_request_counters SET last_iid = {iid} WHERE project_id = {p}")
    };
    t.commit_audited(
        ("project", p),
        &[
            counter,
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, draft, head_sha) VALUES ({p}, {iid}, \
                 'Change {iid}', {}, 'refs/heads/b{iid}', 'refs/heads/main', false, '{}')",
                author.get(),
                head(iid)
            ),
        ],
    )
    .await;
    let rows = t
        .json_rows(&format!(
            "SELECT id FROM forge.merge_requests WHERE project_id = {p} AND iid = {iid}"
        ))
        .await;
    rows[0]["id"].as_i64().expect("id")
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let olga = person(&t, "olga").await;
        let dave = person(&t, "dave").await;
        let eve = person(&t, "eve").await;
        let mallory = person(&t, "mallory").await;
        let web = project(&t, "acme", olga, &[(dave, Role::Developer)]).await;
        // A private project eve and dave hold no role on.
        let secret = project(&t, "hidden", mallory, &[]).await;
        let first = mr(&t, web, 1, olga).await;
        mr(&t, secret, 1, mallory).await;
        // Auto-merge stopped on olga's merge request (current head).
        t.commit_audited(
            ("project", web.get()),
            &[format!(
                "INSERT INTO forge.auto_merge_states (merge_request_id, project_id, head_sha, \
                 state, class, reason, request_id) VALUES ({first}, {}, '{}', 'stopped', \
                 'program_breakage', 'required_check_failed', 'req-am')",
                web.get(),
                head(1)
            )],
        )
        .await;
        let mut tokens = HashMap::new();
        tokens.insert("t-olga", Stand::Is(identity(olga)));
        tokens.insert("t-dave", Stand::Is(identity(dave)));
        tokens.insert("t-eve", Stand::Is(identity(eve)));
        tokens.insert(
            "t-norole",
            Stand::Is(PortalIdentity {
                access_role: false,
                ..identity(dave)
            }),
        );
        tokens.insert(
            "t-otherparty",
            Stand::Is(PortalIdentity {
                portal_party: false,
                ..identity(dave)
            }),
        );
        tokens.insert(
            "t-disabled",
            Stand::Is(PortalIdentity {
                active: false,
                ..identity(dave)
            }),
        );
        tokens.insert(
            "t-unlinked",
            Stand::Refused(ErrorCode::PortalUserNotLinked, "unlinked"),
        );
        let home = tempfile::tempdir().expect("home");
        let root = tempfile::tempdir().expect("root");
        let git = Git::new(GitConfig::new("git", home.path()));
        let storage = StorageRoot::open(root.path()).expect("root");
        Self {
            t,
            olga,
            dave,
            eve,
            web,
            secret,
            portal: Some(Arc::new(FakePortal(tokens))),
            gate: None,
            _dirs: (home, root),
            git,
            storage,
        }
    }

    fn router(&self) -> axum::Router {
        let db: &Db = &self.t.db;
        let mut state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(db.clone()),
            Lookups {
                namespaces: Arc::new(db.clone()),
                projects: Arc::new(db.clone()),
            },
        )
        .with_review(Review::new(db.clone(), Arc::new(NoBranches)))
        .with_pipelines(Pipelines::new(
            db.clone(),
            Browser::new(
                self.git.clone(),
                self.storage.clone(),
                BrowseLimits::default(),
            ),
            Arc::new(OpenMarks),
        ))
        .with_notifications(forge_notify::Settings::new(db.clone()));
        if let Some(portal) = &self.portal {
            state = state.with_portal(Arc::clone(portal));
        }
        if let Some(gate) = &self.gate {
            state.portal_gate = Arc::clone(gate);
        }
        forge_web::app(state, AppConfig::default()).expect("router")
    }

    async fn get(&self, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("GET").uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let req = builder.body(Body::empty()).expect("request");
        let res = self.router().oneshot(req).await.expect("infallible");
        let status = res.status();
        let raw = res.into_body().collect().await.expect("body").to_bytes();
        (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
    }

    async fn as_token(&self, uri: &str, token: &str) -> (StatusCode, Value) {
        let bearer = format!("Bearer {token}");
        self.get(uri, &[("authorization", bearer.as_str())]).await
    }

    /// As [`World::as_token`], with the answer's `Retry-After` and
    /// `Cache-Control`.
    async fn as_token_with_headers(&self, uri: &str, token: &str) -> Answer {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request");
        let res = self.router().oneshot(req).await.expect("infallible");
        let header = |name: &str| {
            res.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (retry_after, cache_control) = (header("retry-after"), header("cache-control"));
        let status = res.status();
        let raw = res.into_body().collect().await.expect("body").to_bytes();
        Answer {
            answer: (status, serde_json::from_slice(&raw).unwrap_or(Value::Null)),
            retry_after,
            cache_control,
        }
    }
}

/// An answer with the two headers the gate's tests read.
struct Answer {
    answer: (StatusCode, Value),
    retry_after: Option<String>,
    cache_control: Option<String>,
}

/// The data of a successful answer (and its envelope's common fields).
fn data(answer: &(StatusCode, Value), kind: &str) -> Value {
    assert_eq!(answer.0, StatusCode::OK, "{}", answer.1);
    assert_eq!(answer.1["success"], true, "{}", answer.1);
    let data = answer.1["data"].clone();
    assert_eq!(data["v"], 1);
    assert_eq!(data["type"], kind);
    let at = data["generated_at"].as_str().expect("generated_at");
    assert!(at.ends_with('Z') && at.len() == 20, "{at}");
    assert!(data["stale_after_sec"].as_u64().is_some());
    assert!(data["title"].as_str().is_some_and(|t| !t.is_empty()));
    data["body"].clone()
}

/// The error code of a failed answer, in the contract's envelope.
fn refused(answer: &(StatusCode, Value)) -> String {
    assert_eq!(answer.1["success"], false, "{}", answer.1);
    assert!(
        answer.1["error"]["message"].as_str().is_some(),
        "{}",
        answer.1
    );
    assert!(answer.1["error"]["request_id"].as_str().is_some());
    answer.1["error"]["code"].as_str().unwrap_or("").to_owned()
}

#[tokio::test]
async fn every_widget_answers_in_its_shape() {
    let w = World::new().await;
    // list: dave reviews olga's change; total counts the visible rows.
    let body = data(&w.as_token(REVIEWS, "t-dave").await, "list");
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(body["total"], 1);
    assert_eq!(items[0]["label"], "Change 1");
    assert_eq!(items[0]["href"], "/acme/web/-/pull/1");
    assert_eq!(items[0]["badge"]["tone"], "info");
    // status: the projects dave holds a role on; no pipeline yet.
    let body = data(&w.as_token(PIPELINES, "t-dave").await, "status");
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["key"], w.web.get().to_string());
    assert_eq!(items[0]["label"], "acme/web");
    assert_eq!(items[0]["state"], "unknown");
    assert_eq!(items[0]["href"], "/acme/web/-/pipelines");
    let deploy = data(
        &w.as_token(&format!("{PIPELINES}?show=deploy"), "t-dave")
            .await,
        "status",
    );
    assert_eq!(deploy["items"][0]["state"], "unknown");
    assert_eq!(deploy["items"][0]["href"], "/acme/web");
    // kpi: olga's stopped auto-merge against the standard 0.
    let kpi = data(&w.as_token(STOPPED, "t-olga").await, "kpi");
    assert_eq!(kpi["value"], 1, "{kpi}");
    assert_eq!(kpi["reference"]["value"], 0);
    assert_eq!(kpi["delta"]["tone"], "stop");
    let none = data(&w.as_token(STOPPED, "t-dave").await, "kpi");
    assert_eq!(none["value"], 0);
    assert_eq!(none["delta"]["tone"], "ok");
    // The options of both widgets: dave's project only.
    for uri in [REVIEW_OPTIONS, PIPELINE_OPTIONS] {
        let a = w.as_token(uri, "t-dave").await;
        assert_eq!(a.0, StatusCode::OK, "{}", a.1);
        let options = a.1["data"]["options"].as_array().expect("options").clone();
        assert_eq!(options.len(), 1, "{}", a.1);
        assert_eq!(options[0]["value"], w.web.get().to_string());
        assert_eq!(options[0]["label"], "acme/web");
    }
}

#[tokio::test]
async fn the_kinds_the_limit_and_the_locale() {
    let w = World::new().await;
    let mine = data(
        &w.as_token(&format!("{REVIEWS}?kind=mine&limit=3"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(mine["total"], 1);
    let stopped = data(
        &w.as_token(&format!("{REVIEWS}?kind=stopped"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(stopped["items"][0]["badge"]["tone"], "stop");
    // spec: no project of dave's released a specification.
    let spec = data(
        &w.as_token(&format!("{REVIEWS}?kind=spec"), "t-dave").await,
        "list",
    );
    assert_eq!(spec["total"], 0);
    // The project filter narrows; unknown keys are ignored.
    let filtered = data(
        &w.as_token(
            &format!("{REVIEWS}?project={}&size=M&refresh=60", w.web.get()),
            "t-dave",
        )
        .await,
        "list",
    );
    assert_eq!(filtered["total"], 1);
    // The locale picks the words.
    let ja = w
        .as_token(&format!("{REVIEWS}?locale=ja&tz=Asia/Jakarta"), "t-dave")
        .await;
    assert_eq!(ja.1["data"]["title"], "自分のレビュー待ち");
    let id = w.as_token(&format!("{REVIEWS}?locale=id"), "t-dave").await;
    assert_eq!(id.1["data"]["title"], "Menunggu tinjauan saya");
    let en = w.as_token(REVIEWS, "t-dave").await;
    assert_eq!(en.1["data"]["title"], "Waiting for my review");
}

#[tokio::test]
async fn a_bad_widget_setting_is_422() {
    let w = World::new().await;
    for uri in [
        format!("{REVIEWS}?kind=everything"),
        format!("{REVIEWS}?limit=2"),
        format!("{REVIEWS}?limit=11"),
        format!("{REVIEWS}?limit=five"),
        format!("{REVIEWS}?project=0"),
        format!("{REVIEWS}?kind=mine&kind=review"),
        format!("{REVIEWS}?locale=fr"),
        format!("{REVIEWS}?tz=../etc"),
        format!("{PIPELINES}?show=everything"),
        format!("{STOPPED}?scope=all"),
        format!(
            "{PIPELINES}?{}",
            (1..=25)
                .map(|n| format!("project={n}"))
                .collect::<Vec<_>>()
                .join("&")
        ),
    ] {
        let a = w.as_token(&uri, "t-dave").await;
        assert_eq!(a.0, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {}", a.1);
        assert_eq!(refused(&a), "FG-WEB-0011", "{uri}");
    }
}

#[tokio::test]
async fn another_persons_and_a_private_projects_rows_never_appear() {
    let w = World::new().await;
    // eve holds no role: nothing to review, no option, no lamp.
    let body = data(&w.as_token(REVIEWS, "t-eve").await, "list");
    assert_eq!(body["total"], 0, "{body}");
    let kpi = data(&w.as_token(STOPPED, "t-eve").await, "kpi");
    assert_eq!(kpi["value"], 0);
    let options = w.as_token(REVIEW_OPTIONS, "t-eve").await;
    assert_eq!(options.1["data"]["options"], Value::Array(Vec::new()));
    // Naming a private project (or one that is not there) shows nothing.
    let asked = format!(
        "{PIPELINES}?project={}&project={}&project=999999",
        w.secret.get(),
        w.web.get()
    );
    let lamps = data(&w.as_token(&asked, "t-dave").await, "status");
    let items = lamps["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{lamps}");
    assert_eq!(items[0]["label"], "acme/web");
    let hidden = data(
        &w.as_token(&format!("{REVIEWS}?project={}", w.secret.get()), "t-dave")
            .await,
        "list",
    );
    assert_eq!(hidden["total"], 0);
    // olga's own list never shows mallory's change in the hidden project.
    let mine = data(
        &w.as_token(&format!("{REVIEWS}?kind=mine"), "t-olga").await,
        "list",
    );
    let labels: Vec<&str> = mine["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["sub"].as_str())
        .collect();
    assert_eq!(labels, ["acme/web !1"]);
    let _ = (w.olga, w.dave, w.eve);
}

#[tokio::test]
async fn the_portals_credential_is_decided_by_authorize() {
    let w = World::new().await;
    for (token, status, code) in [
        ("t-norole", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-otherparty", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-disabled", StatusCode::FORBIDDEN, "FG-AUTH-0004"),
        ("t-unlinked", StatusCode::FORBIDDEN, "FG-AUTH-0027"),
        ("t-forged", StatusCode::UNAUTHORIZED, "FG-AUTH-0026"),
    ] {
        for uri in [REVIEWS, PIPELINES, STOPPED, REVIEW_OPTIONS] {
            let a = w.as_token(uri, token).await;
            assert_eq!(a.0, status, "{token} {uri}: {}", a.1);
            assert_eq!(refused(&a), code, "{token} {uri}");
        }
    }
}

#[tokio::test]
async fn a_bad_portal_token_is_portal_token_invalid() {
    let mut w = World::new().await;
    // No token, a cookie, Basic: the widget routes take the portal's Bearer only.
    let none = w.get(REVIEWS, &[]).await;
    assert_eq!(none.0, StatusCode::UNAUTHORIZED);
    assert_eq!(refused(&none), "FG-AUTH-0026");
    let cookie = w
        .get(REVIEWS, &[("cookie", "__Host-gstn_session=abc")])
        .await;
    assert_eq!(refused(&cookie), "FG-AUTH-0026");
    let basic = w
        .get(REVIEWS, &[("authorization", "Basic dTp0LWRhdmU=")])
        .await;
    assert_eq!(refused(&basic), "FG-AUTH-0026");
    // The portal off: every widget route refuses.
    w.portal = None;
    let off = w.as_token(REVIEWS, "t-dave").await;
    assert_eq!(off.0, StatusCode::UNAUTHORIZED);
    assert_eq!(refused(&off), "FG-AUTH-0026");
}

#[tokio::test]
async fn a_portal_token_is_refused_on_every_other_route() {
    let w = World::new().await;
    // The token the portal forwards is no personal access token: the
    // token authenticator refuses it everywhere but on the widget routes.
    for uri in [
        "/api/v1/user/merge-requests?scope=authored",
        "/api/v1/projects",
        &format!("/api/v1/projects/{}", w.web.get()),
    ] {
        let a = w.as_token(uri, "t-dave").await;
        assert_eq!(a.0, StatusCode::UNAUTHORIZED, "{uri}: {}", a.1);
        assert_eq!(a.1["error_code"], "FG-AUTH-0001", "{uri}");
        // Not the portal's envelope: an ordinary route's answer.
        assert!(a.1.get("success").is_none());
    }
}

/// Review S3: the widget routes are gated before the token is verified —
/// a request over the gate is `429 FG-WEB-0010` with `Retry-After` whatever
/// its token (a forged one would otherwise be 401: the authenticator, and
/// with it the person's lookup, is not reached). Review S4: the refusal is
/// `no-store`.
#[tokio::test]
async fn the_widget_routes_are_gated_before_the_token_is_checked() {
    let mut w = World::new().await;
    // No place among the running requests.
    w.gate = Some(Arc::new(WidgetGate::new(0, 100, Duration::from_secs(60))));
    for token in ["t-dave", "t-forged", "t-unlinked"] {
        let a = w.as_token_with_headers(REVIEWS, token).await;
        assert_eq!(a.answer.0, StatusCode::TOO_MANY_REQUESTS, "{token}");
        assert_eq!(refused(&a.answer), "FG-WEB-0010", "{token}");
        assert_eq!(a.retry_after.as_deref(), Some("1"), "{token}");
        assert_eq!(a.cache_control.as_deref(), Some("no-store"), "{token}");
    }
    // Two requests a minute, whoever they are for and whatever they answer.
    w.gate = Some(Arc::new(WidgetGate::new(8, 2, Duration::from_secs(60))));
    let first = w.as_token_with_headers(REVIEWS, "t-dave").await;
    assert_eq!(first.answer.0, StatusCode::OK, "{}", first.answer.1);
    assert_eq!(
        first.cache_control.as_deref(),
        Some("private, max-age=30"),
        "an answer is kept as before"
    );
    let second = w.as_token_with_headers(STOPPED, "t-forged").await;
    assert_eq!(second.answer.0, StatusCode::UNAUTHORIZED);
    assert_eq!(second.cache_control.as_deref(), Some("no-store"));
    for (uri, token) in [(PIPELINES, "t-olga"), (REVIEWS, "t-forged")] {
        let a = w.as_token_with_headers(uri, token).await;
        assert_eq!(a.answer.0, StatusCode::TOO_MANY_REQUESTS, "{uri} {token}");
        assert_eq!(refused(&a.answer), "FG-WEB-0010", "{uri} {token}");
        let retry: u64 = a
            .retry_after
            .as_deref()
            .and_then(|v| v.parse().ok())
            .expect("Retry-After in seconds");
        assert!((1..=60).contains(&retry), "{retry}");
        assert_eq!(a.cache_control.as_deref(), Some("no-store"));
    }
    // A request that is not the portal's shape is refused before the gate
    // (nothing is verified for it), as before.
    let none = w.get(REVIEWS, &[]).await;
    assert_eq!(none.0, StatusCode::UNAUTHORIZED);
    assert_eq!(refused(&none), "FG-AUTH-0026");
}

// ------------------------------------------------------------ widget design P1

/// The id of merge request `iid` of `project`.
async fn mr_id(t: &FreshDb, project: ProjectId, iid: i64) -> i64 {
    let rows = t
        .json_rows(&format!(
            "SELECT id FROM forge.merge_requests WHERE project_id = {} AND iid = {iid}",
            project.get()
        ))
        .await;
    rows[0]["id"].as_i64().expect("id")
}

/// The `href`s of a body's items.
fn hrefs(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["href"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn the_menu_links_the_purpose_and_a_readable_project_only() {
    let w = World::new().await;
    // Default purpose (developer), no project: one "choose a project" link
    // in place of the project's screens.
    let a = w.as_token(MENU, "t-dave").await;
    let body = data(&a, "links");
    assert_eq!(a.1["data"]["href"], "/");
    let links = hrefs(&body);
    assert!(
        links.contains(&"/-/merge-requests?scope=authored".to_owned()),
        "{body}"
    );
    assert!(
        links.contains(&"/-/projects?tab=member".to_owned()),
        "{body}"
    );
    assert!(links.iter().all(|h| !h.contains("acme")), "{body}");
    // A project dave reads: its screens.
    let mine = data(
        &w.as_token(&format!("{MENU}?project={}", w.web.get()), "t-dave")
            .await,
        "links",
    );
    assert!(
        hrefs(&mine).contains(&"/acme/web/-/pipelines".to_owned()),
        "{mine}"
    );
    // A private project dave holds no role on: as if none were chosen
    // (no path, no hint that it exists).
    let hidden = data(
        &w.as_token(&format!("{MENU}?project={}", w.secret.get()), "t-dave")
            .await,
        "links",
    );
    assert_eq!(hrefs(&hidden), links, "{hidden}");
    // Every purpose answers, in the person's language.
    for purpose in ["developer", "reviewer", "spec", "operations", "manager"] {
        let a = w
            .as_token(&format!("{MENU}?purpose={purpose}&locale=ja"), "t-eve")
            .await;
        let body = data(&a, "links");
        let items = body["items"].as_array().expect("items");
        assert!(!items.is_empty() && items.len() <= 20, "{purpose}");
        assert!(
            a.1["data"]["title"]
                .as_str()
                .is_some_and(|t| t.starts_with("GitStation")),
            "{purpose}"
        );
    }
    let manager = data(
        &w.as_token(&format!("{MENU}?purpose=manager"), "t-dave")
            .await,
        "links",
    );
    assert!(hrefs(&manager).contains(&"/-/activity".to_owned()));
    // The options: dave's project only.
    for uri in [MENU_OPTIONS, ATTENTION_OPTIONS] {
        let a = w.as_token(uri, "t-dave").await;
        assert_eq!(a.0, StatusCode::OK, "{}", a.1);
        let options = a.1["data"]["options"].as_array().expect("options").clone();
        assert_eq!(options.len(), 1, "{uri}: {}", a.1);
        assert_eq!(options[0]["label"], "acme/web");
    }
}

#[tokio::test]
async fn the_inbox_counts_only_what_the_person_may_see_without_admin_rights() {
    let mut w = World::new().await;
    let ada = users::create(
        &w.t.db,
        &ctx("req-user-ada"),
        &NewUser {
            is_admin: true,
            ..NewUser::human("ada", "ada", "ada@example.test")
        },
    )
    .await
    .expect("admin")
    .user_id;
    let mut tokens = HashMap::new();
    tokens.insert("t-dave", Stand::Is(identity(w.dave)));
    tokens.insert("t-ada", Stand::Is(identity(ada)));
    w.portal = Some(Arc::new(FakePortal(tokens)));
    let note = |user: UserId, project: ProjectId, read: bool| {
        format!(
            "INSERT INTO forge.in_app_notifications (user_id, kind, event_id, event_seq, \
             project_id, merge_request_iid, actor_user_id, read_at) VALUES ({}, 'mr_approved', \
             gen_random_uuid(), 1, {}, 1, NULL, {})",
            user.get(),
            project.get(),
            if read {
                "now() + interval '1 second'"
            } else {
                "NULL"
            }
        )
    };
    for sql in [
        note(w.dave, w.web, false),
        note(w.dave, w.web, false),
        note(w.dave, w.web, true),
        // dave holds no role on the hidden project: not counted.
        note(w.dave, w.secret, false),
        // ada is an administrator without a role there: not counted either.
        note(ada, w.secret, false),
    ] {
        w.t.exec_superuser(&sql).await;
    }
    let a = w.as_token(INBOX, "t-dave").await;
    let kpi = data(&a, "kpi");
    assert_eq!(kpi["value"], 2, "{kpi}");
    assert_eq!(kpi["display"], "2 unread");
    assert_eq!(a.1["data"]["href"], "/-/notifications?unread=1");
    let admin = data(&w.as_token(INBOX, "t-ada").await, "kpi");
    assert_eq!(admin["value"], 0, "no administrator override: {admin}");
    let ja = w.as_token(&format!("{INBOX}?locale=ja"), "t-dave").await;
    assert_eq!(ja.1["data"]["title"], "未読の通知");
}

#[tokio::test]
async fn attention_count_counts_each_scope_and_refuses_deploy_for_now() {
    let w = World::new().await;
    // reviews (default): dave reviews olga's change.
    let a = w.as_token(ATTENTION, "t-dave").await;
    let kpi = data(&a, "kpi");
    assert_eq!(kpi["value"], 1, "{kpi}");
    assert_eq!(kpi["delta"]["tone"], "warn");
    assert_eq!(a.1["data"]["href"], "/-/merge-requests?scope=approvable");
    // stopped: olga's stopped auto-merge.
    let stopped = data(
        &w.as_token(&format!("{ATTENTION}?scope=stopped"), "t-olga")
            .await,
        "kpi",
    );
    assert_eq!(stopped["value"], 1);
    assert_eq!(stopped["delta"]["tone"], "stop");
    // The project filter narrows: the hidden project shows nothing.
    let narrowed = data(
        &w.as_token(
            &format!("{ATTENTION}?scope=reviews&project={}", w.secret.get()),
            "t-dave",
        )
        .await,
        "kpi",
    );
    assert_eq!(narrowed["value"], 0);
    let spec = data(
        &w.as_token(&format!("{ATTENTION}?scope=spec"), "t-dave")
            .await,
        "kpi",
    );
    assert_eq!(spec["value"], 0);
    // failed: main's newest pipeline failed in dave's project (and in the
    // hidden one, which is never counted).
    for p in [w.web, w.secret] {
        w.t.exec_superuser(&format!(
            "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, \
               request_id, protected, copy_mark) VALUES ({}, 'refs/heads/main', '{}', \
               'push', 'failed', 'req-pipe', true, 'open')",
            p.get(),
            "c".repeat(40)
        ))
        .await;
    }
    let a = w
        .as_token(&format!("{ATTENTION}?scope=failed&locale=en"), "t-dave")
        .await;
    let failed = data(&a, "kpi");
    assert_eq!(failed["value"], 1, "{failed}");
    assert_eq!(failed["display"], "1 projects");
    assert_eq!(a.1["data"]["href"], "/-/projects?tab=member");
    let asked_hidden = data(
        &w.as_token(
            &format!("{ATTENTION}?scope=failed&project={}", w.secret.get()),
            "t-dave",
        )
        .await,
        "kpi",
    );
    assert_eq!(asked_hidden["value"], 0);
    // pipelines' only_problems: the failed lamp only; none for eve.
    let problems = data(
        &w.as_token(&format!("{PIPELINES}?only_problems=true"), "t-dave")
            .await,
        "status",
    );
    assert_eq!(problems["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(problems["items"][0]["state"], "stop");
    let none = data(
        &w.as_token(&format!("{PIPELINES}?only_problems=true"), "t-eve")
            .await,
        "status",
    );
    assert_eq!(none["items"], Value::Array(Vec::new()));
    // deploy waits for P2: 422 with the setting named.
    for uri in [
        format!("{ATTENTION}?scope=deploy"),
        format!("{ATTENTION}?scope=everything"),
        format!("{ATTENTION}?project=0"),
        format!("{PIPELINES}?only_problems=1"),
        format!("{MENU}?purpose=admin"),
        format!("{MENU}?project=abc"),
        format!("{INBOX}?locale=de"),
        format!("{REVIEWS}?kind=deploy"),
        format!("{REVIEWS}?sort=random"),
    ] {
        let a = w.as_token(&uri, "t-dave").await;
        assert_eq!(a.0, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {}", a.1);
        assert_eq!(refused(&a), "FG-WEB-0011", "{uri}");
    }
}

#[tokio::test]
async fn my_reviews_threads_conflicts_and_queue() {
    let w = World::new().await;
    let web = w.web.get();
    let first = mr_id(&w.t, w.web, 1).await;
    // A second and third change of olga's, and dave's own change.
    let second = mr(&w.t, w.web, 2, w.olga).await;
    let third = mr(&w.t, w.web, 3, w.olga).await;
    let daves = mr(&w.t, w.web, 4, w.dave).await;
    let hidden = mr_id(&w.t, w.secret, 1).await;
    let o = w.olga.get();
    let d = w.dave.get();
    // threads: olga's unresolved thread on dave's change got dave's reply;
    // her thread on her own change 2 has only her note; a resolved one is
    // not shown.
    let thread = |id: i64, mr: i64, starter: i64, resolved: bool| {
        format!(
            "INSERT INTO forge.merge_request_discussions (id, merge_request_id, project_id, kind, \
             started_by, resolved, resolved_by, resolved_at) OVERRIDING SYSTEM VALUE VALUES \
             ({id}, {mr}, {web}, 'general', {starter}, {resolved}, {}, {})",
            if resolved {
                starter.to_string()
            } else {
                "NULL".to_owned()
            },
            if resolved { "now()" } else { "NULL" }
        )
    };
    let note = |discussion: i64, mr: i64, author: i64| {
        format!(
            "INSERT INTO forge.merge_request_notes (discussion_id, merge_request_id, project_id, \
             author_id, body) VALUES ({discussion}, {mr}, {web}, {author}, 'note')"
        )
    };
    w.t.commit_audited(
        ("project", web),
        &[
            thread(9001, daves, o, false),
            note(9001, daves, o),
            note(9001, daves, d),
            thread(9002, second, o, false),
            note(9002, second, o),
            thread(9003, third, o, true),
            note(9003, third, o),
            note(9003, third, d),
        ],
    )
    .await;
    let threads = data(
        &w.as_token(&format!("{REVIEWS}?kind=threads"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(threads["total"], 1, "{threads}");
    assert_eq!(threads["items"][0]["sub"], "acme/web !4");
    assert_eq!(threads["items"][0]["badge"]["tone"], "info");
    assert_eq!(threads["items"][0]["href"], "/acme/web/-/pull/4");
    // eve started nothing: empty.
    let none = data(
        &w.as_token(&format!("{REVIEWS}?kind=threads"), "t-eve")
            .await,
        "list",
    );
    assert_eq!(none["total"], 0);

    // conflicts: change 1 cannot be merged, change 2 is predicted to
    // conflict with dave's change 4, change 3's scan did not finish (never
    // "no conflict").
    w.t.commit_audited(
        ("project", web),
        &[
            format!(
                "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id, \
                 status, checked_head_sha) VALUES ({first}, {web}, 'cannot_be_merged', '{}')",
                head(1)
            ),
            format!(
                "INSERT INTO forge.merge_request_conflict_pairs (low_mr_id, high_mr_id, \
                 project_id, low_head_sha, high_head_sha, target_branch, outcome) VALUES \
                 ({second}, {daves}, {web}, '{}', '{}', 'refs/heads/main', 'conflict')",
                head(2),
                head(4)
            ),
            format!(
                "INSERT INTO forge.merge_request_conflict_scans (merge_request_id, project_id, \
                 status, reason) VALUES ({third}, {web}, 'incomplete', 'peer_cap')"
            ),
        ],
    )
    .await;
    let conflicts = data(
        &w.as_token(&format!("{REVIEWS}?kind=conflicts&locale=en"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(conflicts["total"], 3, "{conflicts}");
    let items = conflicts["items"].as_array().expect("items");
    // Oldest first by default.
    assert_eq!(items[0]["badge"]["label"], "Conflict");
    assert_eq!(items[0]["badge"]["tone"], "stop");
    assert_eq!(items[0]["href"], "/acme/web/-/pull/1/changes");
    assert_eq!(items[1]["badge"]["tone"], "warn");
    assert_eq!(items[1]["sub"], "acme/web !2 \u{2194} !4");
    assert_eq!(items[2]["badge"]["label"], "Not fully checked");
    assert_eq!(items[2]["badge"]["tone"], "neutral");
    // Newest first on request.
    let newest = data(
        &w.as_token(&format!("{REVIEWS}?kind=conflicts&sort=newest"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(newest["items"][0]["sub"], "acme/web !3");
    // dave's change 4 is the other side of the prediction: listed for him.
    let daves_conflicts = data(
        &w.as_token(&format!("{REVIEWS}?kind=conflicts"), "t-dave")
            .await,
        "list",
    );
    assert_eq!(daves_conflicts["total"], 1, "{daves_conflicts}");
    assert_eq!(
        daves_conflicts["items"][0]["sub"],
        "acme/web !4 \u{2194} !2"
    );

    // queue: dave's change 4 then olga's change 1 queued (olga's is 2nd),
    // change 2 dequeued by a person (not shown), change 3 removed by the
    // queue itself.
    let entry = |id: i64, mr: i64, iid: i64, state: &str, removed_by: &str| {
        let finished = if state == "queued" { "NULL" } else { "now()" };
        let reason = if state == "removed" {
            "'checks_failed'"
        } else {
            "NULL"
        };
        format!(
            "INSERT INTO forge.merge_queue_entries (queue_id, project_id, merge_request_id, \
             state, head_sha, method, message, enqueued_by, credential_kind, credential_id, \
             request_id, removed_by, reason, finished_at) VALUES ((SELECT id FROM \
             forge.merge_queues WHERE project_id = {web}), {web}, {mr}, '{state}', '{}', \
             'merge_commit', 'm', {o}, 'session', 1, 'req-q{id}', {removed_by}, {reason}, \
             {finished})",
            head(iid)
        )
    };
    w.t.commit_audited(
        ("project", web),
        &[
            format!(
                "INSERT INTO forge.merge_queues (project_id, target_branch) VALUES ({web}, \
                 'refs/heads/main')"
            ),
            entry(1, daves, 4, "queued", "NULL"),
            entry(2, first, 1, "queued", "NULL"),
            entry(3, second, 2, "removed", &o.to_string()),
            entry(4, third, 3, "removed", "NULL"),
        ],
    )
    .await;
    let queue = data(
        &w.as_token(&format!("{REVIEWS}?kind=queue&locale=en"), "t-olga")
            .await,
        "list",
    );
    assert_eq!(queue["total"], 2, "{queue}");
    assert_eq!(queue["items"][0]["badge"]["label"], "No. 2 in line");
    assert_eq!(queue["items"][0]["badge"]["tone"], "info");
    assert_eq!(
        queue["items"][0]["href"],
        "/acme/web/-/merge-queue?target=main"
    );
    assert_eq!(queue["items"][1]["badge"]["tone"], "stop");
    assert_eq!(queue["items"][1]["sub"], "acme/web !3");

    // Another person's private project never appears in any P1 list.
    w.t.commit_audited(
        ("project", w.secret.get()),
        &[format!(
            "INSERT INTO forge.merge_request_mergeability (merge_request_id, project_id, \
             status, checked_head_sha) VALUES ({hidden}, {}, 'cannot_be_merged', '{}')",
            w.secret.get(),
            head(1)
        )],
    )
    .await;
    for kind in ["threads", "conflicts", "queue"] {
        for token in ["t-olga", "t-dave", "t-eve"] {
            let body = data(
                &w.as_token(&format!("{REVIEWS}?kind={kind}"), token).await,
                "list",
            );
            assert!(
                body["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .all(|i| !i["sub"].as_str().unwrap_or("").contains("hidden")),
                "{kind} {token}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn the_p1_routes_are_decided_by_authorize() {
    let w = World::new().await;
    for (token, status, code) in [
        ("t-norole", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-otherparty", StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ("t-disabled", StatusCode::FORBIDDEN, "FG-AUTH-0004"),
        ("t-unlinked", StatusCode::FORBIDDEN, "FG-AUTH-0027"),
        ("t-forged", StatusCode::UNAUTHORIZED, "FG-AUTH-0026"),
    ] {
        for uri in [ATTENTION, ATTENTION_OPTIONS, INBOX, MENU, MENU_OPTIONS] {
            let a = w.as_token(uri, token).await;
            assert_eq!(a.0, status, "{token} {uri}: {}", a.1);
            assert_eq!(refused(&a), code, "{token} {uri}");
        }
    }
    // A session cookie is no portal credential on the new routes either.
    let cookie = w.get(MENU, &[("cookie", "__Host-gstn_session=abc")]).await;
    assert_eq!(refused(&cookie), "FG-AUTH-0026");
    // Answers are kept as before; failures are not.
    for uri in [ATTENTION, INBOX, MENU] {
        let a = w.as_token_with_headers(uri, "t-dave").await;
        assert_eq!(a.answer.0, StatusCode::OK, "{uri}: {}", a.answer.1);
        assert_eq!(a.cache_control.as_deref(), Some("private, max-age=30"));
        let bad = w
            .as_token_with_headers(&format!("{uri}?locale=xx"), "t-dave")
            .await;
        assert_eq!(bad.answer.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(bad.cache_control.as_deref(), Some("no-store"));
    }
}
