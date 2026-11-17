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
        ));
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
