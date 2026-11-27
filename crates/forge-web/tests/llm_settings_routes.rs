#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The local LLM's settings through the product router (plan 19 §8.2)
//! against the development database: a project's `llm.review` (off by
//! default; a Maintainer's web session changes it, one audit row per
//! change; a Developer, a person's token, an agent's token, a bot's and a
//! service account's sessions are refused and write nothing) and the
//! gateway's pause and resume (an administrator, a session or an `admin`
//! token; never an agent's token, a non-administrator or a token without
//! `api` + `admin`). Principals are inserted as request extensions.
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The settings never look a branch up.
struct NoBranches;

impl forge_review::BranchProbe for NoBranches {
    fn branch_head<'a>(
        &'a self,
        _: RepoId,
        _: &'a RefName,
        _: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        Box::pin(async { panic!("no branch is looked up") })
    }
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

struct World {
    t: FreshDb,
    maintainer: UserId,
    dev: UserId,
    admin: UserId,
    web: ProjectId,
}

fn session(user: UserId, kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, kind)
}

fn admin_session(user: UserId) -> Principal {
    Principal::authenticated(user, true, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, is_admin: bool, scopes: &[Scope], agent: bool) -> Principal {
    let p = Principal::authenticated(
        user,
        is_admin,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    );
    if agent {
        p.as_agent(true)
    } else {
        p
    }
}

impl World {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        who: Principal,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        req.extensions_mut().insert(who);
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(self.t.db.clone()),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_review(forge_review::Review::new(
            self.t.db.clone(),
            Arc::new(NoBranches),
        ));
        let res = forge_web::app(state, AppConfig::default())
            .expect("router")
            .oneshot(req)
            .await
            .expect("infallible");
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

    fn setting(&self) -> String {
        format!("/api/v1/projects/{}/llm", self.web.get())
    }

    async fn audit(&self, like: &str) -> Vec<(String, Value, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, details, actor_user_id FROM forge.audit_log \
             WHERE action LIKE $1 ORDER BY id",
        )
        .bind(like)
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = person(&t, "alice", false).await;
    let maintainer = person(&t, "maya", false).await.user_id;
    let dev = person(&t, "dev", false).await.user_id;
    let admin = person(&t, "olga", true).await.user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-maintainer"),
        web.project_id,
        maintainer,
        Role::Maintainer,
    )
    .await
    .expect("maintainer");
    projects::add_member(&t.db, &ctx("req-dev"), web.project_id, dev, Role::Developer)
        .await
        .expect("developer");
    World {
        t,
        maintainer,
        dev,
        admin,
        web: web.project_id,
    }
}

#[tokio::test]
async fn a_maintainer_sets_llm_review_in_a_session_audited() {
    let w = world().await;
    let uri = w.setting();
    let (status, off) = w
        .call("GET", &uri, session(w.dev, AccountKind::Human), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(
        off,
        json!({"project_id": w.web.get(), "review": "off", "changed_by": null,
               "changed_at": null, "viewer": {"can_change": false}})
    );
    let (_, seen) = w
        .call("GET", &uri, session(w.maintainer, AccountKind::Human), None)
        .await;
    assert_eq!(seen["viewer"]["can_change"], true);
    for mode in ["advisory", "required", "off"] {
        let (status, set) = w
            .call(
                "PUT",
                &uri,
                session(w.maintainer, AccountKind::Human),
                Some(json!({ "review": mode })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{set}");
        assert_eq!(set["review"], mode);
        assert_eq!(set["changed_by"], w.maintainer.get());
    }
    let (_, read) = w
        .call("GET", &uri, session(w.dev, AccountKind::Human), None)
        .await;
    assert_eq!(read["review"], "off");
    let rows = w.audit("llm.review%").await;
    assert_eq!(rows.len(), 3);
    assert!(rows
        .iter()
        .all(|(a, _, by)| a == "llm.review_changed" && *by == Some(w.maintainer.get())));
    assert_eq!(
        rows[0].1,
        json!({"project_id": w.web.get(), "from": "off", "to": "advisory"})
    );
    let (status, bad) = w
        .call(
            "PUT",
            &uri,
            session(w.maintainer, AccountKind::Human),
            Some(json!({"review": "on"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(w.audit("llm.review%").await.len(), 3);
    w.t.drop_now().await;
}

#[tokio::test]
async fn llm_review_is_refused_to_everyone_but_a_maintainers_session() {
    let w = world().await;
    let uri = w.setting();
    let refused = [
        ("developer", session(w.dev, AccountKind::Human)),
        (
            "maintainer token",
            token(w.maintainer, false, &[Scope::Api], false),
        ),
        (
            "agent token",
            token(w.maintainer, false, &[Scope::Api], true),
        ),
        ("bot session", session(w.maintainer, AccountKind::Bot)),
        (
            "service account session",
            session(w.maintainer, AccountKind::ServiceAccount),
        ),
    ];
    for (who, principal) in refused {
        let (status, body) = w
            .call("PUT", &uri, principal, Some(json!({"review": "required"})))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
        assert!(
            body["error_code"]
                .as_str()
                .is_some_and(|c| c.starts_with("FG-AUTH-")),
            "{who}: {body}"
        );
    }
    assert!(w.audit("llm.review%").await.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_administrator_pauses_and_resumes_the_gateway_audited() {
    let w = world().await;
    let (status, paused) = w
        .call(
            "POST",
            "/api/v1/admin/llm/pause",
            admin_session(w.admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["paused"], true);
    assert_eq!(paused["outcome"], "changed");
    assert_eq!(paused["changed_by"], w.admin.get());
    let (status, again) = w
        .call(
            "POST",
            "/api/v1/admin/llm/pause",
            token(w.admin, true, &[Scope::Api, Scope::Admin], false),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["outcome"], "unchanged", "{again}");
    assert!(
        forge_db::llm::ops::gateway_pause(&w.t.db)
            .await
            .expect("read")
            .paused
    );
    let (status, resumed) = w
        .call(
            "POST",
            "/api/v1/admin/llm/resume",
            admin_session(w.admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(resumed["paused"], false);
    let rows = w.audit("llm.%u_ed").await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["llm.paused", "llm.resumed"]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_pause_is_refused_to_agents_and_non_administrators() {
    let w = world().await;
    for path in ["/api/v1/admin/llm/pause", "/api/v1/admin/llm/resume"] {
        let refused = [
            (
                "maintainer session",
                session(w.maintainer, AccountKind::Human),
            ),
            (
                "admin agent token",
                token(w.admin, true, &[Scope::Api, Scope::Admin], true),
            ),
            (
                "admin api token",
                token(w.admin, true, &[Scope::Api], false),
            ),
            (
                "admin read token",
                token(w.admin, true, &[Scope::ReadApi, Scope::Admin], false),
            ),
        ];
        for (who, principal) in refused {
            let (status, body) = w.call("POST", path, principal, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} {who}: {body}");
        }
    }
    assert!(
        !forge_db::llm::ops::gateway_pause(&w.t.db)
            .await
            .expect("read")
            .paused
    );
    assert!(w.audit("llm.%u_ed").await.is_empty());
    w.t.drop_now().await;
}
