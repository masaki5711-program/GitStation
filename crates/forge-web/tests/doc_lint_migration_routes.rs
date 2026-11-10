#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The document lint migration routes (plan 13 Q13, ADR-0026) through the
//! product router against the development database (ADR-0011), with the
//! real loaders and commands: a Maintainer sets, reads and clears the date;
//! a Developer reads but is refused the write; an agent's token, a service
//! account, a token without `api` and a person outside a private project
//! are refused; a date that is no calendar date is `FG-WEB-0006`, one
//! outside the bounds (by the database's date) `FG-DOC-0005`; every change
//! is one audit row. Principals are inserted as request extensions.
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, ProjectId, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
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
        Box::pin(async {
            Err(forge_errors::ForgeError::new(
                forge_errors::ErrorCode::Unauthenticated,
            ))
        })
    }
}

/// The setting never looks a commit up.
struct NoCommits;

impl forge_checks::CommitProbe for NoCommits {
    fn reachable_commit<'a>(
        &'a self,
        _: RepoId,
        _: &'a ObjectId,
        _: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        Box::pin(async { panic!("no commit is looked up") })
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
    maintainer: UserId,
    dev: UserId,
    outsider: UserId,
    web: ProjectId,
}

impl World {
    async fn call(&self, method: &str, who: Principal, body: Option<Value>) -> Answer {
        let uri = format!("/api/v1/projects/{}/doc-lint/migration", self.web.get());
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
            Arc::new(NoTokens),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_checks(forge_checks::Checks::new(
            self.t.db.clone(),
            Arc::new(NoCommits),
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

    async fn date(&self, days: i32) -> String {
        sqlx::query_scalar("SELECT to_char((now() AT TIME ZONE 'UTC')::date + $1, 'YYYY-MM-DD')")
            .bind(days)
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("date")
    }

    async fn audit(&self) -> Vec<(String, Value, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, details, actor_user_id FROM forge.audit_log \
             WHERE action LIKE 'doc_lint_migration.%' AND resource_type = 'project' \
             AND resource_id = $1 ORDER BY id",
        )
        .bind(self.web.get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope], kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(scopes.to_vec()), kind)
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = person(&t, "alice").await;
    let maintainer = person(&t, "maya").await.user_id;
    let dev = person(&t, "dev").await.user_id;
    let outsider = person(&t, "outsider").await.user_id;
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
        outsider,
        web: web.project_id,
    }
}

#[tokio::test]
async fn a_maintainer_sets_reads_and_clears_the_migration_date() {
    let w = world().await;
    let today = w.date(0).await;
    let none = w.call("GET", session(w.dev), None).await;
    assert_eq!(none.status, StatusCode::OK, "{}", none.json);
    assert_eq!(
        none.json,
        json!({
            "project_id": w.web.get(), "warn_until": null, "in_force": false, "set_by": null,
            "set_at": null, "today": today, "max_until": w.date(180).await,
            "viewer": {"can_manage": false},
        })
    );
    let as_maintainer = w.call("GET", session(w.maintainer), None).await;
    assert_eq!(as_maintainer.json["viewer"]["can_manage"], true);

    let until = w.date(90).await;
    let set = w
        .call(
            "PUT",
            session(w.maintainer),
            Some(json!({"warn_until": until})),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.json);
    assert_eq!(set.json["warn_until"], json!(until));
    assert_eq!(set.json["in_force"], true);
    assert_eq!(set.json["set_by"], w.maintainer.get());
    assert!(set.json["set_at"].is_string());

    // A Maintainer's `api` token changes it too; the Developer reads it.
    let later = w.date(180).await;
    let changed = w
        .call(
            "PUT",
            token(w.maintainer, &[Scope::Api], AccountKind::Human),
            Some(json!({"warn_until": later})),
        )
        .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json);
    let read = w.call("GET", session(w.dev), None).await;
    assert_eq!(read.json["warn_until"], json!(later));
    assert_eq!(read.json["in_force"], true);

    let cleared = w.call("DELETE", session(w.maintainer), None).await;
    assert_eq!(cleared.status, StatusCode::NO_CONTENT, "{}", cleared.json);
    let gone = w.call("GET", session(w.dev), None).await;
    assert_eq!(gone.json["warn_until"], Value::Null);
    let again = w.call("DELETE", session(w.maintainer), None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.json);
    assert_eq!(again.code(), "FG-REPO-0005");

    // One audit row per change, by the person.
    let rows = w.audit().await;
    let actions: Vec<&str> = rows.iter().map(|(a, _, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "doc_lint_migration.set",
            "doc_lint_migration.set",
            "doc_lint_migration.cleared"
        ]
    );
    assert!(rows
        .iter()
        .all(|(_, _, by)| *by == Some(w.maintainer.get())));
    assert_eq!(rows[1].1["previous"], json!(until));
    assert_eq!(rows[2].1["warn_until"], json!(later));
}

#[tokio::test]
async fn a_migration_date_outside_the_bounds_is_refused() {
    let w = world().await;
    let maintainer = || session(w.maintainer);
    for (days, reason) in [(0, "today"), (-3, "past"), (181, "too far")] {
        let until = w.date(days).await;
        let a = w
            .call("PUT", maintainer(), Some(json!({"warn_until": until})))
            .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{reason}: {}", a.json);
        assert_eq!(a.code(), "FG-DOC-0005", "{reason}");
        assert!(a.json["user_message"].is_string());
    }
    for bad in ["2026-02-30", "2026-1-01", "tomorrow", ""] {
        let a = w
            .call("PUT", maintainer(), Some(json!({"warn_until": bad})))
            .await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{bad}: {}", a.json);
        assert_eq!(a.code(), "FG-WEB-0006", "{bad}");
    }
    // The date is required.
    let missing = w.call("PUT", maintainer(), Some(json!({}))).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert_eq!(missing.code(), "FG-WEB-0006");
    assert!(w.audit().await.is_empty(), "nothing stored");
}

#[tokio::test]
async fn only_maintainers_may_change_the_migration_date() {
    let w = world().await;
    let body = || Some(json!({"warn_until": "2099-01-01"}));
    for (who, status, code) in [
        (session(w.dev), StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        (session(w.outsider), StatusCode::NOT_FOUND, "FG-REPO-0005"),
        (
            token(w.maintainer, &[Scope::ReadApi], AccountKind::Human),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0002",
        ),
        (
            token(w.maintainer, &[Scope::Api], AccountKind::ServiceAccount),
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
    ] {
        for method in ["PUT", "DELETE"] {
            let a = w
                .call(
                    method,
                    who.clone(),
                    if method == "PUT" { body() } else { None },
                )
                .await;
            assert_eq!(a.status, status, "{method} {who:?}: {}", a.json);
            assert_eq!(a.code(), code, "{method} {who:?}");
        }
    }
    let outsider = w.call("GET", session(w.outsider), None).await;
    assert_eq!(outsider.status, StatusCode::NOT_FOUND);
    assert!(w.audit().await.is_empty());
}
