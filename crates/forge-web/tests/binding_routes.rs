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

const KEYS: &str = "1:BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY= active";

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
    let (status, raw) = send_raw(router, method, uri, who, body, headers).await;
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// [`send`], the body as the bytes on the wire.
async fn send_raw(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Who,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Vec<u8>) {
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
        // What the auth layer loads for a session (the screens show the person's name).
        req.extensions_mut().insert(forge_web::CallerProfile {
            username: "olga".to_owned(),
            display_name: "Olga".to_owned(),
            is_admin: true,
            is_agent: false,
            token_scopes: None,
        });
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
    (status, raw)
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
    /// Dave's private project she does not see.
    secret: i64,
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
    let secret = project(&dave, "spec-secret").await;
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
        secret: secret.get(),
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

/// Review (security MEDIUM / LOW b): a search acting for a person is
/// audited (person, integration, q) and limited per integration; the
/// service account on its own never searches; an unknown person counts
/// against the resolution failures.
#[tokio::test]
async fn act_as_repository_searches_are_audited_and_limited() {
    let w = world().await;
    let act_as = w.act_as();
    let search = |headers: Vec<(&'static str, String)>| {
        let router = w.router.clone();
        let who = w.kanflow();
        async move {
            let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            send(router, "GET", "/api/v1/repositories?q=spec", who, None, &h).await
        }
    };
    let (status, alone) = search(Vec::new()).await;
    assert_eq!(
        (status, code(&alone)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{alone}"
    );
    let (status, _) = search(vec![("forge-act-as", act_as.clone())]).await;
    assert_eq!(status, StatusCode::OK);
    let audit =
        w.t.json_rows(
            "SELECT actor_kind, actor_user_id, resource_type, resource_id, details \
             FROM forge.audit_log WHERE action = 'integration.repository_searched'",
        )
        .await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["actor_kind"], "user_via_service_account");
    assert_eq!(audit[0]["actor_user_id"], w.olga.get());
    assert_eq!(audit[0]["resource_type"], "integration");
    assert_eq!(audit[0]["resource_id"], w.integration);
    assert_eq!(audit[0]["details"]["q"], "spec");
    assert_eq!(audit[0]["details"]["results"], 1);
    // The per-integration budget of searches acting for a person.
    let mut refused = None;
    for _ in 0..forge_web::routes::integration_bindings::ACT_AS_SEARCHES_PER_MINUTE {
        let (status, body) = search(vec![("forge-act-as", act_as.clone())]).await;
        if status != StatusCode::OK {
            refused = Some((status, code(&body).to_owned()));
            break;
        }
    }
    assert_eq!(
        refused,
        Some((StatusCode::TOO_MANY_REQUESTS, "FG-WEB-0010".to_owned()))
    );
    // An unknown person is a resolution failure (counted, FG-INT-0006).
    let (status, unknown) = search(vec![("forge-act-as", "employee:E-nobody".to_owned())]).await;
    assert_eq!(
        (status, code(&unknown)),
        (StatusCode::FORBIDDEN, "FG-INT-0006")
    );
    // Every unknown resolution is counted, under the limit and over it.
    let mut tripped = None;
    for n in 0..200 {
        let (status, body) = search(vec![("forge-act-as", format!("employee:E-nobody-{n}"))]).await;
        if status != StatusCode::FORBIDDEN {
            tripped = Some((status, code(&body).to_owned()));
            break;
        }
        assert_eq!(code(&body), "FG-INT-0006");
    }
    assert_eq!(
        tripped,
        Some((StatusCode::TOO_MANY_REQUESTS, "FG-WEB-0010".to_owned()))
    );
    w.t.drop_now().await;
}

/// ADR-0025 Amendment X3 §4 (the screen promised for U3): what the binding
/// confirmation screen shows — forge's own facts only (the repository, the
/// kanflow project id the integration named, the setting, whether it is
/// bound already, the receiving integration, the person) — to the person's
/// web session with the intent's right (Maintainer); never to a token, never
/// for a Developer, strictly parsed.
#[tokio::test]
async fn the_binding_confirmation_screen_shows_forges_facts_to_the_person_only() {
    let w = world().await;
    let person = || Who::Person(session_of(w.olga), Some(w.olga_session));
    let uri = |project: i64, query: &str| {
        format!("/api/v1/projects/{project}/integration-bindings/act-as-confirmation?{query}")
    };
    let set_q = |integration: i64, resolves: bool| {
        format!("intent=binding_set&integration={integration}&kanflow_project_id=KF-7&resolves_work_items={resolves}")
    };
    let (status, shown) = send(
        w.router.clone(),
        "GET",
        &uri(w.spec, &set_q(w.integration, false)),
        person(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(shown["project_id"], json!(w.spec));
    assert_eq!(shown["project_path"], json!("olga/spec-zeva"));
    assert_eq!(shown["intent"], json!("binding_set"));
    assert_eq!(shown["kanflow_project_id"], json!("KF-7"));
    assert_eq!(shown["resolves_work_items"], json!(false));
    assert_eq!(shown["bound"], json!(false));
    assert_eq!(shown["integration_id"], json!(w.integration));
    assert_eq!(shown["integration_kind"], json!("kanflow"));
    assert_eq!(shown["integration_name"], json!("kanflow-sync"));
    assert_eq!(shown["person_username"], json!("olga"));

    // Bound (through the real confirm → claim → bind), the removal screen says so.
    let (assertion, _) = w
        .confirmed(w.spec, set(w.integration, "KF-7", false, &"s".repeat(43)))
        .await;
    let (status, bound) = w
        .bind_as_olga(
            Some(&assertion),
            json!({"kanflow_project_id": "KF-7", "repository_id": w.spec, "settings": {"resolves_work_items": false}}),
        )
        .await;
    assert_eq!(
        (status, bound["outcome"].clone()),
        (StatusCode::OK, json!("bound")),
        "{bound}"
    );
    let remove_q = format!(
        "intent=binding_remove&integration={}&kanflow_project_id=KF-7",
        w.integration
    );
    let (status, shown) = send(
        w.router.clone(),
        "GET",
        &uri(w.spec, &remove_q),
        person(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(
        (shown["bound"].clone(), shown["resolves_work_items"].clone()),
        (json!(true), Value::Null)
    );

    // A browser escapes `:` (allowed in a kanflow project id) as `%3A`.
    let (status, escaped) = send(
        w.router.clone(),
        "GET",
        &uri(
            w.spec,
            &format!(
                "intent=binding_set&integration={}&kanflow_project_id=KF%3A7&resolves_work_items=true",
                w.integration
            ),
        ),
        person(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{escaped}");
    assert_eq!(escaped["kanflow_project_id"], json!("KF:7"));

    // Never to a token (the integration's own included): a person's web session only.
    let (status, token) = send(
        w.router.clone(),
        "GET",
        &uri(w.spec, &set_q(w.integration, false)),
        w.kanflow(),
        None,
        &[],
    )
    .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "{status} {token}"
    );
    // A Developer of the project cannot bind it: refused like the issue route,
    // with the invisible project's 404 (ADR-0025, decision of 2026-11-10).
    let (status, dev) = send(
        w.router.clone(),
        "GET",
        &uri(w.devonly, &set_q(w.integration, false)),
        person(),
        None,
        &[],
    )
    .await;
    assert_eq!(
        (status, code(&dev)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{dev}"
    );
    // An integration that does not exist: the same 404 as an invisible project.
    let (status, none) = send(
        w.router.clone(),
        "GET",
        &uri(w.spec, &set_q(999_999, false)),
        person(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{none}");
    // Strict query: the setting only with binding_set, nothing unknown or repeated.
    for bad in [
        format!(
            "intent=binding_set&integration={}&kanflow_project_id=KF-7",
            w.integration
        ),
        format!(
            "intent=binding_remove&integration={}&kanflow_project_id=KF-7&resolves_work_items=true",
            w.integration
        ),
        format!(
            "intent=approve&integration={}&kanflow_project_id=KF-7",
            w.integration
        ),
        format!("{}&extra=1", set_q(w.integration, true)),
        format!("{}&intent=binding_set", set_q(w.integration, true)),
        format!(
            "intent=binding_set&integration={}&kanflow_project_id=&resolves_work_items=true",
            w.integration
        ),
        // A malformed escape, and an escape that decodes to a refused character.
        format!(
            "intent=binding_set&integration={}&kanflow_project_id=KF%2&resolves_work_items=true",
            w.integration
        ),
        format!(
            "intent=binding_set&integration={}&kanflow_project_id=KF%2F7&resolves_work_items=true",
            w.integration
        ),
    ] {
        let (status, refused) = send(
            w.router.clone(),
            "GET",
            &uri(w.spec, &bad),
            person(),
            None,
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {refused}");
    }
    w.t.drop_now().await;
}

/// The body of a refusal with its request id blanked (the only part that
/// may differ between two refusals).
fn without_request_id(status: StatusCode, raw: &[u8]) -> (StatusCode, String) {
    let body: Value = serde_json::from_slice(raw).expect("a JSON body");
    let id = body["request_id"].as_str().expect("request_id").to_owned();
    assert!(!id.is_empty());
    let text = String::from_utf8(raw.to_vec()).expect("utf-8");
    assert_eq!(text.matches(id.as_str()).count(), 1, "{text}");
    (status, text.replace(id.as_str(), "<request_id>"))
}

/// ADR-0025 (decision of 2026-11-10): whoever may not press the binding
/// confirmation — a Developer, a token, an integration that does not exist
/// or is disabled, a removal of what is not bound — gets the invisible
/// project's 404, byte for byte but the request id, on the screen's facts
/// (`GET`) and on the press (`POST`): nobody learns whether an integration
/// exists or what the binding state is.
#[tokio::test]
async fn every_refusal_of_the_binding_confirmation_is_the_invisible_projects_404() {
    let w = world().await;
    let disabled = forge_db::integrations::update(
        &w.t.db,
        &ctx("req-int-disable"),
        forge_core::IntegrationId::new(w.other_integration),
        &forge_db::integrations::Changes {
            enabled: Some(false),
            event_types: None,
        },
    )
    .await
    .expect("disable");
    assert_ne!(disabled, forge_db::integrations::UpdateOutcome::Unchanged);
    let person = || Who::Person(session_of(w.olga), Some(w.olga_session));
    let set_q = |integration: i64| {
        format!("intent=binding_set&integration={integration}&kanflow_project_id=KF-9&resolves_work_items=true")
    };
    let remove_q = format!(
        "intent=binding_remove&integration={}&kanflow_project_id=KF-9",
        w.integration
    );
    let set_b = |integration: i64| {
        json!({
            "intent": "binding_set", "integration": integration,
            "kanflow_project_id": "KF-9", "resolves_work_items": true,
            "state": "kanflow-state-refused-0123456789abcdef",
        })
    };
    let remove_b = json!({
        "intent": "binding_remove", "integration": w.integration,
        "kanflow_project_id": "KF-9",
        "state": "kanflow-state-refused-0123456789abcdef",
    });
    // (what, project, GET query, POST body, who)
    let cases = [
        (
            "invisible project",
            w.secret,
            set_q(w.integration),
            set_b(w.integration),
            person(),
        ),
        (
            "absent project",
            999_999,
            set_q(w.integration),
            set_b(w.integration),
            person(),
        ),
        (
            "developer",
            w.devonly,
            set_q(w.integration),
            set_b(w.integration),
            person(),
        ),
        (
            "missing integration",
            w.spec,
            set_q(999_999),
            set_b(999_999),
            person(),
        ),
        (
            "disabled integration",
            w.spec,
            set_q(w.other_integration),
            set_b(w.other_integration),
            person(),
        ),
        (
            "removal of what is not bound",
            w.spec,
            remove_q.clone(),
            remove_b.clone(),
            person(),
        ),
        (
            "a token",
            w.spec,
            set_q(w.integration),
            set_b(w.integration),
            w.kanflow(),
        ),
    ];
    let mut answers = Vec::new();
    for (what, project, query, body, who) in cases {
        let (status, raw) = send_raw(
            w.router.clone(),
            "GET",
            &format!("/api/v1/projects/{project}/integration-bindings/act-as-confirmation?{query}"),
            who.clone(),
            None,
            &[],
        )
        .await;
        answers.push((format!("GET {what}"), without_request_id(status, &raw)));
        let (status, raw) = send_raw(
            w.router.clone(),
            "POST",
            &format!("/-/confirm/projects/{project}/integration-bindings/assertions"),
            who,
            Some(body),
            &[],
        )
        .await;
        answers.push((format!("POST {what}"), without_request_id(status, &raw)));
    }
    let (first, expected) = &answers[0];
    assert_eq!(expected.0, StatusCode::NOT_FOUND, "{first}: {}", expected.1);
    assert!(expected.1.contains("FG-REPO-0005"), "{}", expected.1);
    for (what, answer) in &answers {
        assert_eq!(answer, expected, "{what} differs from {first}");
    }
    // Nothing was written by any refused press.
    let rows =
        w.t.json_rows("SELECT count(*) AS n FROM forge.act_as_assertions")
            .await;
    assert_eq!(rows[0]["n"], json!(0));
    w.t.drop_now().await;
}
