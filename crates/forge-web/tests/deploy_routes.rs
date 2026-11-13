#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Deployments through the product router (M4 C11, plan 14 §4.2,
//! ADR-0031) against the development database: a Maintainer sets the
//! environments, an administrator the targets; a Developer (an agent
//! included) requests; prod is approved only by a person in a web
//! session who is neither the requester nor an agent, a service account
//! or a token; a freeze window refuses the request. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Role, Scope};
use forge_core::{ProjectId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Every commit is on `refs/heads/main` (synthetic).
struct OnMain;

impl forge_deploy::CommitSource for OnMain {
    fn facts<'a>(
        &'a self,
        _: ProjectId,
        _: &'a forge_core::ObjectId,
    ) -> forge_deploy::FactsFuture<'a> {
        Box::pin(async {
            Ok(Some(forge_deploy::CommitFacts {
                subject: "Fix the cart".to_owned(),
                author_name: "Carol".to_owned(),
                author_email: "carol@example.test".to_owned(),
                committer_email: "carol@example.test".to_owned(),
                protected_branch: Some("refs/heads/main".to_owned()),
            }))
        })
    }
}

const HOST_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJ";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
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

struct World {
    t: FreshDb,
    owner: UserId,
    maintainer: UserId,
    developer: UserId,
    admin: UserId,
    web: ProjectId,
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        AccountKind::Human,
    )
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let user = |name: &'static str, admin: bool| {
            let db = t.db.clone();
            async move {
                users::create(
                    &db,
                    &ctx(&format!("req-user-{name}")),
                    &NewUser {
                        is_admin: admin,
                        ..NewUser::human(name, name, &format!("{name}@example.test"))
                    },
                )
                .await
                .expect("user")
            }
        };
        let owner = user("ada", false).await;
        let maintainer = user("bob", false).await;
        let developer = user("cy", false).await;
        let admin = user("root", true).await;
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        for (u, role) in [
            (maintainer.user_id, Role::Maintainer),
            (developer.user_id, Role::Developer),
        ] {
            projects::add_member(&t.db, &ctx("req-member"), web.project_id, u, role)
                .await
                .expect("member");
        }
        Self {
            t,
            owner: owner.user_id,
            maintainer: maintainer.user_id,
            developer: developer.user_id,
            admin: admin.user_id,
            web: web.project_id,
        }
    }

    async fn call(&self, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
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
        .with_deploy(forge_deploy::Deploy::new(
            self.t.db.clone(),
            Arc::new(OnMain),
            forge_core::deploy::TargetPolicy::default(),
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
        Answer {
            status,
            json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        }
    }

    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/{tail}", self.web.get())
    }

    async fn environment(&self, tier: &str, body: Value) -> Answer {
        self.call(
            "PUT",
            &self.uri(&format!("deploy/environments/{tier}")),
            session(self.owner, false),
            Some(body),
        )
        .await
    }

    async fn setup(&self) {
        // prod deploys only a SHA whose push pipeline succeeded.
        for c in ['a', 'b'] {
            self.t
                .exec_superuser(&format!(
                    "INSERT INTO forge.ci_pipelines (project_id, ref, sha, source, status, \
                       request_id, protected, copy_mark) VALUES ({}, 'refs/heads/main', '{}', \
                       'push', 'succeeded', 'req-pipe', true, 'open')",
                    self.web.get(),
                    c.to_string().repeat(40)
                ))
                .await;
        }
        for tier in ["stg", "prod"] {
            let prod = tier == "prod";
            let a = self
                .environment(
                    tier,
                    json!({"requires_approval": prod, "auto_rollback": !prod}),
                )
                .await;
            assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
            let t = self
                .call(
                    "PUT",
                    &self.uri(&format!("deploy/environments/{tier}/targets/web-1")),
                    session(self.admin, true),
                    Some(json!({
                        "host": "app1.internal", "user": "deploy",
                        "host_key_type": "ssh-ed25519", "host_key": HOST_KEY,
                        "script": "web-app"
                    })),
                )
                .await;
            assert_eq!(t.status, StatusCode::CREATED, "{}", t.json);
            assert!(t.json["host_key_fingerprint"]
                .as_str()
                .expect("fingerprint")
                .starts_with("SHA256:"));
            assert!(
                t.json.get("host_key").is_none(),
                "only the fingerprint is answered"
            );
        }
    }

    async fn request(&self, who: Principal, tier: &str, sha: char) -> Answer {
        self.call(
            "POST",
            &self.uri("deployments"),
            who,
            Some(json!({"environment": tier, "sha": sha.to_string().repeat(40)})),
        )
        .await
    }

    async fn approve(&self, who: Principal, id: i64) -> Answer {
        self.call(
            "POST",
            &self.uri(&format!("deployments/{id}/approve")),
            who,
            None,
        )
        .await
    }
}

