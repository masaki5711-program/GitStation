#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The integration feed routes of M3 I1c through the product router against
//! the development database (ADR-0011), with the real loaders, the feed and
//! the binding commands: only the integration's own service account reads
//! its feed (another integration's account, people — administrators
//! included —, an agent's token, a disabled integration and a missing one
//! refused), `Forge-Act-As` is refused, a gone cursor answers 410 with its
//! `reason` in the body, a tampered cursor 400 without its value in any
//! log line, unknown parameters 400, and the answer has the documented
//! shape (docs/api/integration-feed.example.json, the golden example
//! kanflow's fake GitStation serves). Principals are inserted as request
//! extensions (the authentication layer is tested elsewhere). Synthetic
//! data only.

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{
    CursorKeys, ObjectId, ProjectId, PushRecord, PushRecorded, PushRecorder, PushUpdateKind,
    PushedRef, Pusher, RefName, RequestId, UserId, UserKind,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::work_items::WorkItemSync;
use forge_db::Db;
use forge_hosting::Hosting;
use forge_web::{AppConfig, AppState, Lists, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct NoTokens;

impl forge_core::Authenticator for NoTokens {
    fn authenticate<'a>(
        &'a self,
        _: &'a RequestId,
        _: &'a str,
    ) -> forge_core::authenticator::AuthFuture<'a> {
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

fn keys() -> Arc<CursorKeys> {
    Arc::new(CursorKeys::parse(&format!("3 {}\n", "6b".repeat(32))).expect("keys"))
}

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_hosting(Hosting::new(db.clone()))
    .with_feed(forge_integ::Feed::new(db.clone(), keys()))
    .with_lists(Lists::new(
        Arc::new(db.clone()),
        Arc::new(db.clone()),
        keys(),
    ));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn person(t: &FreshDb, name: &str, admin: bool) -> users::Created {
    let mut new = NewUser::human(
        name,
        &format!("Synthetic {name}"),
        &format!("{name}@example.test"),
    );
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

async fn call_with(
    db: &Db,
    method: &str,
    uri: &str,
    who: Principal,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Answer {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    req.extensions_mut().insert(who);
    let res = router(db).oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
    }
}

async fn call(db: &Db, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
    call_with(db, method, uri, who, body, &[]).await
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, admin: bool, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

fn sa_token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::ServiceAccount,
    )
}

const READ: &[Scope] = &[Scope::ReadApi];

struct World {
    t: FreshDb,
    root: UserId,
    alice: users::Created,
    sa: UserId,
    integration: i64,
    web: ProjectId,
    web_repo: forge_core::RepoId,
}

async fn create_account(t: &FreshDb, root: UserId, owner: UserId, name: &str) -> UserId {
    let created = call(
        &t.db,
        "POST",
        "/api/v1/admin/service-accounts",
        session(root, true),
        Some(json!({
            "username": name,
            "owner_id": owner.get(),
            "purpose": "Kanflow reads the event feed of bound projects",
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    UserId::new(created.json["id"].as_i64().expect("id"))
}

async fn create_integration(t: &FreshDb, root: UserId, sa: UserId) -> i64 {
    let created = call(
        &t.db,
        "POST",
        "/api/v1/admin/integrations",
        session(root, true),
        Some(json!({"kind": "kanflow", "service_account_id": sa.get()})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    created.json["id"].as_i64().expect("integration id")
}

async fn bind(w: &World, project: ProjectId, external: &str) {
    let bound = call(
        &w.t.db,
        "PUT",
        &format!(
            "/api/v1/projects/{}/integration-bindings/{}",
            project.get(),
            w.integration
        ),
        session(w.alice.user_id, false),
        Some(json!({"external_project_id": external})),
    )
    .await;
    assert_eq!(bound.status, StatusCode::OK, "{}", bound.json);
}

async fn unbind(w: &World, project: ProjectId) {
    let done = call(
        &w.t.db,
        "DELETE",
        &format!(
            "/api/v1/projects/{}/integration-bindings/{}",
            project.get(),
            w.integration
        ),
        session(w.alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(done.status, StatusCode::NO_CONTENT, "{}", done.json);
}

async fn sequence(t: &FreshDb) {
    forge_db::sequencer::run_once(
        &t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

async fn drain(t: &FreshDb) {
    sequence(t).await;
    loop {
        match subscribers::run_once(&t.db, &WorkItemSync)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
        sequence(t).await;
    }
}

async fn push(w: &World, branch: &str) {
    let who = Pusher {
        request_id: RequestId::parse("req-push").expect("id"),
        user: w.alice.user_id,
        kind: UserKind::Human,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &w.t.db;
    let done = recorder
        .record_push(
            &who,
            &PushRecord {
                repository_id: w.web_repo,
                updates: vec![PushedRef {
                    name: RefName::parse(&format!("refs/heads/{branch}")).expect("ref"),
                    old: ObjectId::parse(&"0".repeat(40)).expect("oid"),
                    new: ObjectId::parse(&"b".repeat(40)).expect("oid"),
                    kind: PushUpdateKind::Create,
                    commits: Vec::new(),
                    total_commits: 0,
                    total_commits_partial: false,
                }],
            },
        )
        .await
        .expect("push");
    assert_eq!(done, PushRecorded::Recorded);
}

/// An administrator, alice and her project `web` bound (over HTTP, as her
/// Maintainer session) to kanflow project `kf-1` of the integration acting
/// as `kanflow-sync`; work-item-sync registered and caught up.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let root = person(&t, "root", true).await.user_id;
    let alice = person(&t, "alice", false).await;
    let sa = create_account(&t, root, alice.user_id, "kanflow-sync").await;
    let integration = create_integration(&t, root, sa).await;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project");
    let w = World {
        t,
        root,
        alice,
        sa,
        integration,
        web: web.project_id,
        web_repo: web.repository_id,
    };
    bind(&w, w.web, "kf-1").await;
    subscribers::register(&w.t.db, &WorkItemSync, Start::Head)
        .await
        .expect("register");
    drain(&w.t).await;
    w
}

fn events_uri(integration: i64) -> String {
    format!("/api/v1/integrations/{integration}/events")
}

fn cursor_uri(integration: i64) -> String {
    format!("/api/v1/integrations/{integration}/events/cursor")
}

async fn head(w: &World) -> String {
    let got = call(
        &w.t.db,
        "GET",
        &cursor_uri(w.integration),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.json);
    assert_eq!(got.json["schema_version"], 1);
    got.json["cursor"].as_str().expect("cursor").to_owned()
}

#[tokio::test]
async fn feed_principal_must_be_the_integrations_service_account() {
    let w = world().await;
    let db = &w.t.db;
    // Its own service account: read_api (api satisfies it).
    for scopes in [READ, &[Scope::Api]] {
        for uri in [events_uri(w.integration), cursor_uri(w.integration)] {
            let ok = call(db, "GET", &uri, sa_token(w.sa, scopes), None).await;
            assert_eq!(ok.status, StatusCode::OK, "{uri}: {}", ok.json);
        }
    }
    // Another integration's account, people (administrators included), a
    // missing integration: the same 404.
    let other_sa = create_account(&w.t, w.root, w.alice.user_id, "kanflow-other").await;
    let other = create_integration(&w.t, w.root, other_sa).await;
    for uri in [events_uri(w.integration), cursor_uri(w.integration)] {
        for who in [
            sa_token(other_sa, READ),
            session(w.root, true),
            token(w.root, true, &[Scope::Api, Scope::Admin]),
            token(w.alice.user_id, false, &[Scope::Api]),
            session(w.alice.user_id, false),
        ] {
            let refused = call(db, "GET", &uri, who, None).await;
            assert_eq!(
                refused.status,
                StatusCode::NOT_FOUND,
                "{uri}: {}",
                refused.json
            );
            assert_eq!(refused.code(), "FG-REPO-0005");
        }
    }
    let missing = call(
        db,
        "GET",
        &events_uri(other + 1000),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.json);
    assert_eq!(missing.code(), "FG-REPO-0005");
    // Its account's agent token, a token without read_api: refused.
    let agent = call(
        db,
        "GET",
        &events_uri(w.integration),
        sa_token(w.sa, READ).as_agent(true),
        None,
    )
    .await;
    assert_eq!(agent.status, StatusCode::FORBIDDEN, "{}", agent.json);
    assert_eq!(agent.code(), "FG-AUTH-0003");
    let scope = call(
        db,
        "GET",
        &events_uri(w.integration),
        sa_token(w.sa, &[Scope::UserEmail]),
        None,
    )
    .await;
    assert_eq!(scope.code(), "FG-AUTH-0002", "{}", scope.json);
    // A disabled integration: 403 on both routes.
    let disabled = call(
        db,
        "PATCH",
        &format!("/api/v1/admin/integrations/{}", w.integration),
        session(w.root, true),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.json);
    for uri in [events_uri(w.integration), cursor_uri(w.integration)] {
        let refused = call(db, "GET", &uri, sa_token(w.sa, READ), None).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-AUTH-0003");
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn act_as_header_on_feed_is_refused() {
    let w = world().await;
    for uri in [events_uri(w.integration), cursor_uri(w.integration)] {
        let refused = call_with(
            &w.t.db,
            "GET",
            &uri,
            sa_token(w.sa, READ),
            None,
            &[("Forge-Act-As", "alice")],
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-AUTH-0003");
    }
    w.t.drop_now().await;
}

/// A gone cursor is 410 FG-INT-0007 with `reason` in the body — the only
/// field besides the standard three; here the rebind of the same project
/// (ADR-0016 Amendment I1c).
#[tokio::test]
async fn gone_cursor_answers_410_with_its_reason() {
    let w = world().await;
    let before = head(&w).await;
    unbind(&w, w.web).await;
    bind(&w, w.web, "kf-1").await;
    drain(&w.t).await;
    let gone = call(
        &w.t.db,
        "GET",
        &format!("{}?cursor={before}", events_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::GONE, "{}", gone.json);
    assert_eq!(gone.code(), "FG-INT-0007");
    assert_eq!(gone.json["reason"], "visible_set_changed");
    let mut keys: Vec<&str> = gone
        .json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["error_code", "reason", "request_id", "user_message"]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn unknown_or_malformed_feed_parameters_are_400() {
    let w = world().await;
    for query in [
        "after=x",
        "limit=0",
        "limit=501",
        "limit=1&limit=2",
        "cursor=",
    ] {
        let bad = call(
            &w.t.db,
            "GET",
            &format!("{}?{query}", events_uri(w.integration)),
            sa_token(w.sa, READ),
            None,
        )
        .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}: {}", bad.json);
        assert_eq!(bad.code(), "FG-WEB-0006", "{query}");
    }
    let bad = call(
        &w.t.db,
        "GET",
        &format!("{}?limit=1", cursor_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(bad.code(), "FG-WEB-0006", "{}", bad.json);
    w.t.drop_now().await;
}

// ── logs ───────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;

impl Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(c) = s.borrow().as_ref() {
                c.0.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        CaptureGuard
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

/// A changed cursor is 400 FG-INT-0008; its value reaches neither the
/// answer nor any log line (the access log too has the path only).
#[tokio::test]
async fn tampered_cursor_value_never_logged() {
    let w = world().await;
    let good = head(&w).await;
    // Flip one character in the middle (base64url stays valid).
    let mut chars: Vec<char> = good.chars().collect();
    let mid = chars.len() / 2;
    chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    let logs = Captured::default();
    let guard = logs.install();
    let refused = call(
        &w.t.db,
        "GET",
        &format!("{}?cursor={tampered}", events_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    drop(guard);
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.json);
    assert_eq!(refused.code(), "FG-INT-0008");
    assert!(!refused.json.to_string().contains(&tampered));
    let text = logs.text();
    assert!(
        text.contains("FG-INT-0008"),
        "the refusal is logged: {text}"
    );
    assert!(!text.contains(&tampered), "the value is never logged");
    assert!(!text.contains(&good));
    w.t.drop_now().await;
}

// ── the documented shape ───────────────────────────────────────────────

fn keys_of(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

/// The answer has exactly the shape of the golden example kanflow's fake
/// GitStation serves (docs/api/integration-feed.example.json): the same
/// fields at every level (`schema_version` 1; fields are only ever added,
/// so a new field updates the example in the same change).
#[tokio::test]
async fn the_feed_answers_the_golden_example_shape() {
    let w = world().await;
    let before = head(&w).await;
    push(&w, "feature/PBI-12-login-form").await;
    drain(&w.t).await;
    let page = call(
        &w.t.db,
        "GET",
        &format!("{}?cursor={before}&limit=10", events_uri(w.integration)),
        sa_token(w.sa, &[Scope::ReadApi, Scope::UserEmail]),
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.json);
    let golden: Value = serde_json::from_str(include_str!(
        "../../../docs/api/integration-feed.example.json"
    ))
    .expect("golden example");
    assert_eq!(keys_of(&page.json), keys_of(&golden["page"]));
    assert_eq!(page.json["schema_version"], 1);
    let event = &page.json["events"][0];
    let golden_event = &golden["page"]["events"][0];
    assert_eq!(keys_of(event), keys_of(golden_event));
    assert_eq!(keys_of(&event["actor"]), keys_of(&golden_event["actor"]));
    assert_eq!(
        keys_of(&event["actor"]["user"]),
        keys_of(&golden_event["actor"]["user"])
    );
    assert_eq!(
        keys_of(&event["work_items"][0]),
        keys_of(&golden_event["work_items"][0])
    );
    assert_eq!(event["type"], "repository.pushed");
    assert_eq!(event["project_id"], w.web.get());
    assert_eq!(event["actor"]["user"]["email"], "alice@example.test");
    assert_eq!(
        event["work_items"][0],
        json!({"provider": "kanflow", "project_id": "kf-1", "ref": "PBI-12"})
    );
    assert!(event["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("evt_") && id.len() == 36));
    let cursor = call(
        &w.t.db,
        "GET",
        &cursor_uri(w.integration),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(keys_of(&cursor.json), keys_of(&golden["cursor"]));
    let gone = &golden["gone"];
    assert_eq!(
        keys_of(gone),
        ["error_code", "reason", "request_id", "user_message"]
    );
    w.t.drop_now().await;
}

/// Re-review (I1c): a second concurrent read of the same feed answers 429
/// FG-WEB-0010 with `Retry-After: 1` through the router while the first one
/// is in flight (the first one then completes with 200).
///
/// Deterministic: the first read takes its slot (in `Feed::read_events`,
/// after the route's own DB lookups), then blocks on an ACCESS EXCLUSIVE
/// lock held here on `forge.event_feed_state`, the first table
/// `Feed::decide` reads. The second request is sent only once a backend is
/// seen waiting on that lock, so the slot is certainly held. Racing two
/// `join!`ed requests was flaky: under load the first could finish before
/// the second reached the slot.
#[tokio::test]
async fn a_concurrent_feed_read_answers_429_with_retry_after() {
    let w = world().await;
    let app = router(&w.t.db);
    let request = || {
        let mut req = Request::builder()
            .method("GET")
            .uri(events_uri(w.integration))
            .body(Body::empty())
            .expect("request");
        req.extensions_mut().insert(sa_token(w.sa, READ));
        req
    };
    let blocker = w.t.lock_table("forge.event_feed_state").await;
    let first = tokio::spawn({
        let app = app.clone();
        let req = request();
        async move { app.oneshot(req).await }
    });
    w.t.until_waiting("event_feed_state").await;
    let busy = app.clone().oneshot(request()).await.expect("infallible");
    blocker.release().await;
    let first = first.await.expect("join").expect("infallible");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(busy.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        busy.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    w.t.drop_now().await;
}

// ── M3 I1d: the development status routes ─────────────────────────────

fn dev_status_uri(integration: i64, reference: &str) -> String {
    format!("/api/v1/integrations/{integration}/work-items/{reference}/dev-status")
}

fn dev_status_all_uri(integration: i64) -> String {
    format!("/api/v1/integrations/{integration}/dev-status")
}

/// The development status is decided like the feed (ADR-0016 Amendment
/// I1d): its own service account reads both routes (read_api, api);
/// another integration's account, people (administrators included) and a
/// missing integration are 404; an agent's token, `Forge-Act-As` and a
/// disabled integration 403.
#[tokio::test]
async fn dev_status_principal_must_be_the_integrations_service_account() {
    let w = world().await;
    let db = &w.t.db;
    let uris = [
        dev_status_uri(w.integration, "PBI-1"),
        dev_status_all_uri(w.integration),
    ];
    for scopes in [READ, &[Scope::Api]] {
        for uri in &uris {
            let ok = call(db, "GET", uri, sa_token(w.sa, scopes), None).await;
            assert_eq!(ok.status, StatusCode::OK, "{uri}: {}", ok.json);
            assert_eq!(ok.json["schema_version"], 1);
        }
    }
    let other_sa = create_account(&w.t, w.root, w.alice.user_id, "kanflow-other").await;
    let _other = create_integration(&w.t, w.root, other_sa).await;
    for uri in &uris {
        for who in [
            sa_token(other_sa, READ),
            session(w.root, true),
            token(w.root, true, &[Scope::Api, Scope::Admin]),
            token(w.alice.user_id, false, &[Scope::Api]),
        ] {
            let refused = call(db, "GET", uri, who, None).await;
            assert_eq!(
                refused.status,
                StatusCode::NOT_FOUND,
                "{uri}: {}",
                refused.json
            );
            assert_eq!(refused.code(), "FG-REPO-0005");
        }
        let agent = call(db, "GET", uri, sa_token(w.sa, READ).as_agent(true), None).await;
        assert_eq!(agent.code(), "FG-AUTH-0003", "{uri}: {}", agent.json);
        let act_as = call_with(
            db,
            "GET",
            uri,
            sa_token(w.sa, READ),
            None,
            &[("Forge-Act-As", "alice")],
        )
        .await;
        assert_eq!(
            act_as.status,
            StatusCode::FORBIDDEN,
            "{uri}: {}",
            act_as.json
        );
        assert_eq!(act_as.code(), "FG-AUTH-0003");
    }
    let missing = call(
        db,
        "GET",
        &dev_status_all_uri(w.integration + 1000),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(missing.code(), "FG-REPO-0005", "{}", missing.json);
    let disabled = call(
        db,
        "PATCH",
        &format!("/api/v1/admin/integrations/{}", w.integration),
        session(w.root, true),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.json);
    for uri in &uris {
        let refused = call(db, "GET", uri, sa_token(w.sa, READ), None).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{uri}: {}",
            refused.json
        );
    }
    w.t.drop_now().await;
}

/// Parameters are checked strictly (FG-WEB-0006); a malformed reference is
/// 400 only for the integration's own account (the decision comes first).
#[tokio::test]
async fn dev_status_parameters_are_checked() {
    let w = world().await;
    let db = &w.t.db;
    for uri in [
        dev_status_uri(w.integration, "PBI-012"),
        dev_status_uri(w.integration, "pbi-1"),
        format!("{}?state=draft", dev_status_uri(w.integration, "PBI-1")),
        format!(
            "{}?project_id=kf%201",
            dev_status_uri(w.integration, "PBI-1")
        ),
        format!("{}?cursor=x", dev_status_uri(w.integration, "PBI-1")),
        format!("{}?limit=0", dev_status_all_uri(w.integration)),
        format!("{}?limit=101", dev_status_all_uri(w.integration)),
        format!("{}?cursor=", dev_status_all_uri(w.integration)),
        format!("{}?after=1", dev_status_all_uri(w.integration)),
    ] {
        let refused = call(db, "GET", &uri, sa_token(w.sa, READ), None).await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "{uri}: {}",
            refused.json
        );
        assert_eq!(refused.code(), "FG-WEB-0006", "{uri}");
    }
    let foreign = call(
        db,
        "GET",
        &dev_status_uri(w.integration, "PBI-012"),
        session(w.root, true),
        None,
    )
    .await;
    assert_eq!(foreign.code(), "FG-REPO-0005", "{}", foreign.json);
    let tampered = call(
        db,
        "GET",
        &format!("{}?cursor=AAAA", dev_status_all_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(
        tampered.status,
        StatusCode::BAD_REQUEST,
        "{}",
        tampered.json
    );
    assert_eq!(tampered.code(), "FG-INT-0008");
    w.t.drop_now().await;
}

/// Plan 11 `unbound_ref_is_404` over HTTP: a kanflow project no visible
/// binding has is 404 FG-REPO-0005 with the standard body only.
#[tokio::test]
async fn unbound_ref_is_404() {
    let w = world().await;
    let refused = call(
        &w.t.db,
        "GET",
        &format!(
            "{}?project_id=kf-other",
            dev_status_uri(w.integration, "PBI-1")
        ),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.json);
    assert_eq!(refused.code(), "FG-REPO-0005");
    assert_eq!(
        keys_of(&refused.json),
        ["error_code", "request_id", "user_message"]
    );
    let bound = call(
        &w.t.db,
        "GET",
        &format!("{}?project_id=kf-1", dev_status_uri(w.integration, "PBI-1")),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(bound.status, StatusCode::OK, "{}", bound.json);
    assert_eq!(bound.json["work_items"][0]["merge_requests"], json!([]));
    w.t.drop_now().await;
}

/// The answers have the golden example's shape (docs/api/dev-status.example.json,
/// kanflow's fake GitStation): the same fields at the top, the work item
/// and the page level (a merge request's fields: the unit test
/// `a_merge_request_has_the_golden_fields`).
#[tokio::test]
async fn the_dev_status_answers_the_golden_example_shape() {
    let w = world().await;
    push(&w, "feature/PBI-12-login-form").await;
    drain(&w.t).await;
    let golden: Value =
        serde_json::from_str(include_str!("../../../docs/api/dev-status.example.json"))
            .expect("golden example");
    let one = call(
        &w.t.db,
        "GET",
        &dev_status_uri(w.integration, "PBI-12"),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.json);
    assert_eq!(keys_of(&one.json), keys_of(&golden["work_item"]));
    assert_eq!(
        keys_of(&one.json["work_items"][0]),
        keys_of(&golden["work_item"]["work_items"][0])
    );
    assert_eq!(one.json["work_items"][0]["ref"], "PBI-12");
    assert_eq!(one.json["work_items"][0]["project_id"], "kf-1");
    let page = call(
        &w.t.db,
        "GET",
        &format!("{}?limit=1", dev_status_all_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.json);
    assert_eq!(keys_of(&page.json), keys_of(&golden["page"]));
    assert_eq!(
        keys_of(&page.json["work_items"][0]),
        keys_of(&golden["page"]["work_items"][0])
    );
    // The feed cursor it names reads the feed.
    let cursor = page.json["feed_cursor"].as_str().expect("feed_cursor");
    let feed = call(
        &w.t.db,
        "GET",
        &format!("{}?cursor={cursor}", events_uri(w.integration)),
        sa_token(w.sa, READ),
        None,
    )
    .await;
    assert_eq!(feed.status, StatusCode::OK, "{}", feed.json);
    assert_eq!(
        keys_of(&golden["not_found"]),
        ["error_code", "request_id", "user_message"]
    );
    w.t.drop_now().await;
}
