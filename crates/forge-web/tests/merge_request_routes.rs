#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge request routes of M3 R1a through the product router, against
//! the development database (ADR-0011) with the real loaders and
//! forge-review's commands; branches from a scripted probe (the browse
//! API's refs in `forge serve`). Allowed and denied rows of every route
//! through HTTP, the identical 404 of an invisible project and a missing
//! iid (ADR-0024 §12), and FG-REVIEW-0001.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{RefName, RepoId, RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
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

/// `main`, `feature`, `fix` and `release` exist.
struct Branches(BTreeSet<&'static str>);

impl BranchProbe for Branches {
    fn branch_exists<'a>(
        &'a self,
        _repo: RepoId,
        branch: &'a RefName,
        _caller: &'a str,
    ) -> LookupFuture<'a, ()> {
        let found = branch.branch().is_some_and(|b| self.0.contains(b));
        Box::pin(async move {
            if found {
                Ok(())
            } else {
                Err(ForgeError::new(ErrorCode::RepositoryPathNotResolved)
                    .with_context("reason", "ref_not_found"))
            }
        })
    }
}

fn router(db: &Db) -> axum::Router {
    let branches = Branches(["main", "feature", "fix", "release"].into_iter().collect());
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
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

async fn call(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Principal,
    body: Option<Value>,
) -> Answer {
    let builder = Request::builder().method(method).uri(uri);
    let mut req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    req.extensions_mut().insert(who);
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session)
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(scopes.to_vec())).as_agent(agent)
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

fn denied(a: &Answer) -> (StatusCode, &str) {
    (a.status, code(a))
}

/// `acme/web` (private, ready), owned by `owner`, with `members`.
async fn web(t: &FreshDb, owner: UserId, members: &[(UserId, Role)]) -> CreatedProject {
    let group = groups::create(
        &t.db,
        &ctx("req-acme"),
        &NewGroup {
            segment: "acme".into(),
            parent: None,
            display_name: "Acme".into(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("acme");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", group.namespace_id, "Web", owner),
    )
    .await
    .expect("web");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    for (who, role) in members {
        projects::add_member(&t.db, &ctx("req-member"), web.project_id, *who, *role)
            .await
            .expect("member");
    }
    web
}

#[tokio::test]
async fn merge_requests_are_opened_listed_read_edited_closed_and_reopened_through_the_routes() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let rita = user(&t, "rita").await;
    let gus = user(&t, "gus").await;
    let web = web(
        &t,
        olga,
        &[
            (dave, Role::Developer),
            (rita, Role::Reporter),
            (gus, Role::Guest),
        ],
    )
    .await;
    let p = web.project_id.get();
    let r = || router(&t.db);
    let list = format!("/api/v1/projects/{p}/merge-requests");
    let one = format!("{list}/1");
    let open_body = json!({
        "title": "Add the login form",
        "description": "synthetic",
        "source_branch": "feature",
        "target_branch": "refs/heads/main",
        "draft": true
    });

    // A Reporter may not open one; a Developer may.
    let refused = call(r(), "POST", &list, session(rita), Some(open_body.clone())).await;
    assert_eq!(denied(&refused), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let opened = call(r(), "POST", &list, session(dave), Some(open_body.clone())).await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json);
    assert_eq!(opened.json["iid"], 1);
    assert_eq!(opened.json["source_branch"], "feature");
    assert_eq!(opened.json["target_branch"], "main");
    assert_eq!(opened.json["state"], "open");
    assert_eq!(opened.json["draft"], true);
    assert_eq!(opened.json["author_id"], dave.get());
    assert_eq!(opened.json["head_sha"], Value::Null);
    // The same pair again is FG-REVIEW-0001.
    let again = call(r(), "POST", &list, session(dave), Some(open_body)).await;
    assert_eq!(denied(&again), (StatusCode::CONFLICT, "FG-REVIEW-0001"));

    // A Reporter reads and lists; a Guest of a private project does not.
    let read = call(r(), "GET", &one, session(rita), None).await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.json);
    assert_eq!(read.json["title"], "Add the login form");
    let listed = call(r(), "GET", &list, session(rita), None).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json["merge_requests"][0]["iid"], 1);
    assert_eq!(listed.json["next_before"], Value::Null);
    let guest = call(r(), "GET", &one, session(gus), None).await;
    assert_eq!(denied(&guest), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let guest_list = call(r(), "GET", &list, session(gus), None).await;
    assert_eq!(denied(&guest_list), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));

    // A Reporter may not edit another's merge request; the author may.
    let patch = json!({"title": "Add the sign-in form", "draft": false});
    let refused = call(r(), "PATCH", &one, session(rita), Some(patch.clone())).await;
    assert_eq!(denied(&refused), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let edited = call(r(), "PATCH", &one, session(dave), Some(patch)).await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.json);
    assert_eq!(
        (&edited.json["title"], &edited.json["draft"]),
        (&json!("Add the sign-in form"), &json!(false))
    );

    // Close and reopen: the author, and an Owner; a Reporter not.
    let close = format!("{one}/close");
    let reopen = format!("{one}/reopen");
    let refused = call(r(), "POST", &close, session(rita), None).await;
    assert_eq!(denied(&refused), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let closed = call(r(), "POST", &close, session(dave), None).await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.json);
    assert_eq!(closed.json["state"], "closed");
    assert_eq!(closed.json["closed_by"], dave.get());
    let open_list = call(r(), "GET", &list, session(rita), None).await;
    assert_eq!(
        open_list.json["merge_requests"],
        json!([]),
        "open by default"
    );
    let all = call(
        r(),
        "GET",
        &format!("{list}?state=all"),
        session(rita),
        None,
    )
    .await;
    assert_eq!(all.json["merge_requests"][0]["state"], "closed");
    let reopened = call(r(), "POST", &reopen, session(olga), None).await;
    assert_eq!(reopened.status, StatusCode::OK, "{}", reopened.json);
    assert_eq!(reopened.json["state"], "open");
    assert_eq!(reopened.json["closed_at"], Value::Null);
    t.drop_now().await;
}

