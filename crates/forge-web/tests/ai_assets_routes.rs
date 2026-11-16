#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `ai_assets.enabled` through the product router (plan 17 P17-3,
//! ADR-0035) against the development database: off by default; a
//! Maintainer's web session turns it on and off, each change one audit row;
//! a Developer, a person's token, an agent's token, a bot's and a service
//! account's sessions are refused and write nothing. Principals are
//! inserted as request extensions. Synthetic data only.

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

/// The setting never looks a branch up.
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

async fn person(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(
            name,
            &format!("Synthetic {name}"),
            &format!("{name}@example.test"),
        ),
    )
    .await
    .expect("user")
}

struct World {
    t: FreshDb,
    maintainer: UserId,
    dev: UserId,
    web: ProjectId,
}

fn session(user: UserId, kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, kind)
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    let p = Principal::authenticated(
        user,
        false,
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
    async fn call(&self, method: &str, who: Principal, body: Option<Value>) -> (StatusCode, Value) {
        let uri = format!("/api/v1/projects/{}/ai-assets", self.web.get());
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

    async fn audit(&self) -> Vec<(String, Value, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, details, actor_user_id FROM forge.audit_log \
             WHERE action LIKE 'ai_assets.%' AND resource_type = 'project' \
             AND resource_id = $1 ORDER BY id",
        )
        .bind(self.web.get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = person(&t, "alice").await;
    let maintainer = person(&t, "maya").await.user_id;
    let dev = person(&t, "dev").await.user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("assets", owner.namespace_id, "Assets", owner.user_id),
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
        web: web.project_id,
    }
}

#[tokio::test]
async fn a_maintainer_turns_ai_assets_on_and_off_in_a_session_audited() {
    let w = world().await;
    let (status, off) = w
        .call("GET", session(w.dev, AccountKind::Human), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{off}");
    assert_eq!(
        off,
        json!({"project_id": w.web.get(), "enabled": false, "changed_by": null,
               "changed_at": null, "viewer": {"can_change": false}})
    );
    let (_, seen) = w
        .call("GET", session(w.maintainer, AccountKind::Human), None)
        .await;
    assert_eq!(seen["viewer"]["can_change"], true);

    let (status, on) = w
        .call(
            "PUT",
            session(w.maintainer, AccountKind::Human),
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{on}");
    assert_eq!(on["enabled"], true);
    assert_eq!(on["changed_by"], w.maintainer.get());
    let (_, read) = w
        .call("GET", session(w.dev, AccountKind::Human), None)
        .await;
    assert_eq!(read["enabled"], true);

    let (status, again) = w
        .call(
            "PUT",
            session(w.maintainer, AccountKind::Human),
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["enabled"], false);
    let rows = w.audit().await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["ai_assets.enabled", "ai_assets.disabled"]);
    assert!(rows
        .iter()
        .all(|(_, _, by)| *by == Some(w.maintainer.get())));
    assert_eq!(
        rows[0].1,
        json!({"project_id": w.web.get(), "from": false, "to": true})
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn ai_assets_is_refused_to_everyone_but_a_maintainers_session() {
    let w = world().await;
    let refused = [
        (
            "developer",
            session(w.dev, AccountKind::Human),
            StatusCode::FORBIDDEN,
        ),
        (
            "maintainer token",
            token(w.maintainer, &[Scope::Api], false),
            StatusCode::FORBIDDEN,
        ),
        (
            "agent token",
            token(w.maintainer, &[Scope::Api], true),
            StatusCode::FORBIDDEN,
        ),
        (
            "bot session",
            session(w.maintainer, AccountKind::Bot),
            StatusCode::FORBIDDEN,
        ),
        (
            "service account session",
            session(w.maintainer, AccountKind::ServiceAccount),
            StatusCode::FORBIDDEN,
        ),
    ];
    for (who, principal, want) in refused {
        let (status, body) = w
            .call("PUT", principal, Some(json!({"enabled": true})))
            .await;
        assert_eq!(status, want, "{who}: {body}");
        assert!(
            body["error_code"]
                .as_str()
                .is_some_and(|c| c.starts_with("FG-AUTH-")),
            "{who}: {body}"
        );
    }
    assert!(w.audit().await.is_empty());
    let (_, read) = w
        .call("GET", session(w.dev, AccountKind::Human), None)
        .await;
    assert_eq!(read["enabled"], false);
    w.t.drop_now().await;
}
