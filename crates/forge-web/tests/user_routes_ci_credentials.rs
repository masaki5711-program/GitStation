#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Every `/api/v1/user*` route refuses a runner's and a job's credential
//! with the CI arm's denial (`403 FG-AUTH-0003`, ADR-0029 §1) - never a
//! `500` - and an act-as caller with `act_as_not_allowed` (same code).
//! Runner and job credentials carry no user; the loaders of the caller's
//! own lists used to assume a person and failed with `FG-SYS-0001
//! reason=*_route_without_user` (found in plan 15 MR 6, whose loader was
//! the first to answer `absent_because(..)` instead). Real tokens through
//! the product router and the development database (ADR-0011); synthetic
//! data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::Scope;
use forge_core::act_as::ActAsKeys;
use forge_core::{ObjectId, RefName, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_runners::lease;
use forge_db::ci_runners::{register, NewRunner};
use forge_db::groups::{self, NewGroup};
use forge_db::pipelines::{
    self as pipelines, NewCiJob, NewPipeline, PipelineRequest, Planned, Source,
};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
use forge_notify::Settings;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        // Runner and job tokens are authenticated by forge-db.
        Arc::new(db.clone()),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    // The owner lookups of the `{id}` routes need their stores.
    .with_hosting(Hosting::new(db.clone()))
    .with_notifications(Settings::new(db.clone()))
    .with_act_as(forge_integ::ActAs::new(
        db.clone(),
        Some(ActAsKeys::parse(KEYS).expect("keys")),
    ));
    forge_web::app(state, AppConfig::default()).expect("router")
}

/// The act-as keys of the synthetic kanflow integration.
const KEYS: &str = "1:BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY= active";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

/// The callers without a person of their own: a runner, the job it
/// claimed, and a service account acting for olga.
struct Callers {
    runner: String,
    job: String,
    kanflow: String,
    act_as: String,
}

async fn credentials(t: &FreshDb) -> Callers {
    let olga: UserId = users::create(
        &t.db,
        &ctx("req-user-olga"),
        &NewUser::human("olga", "olga", "olga@example.test"),
    )
    .await
    .expect("olga")
    .user_id;
    let g = groups::create(
        &t.db,
        &ctx("req-acme"),
        &NewGroup {
            segment: "acme".into(),
            parent: None,
            display_name: "acme".into(),
            description: String::new(),
            owner: olga,
        },
    )
    .await
    .expect("group");
    let web = projects::create(
        &t.db,
        &ctx("req-acme-web"),
        &NewProject::new("web", g.namespace_id, "Web", olga),
    )
    .await
    .expect("project")
    .project_id;
    let runner = register(
        &t.db,
        &ChangeContext::new(
            Actor::User(olga),
            RequestId::parse("req-runner").expect("id"),
        ),
        &NewRunner {
            name: "linux-1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner");
    let _pipeline = pipelines::create(
        &t.db,
        &NewPipeline {
            ai_eval_job: None,
            request: PipelineRequest {
                project: web,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
                source: Source::Manual,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-seed".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: pipelines::CopyMark::Open,
            planned: Planned::Jobs(vec![NewCiJob {
                name: "build".to_owned(),
                stage: "build".to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                timeout_s: 600,
                tags: Vec::new(),
                secrets: Vec::new(),
            }]),
        },
    )
    .await
    .expect("pipeline");
    let job = lease::claim(&t.db, runner.runner.id)
        .await
        .expect("claim")
        .expect("a job")
        .token
        .reveal();
    // A service account's act-as token, acting for olga.
    let sa = forge_db::service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &forge_db::service_accounts::NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: olga,
            purpose: "synthetic act-as caller".to_owned(),
        },
        olga,
    )
    .await
    .expect("sa")
    .user_id;
    let _integration = forge_db::integrations::create(
        &t.db,
        &ctx("req-int"),
        &forge_db::integrations::NewIntegration {
            kind: forge_core::IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        olga,
    )
    .await
    .expect("integration");
    let kanflow = forge_db::tokens::issue_for_service_account(
        &t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: vec![Scope::ReadApi, Scope::ActAs],
            expires_in: None,
            is_agent: false,
        },
        olga,
    )
    .await
    .expect("token")
    .token
    .reveal();
    Callers {
        runner: format!("Bearer {}", runner.token.reveal()),
        job: format!("Bearer {job}"),
        kanflow: format!("Bearer {kanflow}"),
        act_as: format!("user:{}", olga.get()),
    }
}

