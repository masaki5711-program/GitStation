#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! A kanflow project's repositories through the product router (M3 X3,
//! plan 03 §7.1, ADR-0016 Amendment X3, ADR-0025 Amendment X3), against
//! the development database with the real token authenticator for the
//! integration's service account and injected sessions for the people:
//! the repository search acting for a person (only what they could bind),
//! the confirmation, the claim, the bind / change / removal acting for the
//! person (audited as both), the shared spec repository in two kanflow
//! projects, the code repository refused a second, the integration's list,
//! and the refusals (the service account on its own, no assertion, another
//! setting, another integration). Synthetic data only.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::act_as::ActAsKeys;
use forge_core::{RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_web::SessionCredential;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

const KEYS: &str = "1:BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc= active";

#[derive(Clone)]
enum Who {
    Person(Principal, Option<i64>),
    Bearer(String),
}

fn session_of(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Who,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Who::Bearer(token) = &who {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    if let Who::Person(principal, session) = who {
        req.extensions_mut().insert(principal);
        if let Some(id) = session {
            req.extensions_mut().insert(SessionCredential {
                session_id: id,
                public_id: "00000000000000a1".to_owned(),
                csrf_token: "c5rf".repeat(16),
                idle_expires_at: "2099-01-01T00:00:00Z".to_owned(),
                absolute_expires_at: "2099-01-01T00:00:00Z".to_owned(),
            });
        }
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

struct World {
    t: FreshDb,
    router: axum::Router,
    /// A Maintainer of `spec` and `web` (an administrator too: no
    /// administrator right passes acting for her).
    olga: UserId,
    olga_session: i64,
    integration: i64,
    other_integration: i64,
    /// kanflow's token: read_api, api, act_as.
    kanflow: String,
    spec: i64,
    web: i64,
    /// One where she is only a Developer.
    devonly: i64,
}

async fn sa_with_integration(t: &FreshDb, name: &str, owner: UserId) -> (UserId, i64) {
    let sa = forge_db::service_accounts::create(
        &t.db,
        &ctx(&format!("req-sa-{name}")),
        &forge_db::service_accounts::NewServiceAccount {
            username: name.to_owned(),
            display_name: name.to_owned(),
            owner,
            purpose: "kanflow binds repositories for its PO / SM".to_owned(),
        },
        owner,
    )
    .await
    .expect("sa")
    .user_id;
    let integration = forge_db::integrations::create(
        &t.db,
        &ctx(&format!("req-int-{name}")),
        &forge_db::integrations::NewIntegration {
            kind: forge_core::IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        owner,
    )
    .await
    .expect("integration");
    (sa, integration.get())
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut olga_new = NewUser::human("olga", "Olga", "olga@example.test");
    olga_new.is_admin = true;
    let olga = users::create(&t.db, &ctx("req-olga"), &olga_new)
        .await
        .expect("olga");
    let dave = users::create(
        &t.db,
        &ctx("req-dave"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dave");
    let project = |owner: &users::Created, name: &'static str| {
        let t = &t;
        let (ns, by) = (owner.namespace_id, owner.user_id);
        async move {
            projects::create(
                &t.db,
                &ctx(&format!("req-p-{name}")),
                &NewProject::new(name, ns, name, by),
            )
            .await
            .expect("project")
            .project_id
        }
    };
    let spec = project(&olga, "spec-zeva").await;
    let web = project(&olga, "web").await;
    // Dave's private project she does not belong to.
    project(&dave, "spec-secret").await;
    let devonly = project(&dave, "spec-shared").await;
    projects::add_member(&t.db, &ctx("req-m"), devonly, olga.user_id, Role::Developer)
        .await
        .expect("member");
    let (sa, integration) = sa_with_integration(&t, "kanflow-sync", olga.user_id).await;
    let (_, other_integration) = sa_with_integration(&t, "kanflow-two", olga.user_id).await;
    let kanflow = forge_db::tokens::issue_for_service_account(
        &t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: vec![Scope::ReadApi, Scope::Api, Scope::ActAs],
            expires_in: None,
            is_agent: false,
        },
        olga.user_id,
    )
    .await
    .expect("token")
    .token
    .reveal();
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(t.db.clone()),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_hosting(forge_hosting::Hosting::new(t.db.clone()))
    .with_act_as(forge_integ::ActAs::new(
        t.db.clone(),
        Some(ActAsKeys::parse(KEYS).expect("keys")),
    ));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let olga_session = forge_db::sessions::issue(
        &t.db,
        &ctx("req-session"),
        &forge_db::sessions::NewSession {
            user: olga.user_id,
            idle: Duration::from_secs(3600),
            absolute: Duration::from_secs(8 * 3600),
            oidc: None,
        },
    )
    .await
    .expect("session")
    .session_id;
    World {
        t,
        router,
        olga: olga.user_id,
        olga_session,
        integration,
        other_integration,
        kanflow,
        spec: spec.get(),
        web: web.get(),
        devonly: devonly.get(),
    }
}

impl World {
    fn kanflow(&self) -> Who {
        Who::Bearer(self.kanflow.clone())
    }

    fn act_as(&self) -> String {
        format!("user:{}", self.olga.get())
    }

    /// Olga confirms `body` for `project` in forge; kanflow claims it.
    async fn confirmed(&self, project: i64, body: Value) -> (String, Value) {
        let (status, issued) = send(
            self.router.clone(),
            "POST",
            &format!("/-/confirm/projects/{project}/integration-bindings/assertions"),
            Who::Person(session_of(self.olga), Some(self.olga_session)),
            Some(body.clone()), // clone: the claim reuses the state
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{issued}");
        let (status, claimed) = send(
            self.router.clone(),
            "POST",
            &format!("/api/v1/integrations/{}/act-as/claim", self.integration),
            self.kanflow(),
            Some(json!({ "state": body["state"] })),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{claimed}");
        let assertion = claimed["assertion"].as_str().expect("assertion").to_owned();
        (assertion, claimed)
    }

    async fn bind_as_olga(&self, assertion: Option<&str>, body: Value) -> (StatusCode, Value) {
        let act_as = self.act_as();
        let mut headers = vec![("forge-act-as", act_as.as_str())];
        if let Some(a) = assertion {
            headers.push(("forge-assertion", a));
        }
        send(
            self.router.clone(),
            "POST",
            &format!("/api/v1/integrations/{}/bindings", self.integration),
            self.kanflow(),
            Some(body),
            &headers,
        )
        .await
    }
}

fn set(integration: i64, kanflow: &str, resolves: bool, state: &str) -> Value {
    json!({
        "intent": "binding_set",
        "integration": integration,
        "kanflow_project_id": kanflow,
        "resolves_work_items": resolves,
        "state": state,
    })
}

/// Plan 03 §7.1: the search shows only what the person could bind; a
/// shared spec repository is bound to two kanflow projects acting for the
/// person who confirmed each; the integration lists them; the audit names
/// both the person and the service account.
#[tokio::test]
async fn kanflow_binds_a_shared_spec_repository_to_two_projects_as_the_person() {
    let w = world().await;
    let act_as = w.act_as();
    let (status, found) = send(
        w.router.clone(),
        "GET",
        "/api/v1/repositories?q=SPEC",
        w.kanflow(),
        None,
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    let ids: Vec<i64> = found["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["id"].as_i64())
        .collect();
    // Her own spec repository only: not dave's (no administrator right
    // passes acting for her), not one where she is a Developer.
    assert_eq!(ids, vec![w.spec], "{found}");
    assert!(!found.to_string().contains("spec-secret"));
    assert!(found.get("total").is_none());
    for (n, kanflow) in ["KF-1", "KF-2"].iter().enumerate() {
        let state = format!("kanflow-state-bind-{n}-0123456789abcdef");
        let (assertion, claimed) = w
            .confirmed(w.spec, set(w.integration, kanflow, false, &state))
            .await;
        assert_eq!(claimed["intent"], "binding_set");
        assert_eq!(claimed["kanflow_project_id"], *kanflow);
        assert_eq!(claimed["resolves_work_items"], false);
        assert_eq!(claimed["iid"], Value::Null);
        let (status, bound) = w
            .bind_as_olga(
                Some(&assertion),
                json!({
                    "kanflow_project_id": kanflow,
                    "repository_id": w.spec,
                    "settings": {"resolves_work_items": false},
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{bound}");
        assert_eq!(bound["outcome"], "bound");
        assert_eq!(bound["binding"]["settings"]["resolves_work_items"], false);
    }
    let (status, listed) = send(
        w.router.clone(),
        "GET",
        &format!(
            "/api/v1/integrations/{}/bindings?kanflow_project_id=KF-1",
            w.integration
        ),
        w.kanflow(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["repository_path"], "olga/spec-zeva");
    let audit =
        w.t.json_rows(
            "SELECT actor_kind, actor_user_id, via_assertion_id FROM forge.audit_log \
             WHERE action = 'integration.bound' ORDER BY id",
        )
        .await;
    assert_eq!(audit.len(), 2, "{audit:?}");
    assert_eq!(audit[0]["actor_kind"], "user_via_service_account");
    assert_eq!(audit[0]["actor_user_id"], w.olga.get());
    let consumed =
        w.t.json_rows("SELECT count(*) AS n FROM forge.act_as_assertions WHERE state = 'consumed'")
            .await;
    assert_eq!(consumed[0]["n"], 2);
    w.t.drop_now().await;
}

/// The integration's service account never binds on its own; acting for a
/// person it needs the person's assertion, for this setting and this
/// integration; a repository with code is refused a second kanflow
/// project; a removal needs its own assertion.
#[tokio::test]
async fn binding_refusals_and_a_removal_acting_for_the_person() {
    let w = world().await;
    let body = |kanflow: &str, repo: i64, resolves: bool| {
        json!({
            "kanflow_project_id": kanflow,
            "repository_id": repo,
            "settings": {"resolves_work_items": resolves},
        })
    };
    // The service account on its own, on a project it is no member of:
    // the same 404 as any invisible project.
    let (status, own) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/integrations/{}/bindings", w.integration),
        w.kanflow(),
        Some(body("KF-1", w.web, true)),
        &[],
    )
    .await;
    assert_eq!(
        (status, code(&own)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{own}"
    );
    // Acting for her without an assertion.
    let (status, missing) = w.bind_as_olga(None, body("KF-1", w.web, true)).await;
    assert_eq!(
        (status, code(&missing)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{missing}"
    );
    // An assertion for the shared setting used to bind as resolving.
    let (assertion, _) = w
        .confirmed(
            w.web,
            set(
                w.integration,
                "KF-1",
                false,
                "kanflow-state-mismatch-0123456789",
            ),
        )
        .await;
    let (status, other) = w
        .bind_as_olga(Some(&assertion), body("KF-1", w.web, true))
        .await;
    assert_eq!(
        (status, code(&other)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0020"),
        "{other}"
    );
    // Another integration's route with this integration's token.
    let (assertion, _) = w
        .confirmed(
            w.web,
            set(
                w.integration,
                "KF-1",
                true,
                "kanflow-state-otherint-0123456789",
            ),
        )
        .await;
    let act_as = w.act_as();
    let (status, elsewhere) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/integrations/{}/bindings", w.other_integration),
        w.kanflow(),
        Some(body("KF-1", w.web, true)),
        &[("forge-act-as", &act_as), ("forge-assertion", &assertion)],
    )
    .await;
    assert_eq!(
        (status, code(&elsewhere)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // The code repository, bound by the person herself (her session).
    let (status, bound) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/integrations/{}/bindings", w.integration),
        Who::Person(session_of(w.olga), None),
        Some(body("KF-1", w.web, true)),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bound}");
    let binding_id = bound["binding"]["binding_id"].as_i64().expect("id");
    // Now a Reporter of it (the binding granted it), the service account on
    // its own is still refused: binding is a person's action.
    let (status, own) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/integrations/{}/bindings", w.integration),
        w.kanflow(),
        Some(body("KF-2", w.web, true)),
        &[],
    )
    .await;
    assert_eq!(
        (status, code(&own)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{own}"
    );
    // A second kanflow project for it: refused.
    let (assertion, _) = w
        .confirmed(
            w.web,
            set(
                w.integration,
                "KF-2",
                true,
                "kanflow-state-second-0123456789ab",
            ),
        )
        .await;
    let (status, second) = w
        .bind_as_olga(Some(&assertion), body("KF-2", w.web, true))
        .await;
    assert_eq!(
        (status, code(&second)),
        (StatusCode::CONFLICT, "FG-INT-0016"),
        "{second}"
    );
    // The search hides a repository she is only a Developer of.
    let (_, found) = send(
        w.router.clone(),
        "GET",
        "/api/v1/repositories?q=shared",
        w.kanflow(),
        None,
        &[("forge-act-as", &act_as)],
    )
    .await;
    assert_eq!(found["items"], json!([]), "{found} ({})", w.devonly);
    // A removal: its own confirmation and assertion.
    let (assertion, claimed) = w
        .confirmed(
            w.web,
            json!({
                "intent": "binding_remove",
                "integration": w.integration,
                "kanflow_project_id": "KF-1",
                "state": "kanflow-state-remove-0123456789ab",
            }),
        )
        .await;
    assert_eq!(claimed["resolves_work_items"], Value::Null);
    let path = format!(
        "/api/v1/integrations/{}/bindings/{binding_id}",
        w.integration
    );
    let (status, removed) = send(
        w.router.clone(),
        "DELETE",
        &path,
        w.kanflow(),
        None,
        &[("forge-act-as", &act_as), ("forge-assertion", &assertion)],
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{removed}");
    // Gone: the same 404 as an invisible project's binding.
    let (status, gone) = send(
        w.router.clone(),
        "DELETE",
        &path,
        Who::Person(session_of(w.olga), None),
        None,
        &[],
    )
    .await;
    assert_eq!(
        (status, code(&gone)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    w.t.drop_now().await;
}