/// ADR-0024 §12: an invisible project's existing iid, its missing iid, a
/// missing project and a visible project's missing iid all answer the same
/// 404 body (FG-REPO-0005, the same user message).
#[tokio::test]
async fn invisible_merge_request_is_indistinguishable_from_missing() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let stranger = user(&t, "sam").await;
    let web = web(&t, olga, &[(dave, Role::Developer)]).await;
    let p = web.project_id.get();
    let r = || router(&t.db);
    let opened = call(
        r(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        session(dave),
        Some(json!({"title": "x", "source_branch": "feature", "target_branch": "main"})),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json);
    let mut answers = Vec::new();
    for (who, uri) in [
        (stranger, format!("/api/v1/projects/{p}/merge-requests/1")),
        (stranger, format!("/api/v1/projects/{p}/merge-requests/2")),
        (
            stranger,
            "/api/v1/projects/9999/merge-requests/1".to_owned(),
        ),
        (dave, format!("/api/v1/projects/{p}/merge-requests/2")),
        (
            stranger,
            format!("/api/v1/projects/{p}/merge-requests/1/close"),
        ),
    ] {
        let method = if uri.ends_with("/close") {
            "POST"
        } else {
            "GET"
        };
        let a = call(r(), method, &uri, session(who), None).await;
        assert_eq!(denied(&a), (StatusCode::NOT_FOUND, "FG-REPO-0005"), "{uri}");
        let mut body = a.json.clone();
        body.as_object_mut()
            .expect("an error object")
            .remove("request_id");
        answers.push(body);
    }
    assert!(
        answers.windows(2).all(|w| w[0] == w[1]),
        "every 404 body is the same: {answers:?}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn branch_input_scopes_and_draft_rights_are_enforced_through_the_routes() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = user(&t, "dave").await;
    let erin = user(&t, "erin").await;
    let mia = user(&t, "mia").await;
    let web = web(
        &t,
        olga,
        &[
            (dave, Role::Developer),
            (erin, Role::Developer),
            (mia, Role::Maintainer),
        ],
    )
    .await;
    let p = web.project_id.get();
    let r = || router(&t.db);
    let list = format!("/api/v1/projects/{p}/merge-requests");
    let body = |source: &str, target: &str| json!({"title": "x", "source_branch": source, "target_branch": target, "draft": true});
    // Branches only, never the same; checked before any lookup.
    for (source, target) in [
        ("feature", "refs/tags/v1"),
        ("refs/merge-requests/1/head", "main"),
        ("main", "refs/heads/main"),
        ("a..b", "main"),
    ] {
        let a = call(
            r(),
            "POST",
            &list,
            session(dave),
            Some(body(source, target)),
        )
        .await;
        assert_eq!(
            denied(&a),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{source} -> {target}"
        );
    }
    // A branch that does not exist.
    let a = call(
        r(),
        "POST",
        &list,
        session(dave),
        Some(body("nope", "main")),
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::NOT_FOUND, "FG-REPO-0023"));
    // A read_api token reads but does not open; an agent's api token opens.
    let a = call(
        r(),
        "POST",
        &list,
        token(dave, &[Scope::ReadApi], false),
        Some(body("feature", "main")),
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::FORBIDDEN, "FG-AUTH-0002"));
    let a = call(
        r(),
        "POST",
        &list,
        token(dave, &[Scope::Api], true),
        Some(body("feature", "main")),
    )
    .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
    let a = call(
        r(),
        "GET",
        &list,
        token(erin, &[Scope::ReadApi], false),
        None,
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    // Another Developer may retitle and retarget, not touch the draft flag;
    // a Maintainer may.
    let one = format!("{list}/1");
    let a = call(
        r(),
        "PATCH",
        &one,
        session(erin),
        Some(json!({"title": "Retitled", "target_branch": "release"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["target_branch"], "release");
    let a = call(
        r(),
        "PATCH",
        &one,
        session(erin),
        Some(json!({"draft": false})),
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let a = call(
        r(),
        "PATCH",
        &one,
        session(mia),
        Some(json!({"draft": false})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["draft"], false);
    // Retargeting onto the source is refused before the lookup; onto a
    // missing branch after it.
    let a = call(
        r(),
        "PATCH",
        &one,
        session(dave),
        Some(json!({"target_branch": "feature"})),
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::FORBIDDEN, "FG-AUTH-0003"));
    let a = call(
        r(),
        "PATCH",
        &one,
        session(dave),
        Some(json!({"target_branch": "gone"})),
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::NOT_FOUND, "FG-REPO-0023"));
    // Unknown fields and queries are refused.
    let a = call(
        r(),
        "PATCH",
        &one,
        session(dave),
        Some(json!({"state": "merged"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.json);
    let a = call(
        r(),
        "GET",
        &format!("{list}?state=draft"),
        session(dave),
        None,
    )
    .await;
    assert_eq!(denied(&a), (StatusCode::BAD_REQUEST, "FG-WEB-0006"));
    t.drop_now().await;
}