/// Every `/api/v1/user*` route of the registry: (method, path template,
/// URI, JSON body). A registered route missing here fails
/// `the_table_covers_every_user_route_of_the_registry`.
const ROUTES: &[(&str, &str, &str, Option<&str>)] = &[
    ("GET", "/api/v1/user", "/api/v1/user", None),
    ("GET", "/api/v1/user/tokens", "/api/v1/user/tokens", None),
    (
        "POST",
        "/api/v1/user/tokens",
        "/api/v1/user/tokens",
        Some(r#"{"name":"ci","scopes":["read_api"]}"#),
    ),
    (
        "DELETE",
        "/api/v1/user/tokens/{id}",
        "/api/v1/user/tokens/1",
        None,
    ),
    // M4 C13: the SSH keys.
    (
        "GET",
        "/api/v1/user/ssh-keys",
        "/api/v1/user/ssh-keys",
        None,
    ),
    (
        "POST",
        "/api/v1/user/ssh-keys",
        "/api/v1/user/ssh-keys",
        Some(
            r#"{"key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAGuL+srU7BTyMdIqbgh4NRqXH1vOCQWfZxL4x1ZB/uN"}"#,
        ),
    ),
    (
        "DELETE",
        "/api/v1/user/ssh-keys/{id}",
        "/api/v1/user/ssh-keys/1",
        None,
    ),
    (
        "GET",
        "/api/v1/user/sessions",
        "/api/v1/user/sessions",
        None,
    ),
    (
        "DELETE",
        "/api/v1/user/sessions/{public_id}",
        "/api/v1/user/sessions/0123456789abcdef",
        None,
    ),
    (
        "GET",
        "/api/v1/user/notification-preferences",
        "/api/v1/user/notification-preferences",
        None,
    ),
    (
        "PUT",
        "/api/v1/user/notification-preferences",
        "/api/v1/user/notification-preferences",
        Some(r#"{"email_enabled":true,"language":null,"muted_kinds":[]}"#),
    ),
    (
        "GET",
        "/api/v1/user/notifications",
        "/api/v1/user/notifications",
        None,
    ),
    (
        "GET",
        "/api/v1/user/notifications/unread-count",
        "/api/v1/user/notifications/unread-count",
        None,
    ),
    (
        "POST",
        "/api/v1/user/notifications/{id}/read",
        "/api/v1/user/notifications/1/read",
        None,
    ),
    (
        "POST",
        "/api/v1/user/notifications/read-all",
        "/api/v1/user/notifications/read-all",
        Some("{}"),
    ),
    (
        "GET",
        "/api/v1/user/merge-requests",
        "/api/v1/user/merge-requests?scope=authored",
        None,
    ),
    (
        "GET",
        "/api/v1/user/merge-requests/counts",
        "/api/v1/user/merge-requests/counts",
        None,
    ),
    (
        "GET",
        "/api/v1/user/activity",
        "/api/v1/user/activity",
        None,
    ),
];

/// Token creations whose body the route would refuse (`FG-WEB-0006`): the
/// caller is decided first, so a runner or a job still gets `403`.
const BAD_TOKEN_BODIES: &[&str] = &[
    r#"{"name":"ci","scopes":["no_such_scope"]}"#,
    r#"{"name":"ci","scopes":[]}"#,
];

async fn send(
    db: &Db,
    method: &str,
    uri: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req
        .body(Body::from(body.unwrap_or_default().to_owned()))
        .expect("request");
    let res = router(db).oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

#[test]
fn the_table_covers_every_user_route_of_the_registry() {
    let mut registered: Vec<String> = forge_web::app::registry()
        .defs()
        .iter()
        .filter(|d| d.path == "/api/v1/user" || d.path.starts_with("/api/v1/user/"))
        // `HEAD` runs its `GET`'s loader.
        .filter(|d| d.verb != forge_web::registry::Verb::Head)
        .map(|d| format!("{} {}", d.verb.as_str(), d.path))
        .collect();
    registered.sort();
    registered.dedup();
    let mut tested: Vec<String> = ROUTES
        .iter()
        .map(|(method, template, _, _)| format!("{method} {template}"))
        .collect();
    tested.sort();
    tested.dedup();
    let missing: Vec<&String> = registered.iter().filter(|r| !tested.contains(r)).collect();
    let stale: Vec<&String> = tested.iter().filter(|t| !registered.contains(t)).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "ROUTES in crates/forge-web/tests/user_routes_ci_credentials.rs is out of date: \
         registered but untested {missing:?}; tested but not registered {stale:?}"
    );
}

#[tokio::test]
async fn runner_job_and_act_as_credentials_are_refused_on_every_user_route_never_500() {
    let t = FreshDb::migrated().await;
    let c = credentials(&t).await;
    let runner = [("authorization", c.runner.as_str())];
    let job = [("authorization", c.job.as_str())];
    let act_as = [
        ("authorization", c.kanflow.as_str()),
        ("forge-act-as", c.act_as.as_str()),
    ];
    let callers: [(&str, &[(&str, &str)]); 3] =
        [("runner", &runner), ("job", &job), ("act-as", &act_as)];
    let mut requests: Vec<(&str, &str, Option<&str>)> = ROUTES
        .iter()
        .map(|(method, _, uri, body)| (*method, *uri, *body))
        .collect();
    for body in BAD_TOKEN_BODIES {
        requests.push(("POST", "/api/v1/user/tokens", Some(body)));
    }
    let mut wrong = Vec::new();
    for (method, uri, body) in requests {
        for (who, headers) in &callers {
            let (status, json) = send(&t.db, method, uri, body, headers).await;
            let code = json["error_code"].as_str().unwrap_or("");
            if status != StatusCode::FORBIDDEN || code != "FG-AUTH-0003" {
                wrong.push(format!("{who} {method} {uri} {body:?}: {status} {json}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "expected 403 FG-AUTH-0003 (ci_not_allowed / act_as_not_allowed) for:\n{}",
        wrong.join("\n")
    );
}
