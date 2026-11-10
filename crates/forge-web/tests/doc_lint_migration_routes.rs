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
    headers: axum::http::HeaderMap,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

/// Who calls: a principal as a request extension, nobody, or a real token
/// through the authenticator.
#[derive(Debug, Clone)]
enum Who {
    P(Principal),
    Nobody,
    Bearer(String),
}

impl From<Principal> for Who {
    fn from(p: Principal) -> Self {
        Who::P(p)
    }
}

struct World {
    t: FreshDb,
    maintainer: UserId,
    dev: UserId,
    outsider: UserId,
    web: ProjectId,
    /// Another person's private project the outsider cannot see.
    hidden: ProjectId,
}

impl World {
    async fn call(&self, method: &str, who: impl Into<Who>, body: Option<Value>) -> Answer {
        self.call_on(self.web, method, who.into(), body, &[]).await
    }

    async fn call_on(
        &self,
        project: ProjectId,
        method: &str,
        who: Who,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Answer {
        let uri = format!("/api/v1/projects/{}/doc-lint/migration", project.get());
        let mut builder = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        if let Who::Bearer(token) = &who {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        if let Who::P(p) = who {
            req.extensions_mut().insert(p);
        }
        let state = AppState::new(
            "test",
            Vec::new(),
            // The real token authenticator (the credential of the audit row).
            Arc::new(self.t.db.clone()),
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
        let headers = res.headers().clone();
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
            headers,
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
        self.audit_full()
            .await
            .into_iter()
            .map(|(a, d, u, _, _)| (a, d, u))
            .collect()
    }

    /// action, details, actor_user_id, actor_kind, credential_id.
    async fn audit_full(&self) -> Vec<(String, Value, Option<i64>, String, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, details, actor_user_id, actor_kind, credential_id FROM forge.audit_log \
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
    let hidden = projects::create(
        &t.db,
        &ctx("req-hidden"),
        &NewProject::new("hidden", owner.namespace_id, "Hidden", owner.user_id),
    )
    .await
    .expect("hidden")
    .project_id;
    World {
        t,
        maintainer,
        dev,
        outsider,
        web: web.project_id,
        hidden,
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

/// A caller, the extra request headers, the refusal's status and code.
type DenyCase = (
    Who,
    &'static [(&'static str, &'static str)],
    StatusCode,
    &'static str,
);

#[tokio::test]
async fn only_maintainers_may_change_the_migration_date() {
    let w = world().await;
    let body = || Some(json!({"warn_until": "2099-01-01"}));
    let sa = token(w.maintainer, &[Scope::Api], AccountKind::ServiceAccount);
    let cases: Vec<DenyCase> = vec![
        (
            session(w.dev).into(),
            &[],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            session(w.outsider).into(),
            &[],
            StatusCode::NOT_FOUND,
            "FG-REPO-0005",
        ),
        (Who::Nobody, &[], StatusCode::UNAUTHORIZED, "FG-AUTH-0001"),
        (
            token(w.maintainer, &[Scope::ReadApi], AccountKind::Human).into(),
            &[],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0002",
        ),
        (
            sa.clone().into(),
            &[],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            token(w.maintainer, &[Scope::Api], AccountKind::Bot).into(),
            &[],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        (
            token(w.maintainer, &[Scope::Api], AccountKind::Human)
                .as_agent(true)
                .into(),
            &[],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
        // Never acting for a person (ADR-0025 §4).
        (
            token(
                w.maintainer,
                &[Scope::Api, Scope::ActAs],
                AccountKind::ServiceAccount,
            )
            .into(),
            &[("Forge-Act-As", "maya")],
            StatusCode::FORBIDDEN,
            "FG-AUTH-0003",
        ),
    ];
    for (who, headers, status, code) in &cases {
        for method in ["PUT", "DELETE"] {
            let a = w
                .call_on(
                    w.web,
                    method,
                    who.clone(),
                    if method == "PUT" { body() } else { None },
                    headers,
                )
                .await;
            assert_eq!(a.status, *status, "{method} {who:?}: {}", a.json);
            assert_eq!(a.code(), *code, "{method} {who:?}");
        }
    }
    // Reading: anonymous is asked to sign in; an outsider does not see it.
    let anonymous = w.call_on(w.web, "GET", Who::Nobody, None, &[]).await;
    assert_eq!(
        anonymous.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        anonymous.json
    );
    let outsider = w.call("GET", session(w.outsider), None).await;
    assert_eq!(outsider.status, StatusCode::NOT_FOUND);
    assert!(w.audit().await.is_empty());
}

/// A project that does not exist answers exactly like a private project
/// the caller cannot see: same status, body (but the request id) and
/// headers (but the request id), for every method.
#[tokio::test]
async fn a_missing_project_answers_like_an_invisible_one() {
    let w = world().await;
    let strip = |a: &Answer| {
        let mut json = a.json.clone(); // clone: compared without its id
        json.as_object_mut().expect("object").remove("request_id");
        let mut headers: Vec<(String, String)> = a
            .headers
            .iter()
            .filter(|(k, _)| !k.as_str().contains("request-id"))
            .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
            .collect();
        headers.sort();
        (a.status, json, headers)
    };
    for method in ["GET", "PUT", "DELETE"] {
        let body = || (method == "PUT").then(|| json!({"warn_until": "2099-01-01"}));
        let who = || Who::P(session(w.outsider));
        let hidden = w.call_on(w.hidden, method, who(), body(), &[]).await;
        let missing = w
            .call_on(ProjectId::new(987_654), method, who(), body(), &[])
            .await;
        assert_eq!(
            hidden.status,
            StatusCode::NOT_FOUND,
            "{method}: {}",
            hidden.json
        );
        assert_eq!(hidden.code(), "FG-REPO-0005");
        assert_eq!(strip(&missing), strip(&hidden), "{method}");
    }
}

/// Every change is one audit row on the project: who, with which
/// credential, the date and the one it replaced.
#[tokio::test]
async fn every_change_is_audited_with_its_actor_and_credential() {
    let w = world().await;
    let issued = forge_db::tokens::issue(
        &w.t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: w.maintainer,
            name: "gstn".to_owned(),
            scopes: vec![Scope::Api],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("token");
    let bearer = Who::Bearer(issued.token.reveal());
    let (first, second) = (w.date(10).await, w.date(20).await);
    for until in [&first, &second] {
        let a = w
            .call_on(
                w.web,
                "PUT",
                bearer.clone(),
                Some(json!({"warn_until": until})),
                &[],
            )
            .await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    }
    let cleared = w.call("DELETE", session(w.maintainer), None).await;
    assert_eq!(cleared.status, StatusCode::NO_CONTENT, "{}", cleared.json);
    let rows = w.audit_full().await;
    let me = Some(w.maintainer.get());
    let project = w.web.get();
    assert_eq!(
        rows,
        vec![
            (
                "doc_lint_migration.set".to_owned(),
                json!({"project_id": project, "warn_until": first, "previous": null}),
                me,
                "user".to_owned(),
                Some(issued.token_id),
            ),
            (
                "doc_lint_migration.set".to_owned(),
                json!({"project_id": project, "warn_until": second, "previous": first}),
                me,
                "user".to_owned(),
                Some(issued.token_id),
            ),
            (
                "doc_lint_migration.cleared".to_owned(),
                json!({"project_id": project, "warn_until": second}),
                me,
                "user".to_owned(),
                None,
            ),
        ]
    );
}
