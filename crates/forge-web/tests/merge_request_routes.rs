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
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        forge_auth::AccountKind::Human,
    )
    .as_agent(agent)
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

/// MR B0: every merge request answer names its author (`author`:
/// username and display name, read with the row) — the create, the read,
/// the list, an edit and a close — so the screens need no user lookup.
#[tokio::test]
async fn merge_request_body_carries_author_name() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let dave = users::create(
        &t.db,
        &ctx("req-user-dave"),
        &NewUser::human("dave.k", "Dave Kowalski", "dave@example.test"),
    )
    .await
    .expect("dave")
    .user_id;
    let web = web(&t, olga, &[(dave, Role::Developer)]).await;
    let p = web.project_id.get();
    let r = || router(&t.db);
    let list = format!("/api/v1/projects/{p}/merge-requests");
    let one = format!("{list}/1");
    let author = json!({"username": "dave.k", "display_name": "Dave Kowalski"});

    let opened = call(
        r(),
        "POST",
        &list,
        session(dave),
        Some(json!({"title": "T", "source_branch": "feature", "target_branch": "main"})),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json);
    assert_eq!(opened.json["author"], author);
    assert_eq!(opened.json["author_id"], dave.get(), "the id stays");
    let read = call(r(), "GET", &one, session(olga), None).await;
    assert_eq!(read.json["author"], author, "{}", read.json);
    let listed = call(r(), "GET", &list, session(olga), None).await;
    assert_eq!(listed.json["merge_requests"][0]["author"], author);
    let edited = call(
        r(),
        "PATCH",
        &one,
        session(olga),
        Some(json!({"title": "U"})),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.json);
    assert_eq!(edited.json["author"], author, "the author, not the editor");
    let closed = call(r(), "POST", &format!("{one}/close"), session(olga), None).await;
    assert_eq!(closed.json["author"], author);
    t.drop_now().await;
}

/// ADR-0024 §12: every merge request route answers the same 404 body
/// (FG-REPO-0005, the same user message) for an invisible project, a
/// missing project and — on the routes of one merge request — a missing
/// iid of a project the caller sees, and for an existing iid of an
/// invisible project.
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
    let create = json!({"title": "y", "source_branch": "fix", "target_branch": "main"});
    let patch = json!({"title": "z"});
    // (method, suffix after the project's merge-requests, body)
    let routes: [(&str, &str, Option<Value>); 6] = [
        ("GET", "", None),
        ("POST", "", Some(create)),
        ("GET", "/{iid}", None),
        ("PATCH", "/{iid}", Some(patch)),
        ("POST", "/{iid}/close", None),
        ("POST", "/{iid}/reopen", None),
    ];
    let mut answers = Vec::new();
    for (method, suffix, body) in &routes {
        let one = suffix.contains("{iid}");
        let uri = |project: i64, iid: i64| {
            format!(
                "/api/v1/projects/{project}/merge-requests{}",
                suffix.replace("{iid}", &iid.to_string())
            )
        };
        // (who, project, iid): an invisible project (existing iid and, for
        // the routes of one, a missing one), a missing project, and a
        // visible project's missing iid.
        let mut cases = vec![(stranger, p, 1), (stranger, 9999, 1)];
        if one {
            cases.push((stranger, p, 2));
            cases.push((dave, p, 2));
        }
        for (who, project, iid) in cases {
            let target = uri(project, iid);
            let a = call(r(), method, &target, session(who), body.clone()).await;
            assert_eq!(
                denied(&a),
                (StatusCode::NOT_FOUND, "FG-REPO-0005"),
                "{method} {target}"
            );
            let mut answer = a.json.clone();
            answer
                .as_object_mut()
                .expect("an error object")
                .remove("request_id");
            answers.push(answer);
        }
    }
    assert!(
        answers.windows(2).all(|w| w[0] == w[1]),
        "every 404 body is the same: {answers:?}"
    );
    t.drop_now().await;
}

/// The list's paging and query through the router: `limit` and `before`
/// page newest first, `next_before` ends with `null`; a bad `state`,
/// `limit` or `before` and an unknown parameter are FG-WEB-0006 (400).
#[tokio::test]
async fn the_list_pages_newest_first_and_refuses_a_bad_query() {
    let t = FreshDb::migrated().await;
    let olga = user(&t, "olga").await;
    let web = web(&t, olga, &[]).await;
    let p = web.project_id.get();
    let r = || router(&t.db);
    let list = format!("/api/v1/projects/{p}/merge-requests");
    for source in ["feature", "fix", "release"] {
        let a = call(
            r(),
            "POST",
            &list,
            session(olga),
            Some(json!({"title": source, "source_branch": source, "target_branch": "main"})),
        )
        .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
    }
    let iids = |a: &Answer| -> Vec<i64> {
        a.json["merge_requests"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|m| m["iid"].as_i64().expect("iid"))
            .collect()
    };
    let first = call(r(), "GET", &format!("{list}?limit=2"), session(olga), None).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.json);
    assert_eq!(iids(&first), [3, 2]);
    assert_eq!(first.json["next_before"], 2);
    let second = call(
        r(),
        "GET",
        &format!("{list}?limit=2&before=2"),
        session(olga),
        None,
    )
    .await;
    assert_eq!(iids(&second), [1]);
    assert_eq!(second.json["next_before"], Value::Null);
    let closed = call(
        r(),
        "GET",
        &format!("{list}?state=closed"),
        session(olga),
        None,
    )
    .await;
    assert_eq!(iids(&closed), Vec::<i64>::new());
    for bad in [
        "state=draft",
        "limit=0",
        "limit=101",
        "before=0",
        "before=x",
        "page=2",
        "state=open&state=all",
    ] {
        let a = call(r(), "GET", &format!("{list}?{bad}"), session(olga), None).await;
        assert_eq!(
            denied(&a),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{bad}"
        );
    }
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