#[tokio::test]
async fn only_a_person_in_a_session_approves_prod_and_never_their_own_request() {
    let w = World::new().await;
    w.setup().await;
    // A Developer (an agent's token too) may request; prod waits.
    let agent = token(w.developer, &[Scope::Api]).as_agent(true);
    let d = w.request(agent, "prod", 'a').await;
    assert_eq!(d.status, StatusCode::CREATED, "{}", d.json);
    assert_eq!(d.json["status"], "pending_approval");
    let id = d.json["id"].as_i64().expect("id");
    // Any token (an agent's too) is refused: approval is a web session.
    let a = w
        .approve(token(w.maintainer, &[Scope::Api]).as_agent(true), id)
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    let a = w.approve(token(w.maintainer, &[Scope::Api]), id).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    // The Developer requested it: the requester never approves (before
    // any role).
    let a = w.approve(session(w.developer, false), id).await;
    assert_eq!(a.code(), "FG-DEPLOY-0009");
    // A Maintainer in a session approves; a second approval conflicts.
    let a = w.approve(session(w.maintainer, false), id).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["status"], "queued");
    assert_eq!(a.json["approved_by"], w.maintainer.get());
    let again = w.approve(session(w.owner, false), id).await;
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::CONFLICT, "FG-DEPLOY-0010")
    );
    // The requester never approves their own.
    let mine = w.request(session(w.maintainer, false), "prod", 'b').await;
    let mine_id = mine.json["id"].as_i64().expect("id");
    let own = w.approve(session(w.maintainer, false), mine_id).await;
    assert_eq!(
        (own.status, own.code()),
        (StatusCode::FORBIDDEN, "FG-DEPLOY-0009")
    );
    // An unknown deployment is the invisible 404.
    let none = w.approve(session(w.owner, false), 999_999).await;
    assert_eq!(none.code(), "FG-REPO-0005");
    // The list shows both.
    let list = w
        .call(
            "GET",
            &w.uri("deployments"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json["deployments"].as_array().expect("list").len(), 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn targets_are_administrators_and_environments_are_maintainers() {
    let w = World::new().await;
    w.setup().await;
    let target_uri = w.uri("deploy/environments/stg/targets/web-2");
    let owner = w
        .call(
            "PUT",
            &target_uri,
            session(w.owner, false),
            Some(json!({
                "host": "app2.internal", "user": "deploy",
                "host_key_type": "ssh-ed25519", "host_key": HOST_KEY, "script": "web-app"
            })),
        )
        .await;
    assert_eq!(owner.code(), "FG-AUTH-0003", "an Owner is no administrator");
    let bad = w
        .call(
            "PUT",
            &w.uri("deploy/environments/stg/targets/web-2"),
            session(w.admin, true),
            Some(json!({
                "host": "-oProxyCommand=sh", "user": "deploy",
                "host_key_type": "ssh-ed25519", "host_key": HOST_KEY, "script": "web-app"
            })),
        )
        .await;
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let env = w
        .call(
            "PUT",
            &w.uri("deploy/environments/dev"),
            token(w.maintainer, &[Scope::Api]).as_agent(true),
            Some(json!({"requires_approval": false, "auto_rollback": true})),
        )
        .await;
    assert_eq!(
        env.code(),
        "FG-AUTH-0003",
        "an agent never sets an environment"
    );
    let prod = w
        .environment(
            "prod",
            json!({"requires_approval": true, "auto_rollback": true}),
        )
        .await;
    assert_eq!(
        (prod.status, prod.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    // A freeze window refuses the request.
    let frozen = w
        .environment(
            "stg",
            json!({"requires_approval": false, "auto_rollback": true, "freeze_windows": [
                {"starts_at": "2000-01-01T00:00:00Z", "ends_at": "2100-01-01T00:00:00Z"}
            ]}),
        )
        .await;
    assert_eq!(frozen.status, StatusCode::OK, "{}", frozen.json);
    assert_eq!(frozen.json["frozen_now"], true);
    let refused = w.request(session(w.developer, false), "stg", 'c').await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::CONFLICT, "FG-DEPLOY-0002")
    );
    let read = w
        .call(
            "GET",
            &w.uri("deploy/environments"),
            session(w.developer, false),
            None,
        )
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.json["environments"].as_array().expect("envs").len(), 2);
    w.t.drop_now().await;
}
