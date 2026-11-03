#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The work item link routes of M3 I1b through the product router against
//! the development database (ADR-0011), with the real loaders and
//! commands: a Developer links a kanflow work item to a merge request by
//! hand, lists the links (open and ended), unlinks it (the row stays); a
//! Reporter is refused, an unbound project is FG-INT-0018, a reference
//! that is not canonical is FG-WEB-0006 before anything is looked up.
//! Principals are inserted as request extensions. Synthetic data only.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{IntegrationKind, RefName, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::integrations::{self, NewIntegration};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::repositories;
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::Hosting;
use forge_review::{BranchProbe, Review};
use forge_web::{AppConfig, AppState, Lookups};
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
        Box::pin(async { Err(ForgeError::new(ErrorCode::Unauthenticated)) })
    }
}

struct Branches(BTreeSet<&'static str>);

impl BranchProbe for Branches {
    fn branch_head<'a>(
        &'a self,
        _repo: RepoId,
        branch: &'a RefName,
        _caller: &'a str,
    ) -> LookupFuture<'a, forge_core::ObjectId> {
        let found = branch.branch().is_some_and(|b| self.0.contains(b));
        Box::pin(async move {
            if found {
                forge_core::ObjectId::parse(&"1".repeat(40))
            } else {
                Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"))
            }
        })
    }
}

fn router(db: &Db) -> axum::Router {
    let branches = Branches(["main", "feature"].into_iter().collect());
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
    .with_review(Review::new(db.clone(), Arc::new(branches)));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn user(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
    .user_id
}

struct Answer {
    status: StatusCode,
    json: Value,
}

async fn call(db: &Db, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
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
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn denied(a: &Answer) -> (StatusCode, &str) {
    (a.status, a.json["error_code"].as_str().unwrap_or(""))
}

/// `acme/<segment>` (private, ready), owned by `owner`, with `members`.
async fn project(
    t: &FreshDb,
    group: forge_core::NamespaceId,
    segment: &str,
    owner: UserId,
    members: &[(UserId, Role)],
) -> CreatedProject {
    let made = projects::create(
        &t.db,
        &ctx(&format!("req-{segment}")),
        &NewProject::new(segment, group, segment, owner),
    )
    .await
    .expect("project");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), made.repository_id)
        .await
        .expect("ready");
    for (who, role) in members {
        projects::add_member(&t.db, &ctx("req-member"), made.project_id, *who, *role)
            .await
            .expect("member");
    }
    made
}

struct World {
    t: FreshDb,
    olga: UserId,
    dave: UserId,
    rita: UserId,
    web: CreatedProject,
    api: CreatedProject,
}

/// olga owns `acme/web` (bound to kanflow `kf-1`) and `acme/api` (not
/// bound); dave is a Developer and rita a Reporter of both; each has
/// merge request !1 from `feature`.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let rita = user(&t, "rita").await;
    let group = groups::create(
        &t.db,
        &ctx("req-acme"),
        &NewGroup {
            segment: "acme".into(),
            parent: None,
            display_name: "Acme".into(),
            description: String::new(),
            owner: olga,
        },
    )
    .await
    .expect("acme");
    let members = [(dave, Role::Developer), (rita, Role::Reporter)];
    let web = project(&t, group.namespace_id, "web", olga, &members).await;
    let api = project(&t, group.namespace_id, "api", olga, &members).await;
    let sa = service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: olga,
            purpose: "Kanflow reads the event feed".to_owned(),
        },
        olga,
    )
    .await
    .expect("service account")
    .user_id;
    let integration = integrations::create(
        &t.db,
        &ctx("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        olga,
    )
    .await
    .expect("integration");
    let bound = call(
        &t.db,
        "PUT",
        &format!(
            "/api/v1/projects/{}/integration-bindings/{}",
            web.project_id.get(),
            integration.get()
        ),
        session(olga),
        Some(json!({"external_project_id": "kf-1"})),
    )
    .await;
    assert_eq!(bound.status, StatusCode::OK, "{}", bound.json);
    for p in [&web, &api] {
        let opened = call(
            &t.db,
            "POST",
            &format!("/api/v1/projects/{}/merge-requests", p.project_id.get()),
            session(dave),
            Some(json!({
                "title": "Add the login form",
                "description": "",
                "source_branch": "feature",
                "target_branch": "main",
                "draft": false
            })),
        )
        .await;
        assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json);
    }
    World {
        t,
        olga,
        dave,
        rita,
        web,
        api,
    }
}

