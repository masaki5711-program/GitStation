#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Every `/api/v1/user*` route refuses a runner's and a job's credential
//! with the CI arm's denial (`403 FG-AUTH-0003`, ADR-0029 §1) - never a
//! `500`. Those credentials carry no user; the loaders of the caller's own
//! lists used to assume a person and failed with `FG-SYS-0001
//! reason=*_route_without_user` (found in plan 15 MR 6, whose loader was
//! the first to answer `absent_because(..)` instead). Real tokens through
//! the product router and the development database (ADR-0011); synthetic
//! data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
    .with_notifications(Settings::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

/// A runner's bearer token and the token of the job it claimed.
async fn ci_tokens(t: &FreshDb) -> (String, String) {
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
    (
        format!("Bearer {}", runner.token.reveal()),
        format!("Bearer {job}"),
    )
}

/// Every `/api/v1/user*` route of the registry (method, URI, JSON body).
const ROUTES: &[(&str, &str, Option<&str>)] = &[
    ("GET", "/api/v1/user", None),
    ("GET", "/api/v1/user/tokens", None),
    (
        "POST",
        "/api/v1/user/tokens",
        Some(r#"{"name":"ci","scopes":["read_api"]}"#),
    ),
    ("DELETE", "/api/v1/user/tokens/1", None),
    ("GET", "/api/v1/user/sessions", None),
    ("DELETE", "/api/v1/user/sessions/0123456789abcdef", None),
    ("GET", "/api/v1/user/notification-preferences", None),
    (
        "PUT",
        "/api/v1/user/notification-preferences",
        Some(r#"{"email_enabled":true,"language":null,"muted_kinds":[]}"#),
    ),
    ("GET", "/api/v1/user/notifications", None),
    ("GET", "/api/v1/user/notifications/unread-count", None),
    ("POST", "/api/v1/user/notifications/1/read", None),
    ("POST", "/api/v1/user/notifications/read-all", Some("{}")),
    ("GET", "/api/v1/user/merge-requests?scope=authored", None),
    ("GET", "/api/v1/user/merge-requests/counts", None),
];

async fn send(
    db: &Db,
    method: &str,
    uri: &str,
    body: Option<&str>,
    auth: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", auth);
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

#[tokio::test]
async fn runner_and_job_credentials_are_refused_on_every_user_route_never_500() {
    let t = FreshDb::migrated().await;
    let (runner, job) = ci_tokens(&t).await;
    let mut wrong = Vec::new();
    for (method, uri, body) in ROUTES {
        for (who, auth) in [("runner", runner.as_str()), ("job", job.as_str())] {
            let (status, json) = send(&t.db, method, uri, *body, auth).await;
            let code = json["error_code"].as_str().unwrap_or("");
            if status != StatusCode::FORBIDDEN || code != "FG-AUTH-0003" {
                wrong.push(format!("{who} {method} {uri}: {status} {json}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "expected 403 FG-AUTH-0003 (the CI arm's ci_not_allowed) for:\n{}",
        wrong.join("\n")
    );
}