fn links_uri(p: &CreatedProject) -> String {
    format!(
        "/api/v1/projects/{}/merge-requests/1/work-items",
        p.project_id.get()
    )
}

#[tokio::test]
async fn a_developer_links_lists_and_unlinks_a_work_item() {
    let w = world().await;
    let one = format!("{}/PBI-43", links_uri(&w.web));
    let linked = call(&w.t.db, "PUT", &one, session(w.dave), None).await;
    assert_eq!(linked.status, StatusCode::OK, "{}", linked.json);
    assert_eq!(linked.json["outcome"], "linked");
    assert_eq!(linked.json["iid"], 1);
    assert_eq!(linked.json["source"], "manual");
    assert_eq!(
        linked.json["work_item"],
        json!({"provider": "kanflow", "project_id": "kf-1", "ref": "PBI-43"})
    );
    let again = call(&w.t.db, "PUT", &one, session(w.olga), None).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    assert_eq!(again.json["outcome"], "unchanged");
    assert_eq!(again.json["link_id"], linked.json["link_id"]);

    // A Reporter reads the links.
    let listed = call(&w.t.db, "GET", &links_uri(&w.web), session(w.rita), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["truncated"], false);
    let links = listed.json["links"].as_array().expect("links");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["linked_by"], w.dave.get());
    assert_eq!(links[0]["unlinked_at"], Value::Null);

    let unlinked = call(&w.t.db, "DELETE", &one, session(w.dave), None).await;
    assert_eq!(unlinked.status, StatusCode::NO_CONTENT, "{}", unlinked.json);
    // The row stays, ended by the person.
    let listed = call(&w.t.db, "GET", &links_uri(&w.web), session(w.rita), None).await;
    let links = listed.json["links"].as_array().expect("links");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["unlink_source"], "manual");
    assert_eq!(links[0]["unlinked_by"], w.dave.get());
    assert!(links[0]["unlinked_at"].is_string());
    // Nothing is linked now.
    let gone = call(&w.t.db, "DELETE", &one, session(w.dave), None).await;
    assert_eq!(denied(&gone), (StatusCode::NOT_FOUND, "FG-REPO-0005"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_reporter_may_not_link_a_work_item() {
    let w = world().await;
    let one = format!("{}/PBI-43", links_uri(&w.web));
    for method in ["PUT", "DELETE"] {
        let refused = call(&w.t.db, method, &one, session(w.rita), None).await;
        assert_eq!(
            denied(&refused),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{method}"
        );
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn work_item_link_on_an_unbound_project_is_409() {
    let w = world().await;
    let one = format!("{}/PBI-43", links_uri(&w.api));
    let refused = call(&w.t.db, "PUT", &one, session(w.dave), None).await;
    assert_eq!(denied(&refused), (StatusCode::CONFLICT, "FG-INT-0018"));
    assert!(refused.json["request_id"].is_string(), "{}", refused.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_reference_that_is_not_canonical_is_refused_before_any_lookup() {
    let w = world().await;
    for raw in ["pbi-43", "PBI-043", "PBI%20%2343", "ICE-1", "PBI-12345678"] {
        let uri = format!("{}/{raw}", links_uri(&w.web));
        let refused = call(&w.t.db, "PUT", &uri, session(w.dave), None).await;
        assert_eq!(
            denied(&refused),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{raw}"
        );
    }
    w.t.drop_now().await;
}
