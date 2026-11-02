#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Namespace moves (step D5 MR5) through the product router, against the
//! development database (ADR-0011) with the real loaders and
//! forge-hosting's commands: `POST /api/v1/projects/{id}/transfer`,
//! `POST /api/v1/groups/{id}/transfer`, `POST /api/v1/users/{id}/rename`
//! and `POST /api/v1/namespaces/tombstones/release`. Each route's rows,
//! allowed and denied (Owner, Maintainer, administrator, a read-only
//! token, an administrator's token without `admin`, an agent, a stranger,
//! a project pending deletion, malformed ids), its answers (path taken,
//! rate limit, no active Owner, a target that may not be used) and the
//! old path afterwards: `302` for a reader's clone, `404` for anyone else
//! and for a push.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{GroupId, NamespaceId, ProjectId, RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
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

fn router(db: &Db) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_hosting(Hosting::new(db.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn user(t: &FreshDb, name: &str, admin: bool) -> users::Created {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
}

async fn group(
    t: &FreshDb,
    segment: &str,
    parent: Option<NamespaceId>,
    owner: UserId,
) -> CreatedGroup {
    groups::create(
        &t.db,
        &ctx(&format!("req-group-{segment}")),
        &NewGroup {
            segment: segment.to_owned(),
            parent,
            display_name: segment.to_owned(),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group")
}

async fn member(t: &FreshDb, g: GroupId, who: UserId, role: Role) {
    groups::add_member(&t.db, &ctx("req-member"), g, who, role)
        .await
        .expect("member");
}

struct Answer {
    status: StatusCode,
    location: Option<String>,
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
    let location = res
        .headers()
        .get("location")
        .map(|v| v.to_str().expect("ascii").to_owned());
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        location,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session)
}

fn token(user: UserId, admin: bool, scope: Scope, agent: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Token(vec![scope])).as_agent(agent)
}

/// An administrator's token with `api` and `admin`.
fn admin_token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        true,
        true,
        Credential::Token(vec![Scope::Api, Scope::Admin]),
    )
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

/// The error body is the user message and the request id only.
fn only_user_message(a: &Answer) {
    let keys: Vec<&str> = a
        .json
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default();
    assert_eq!(
        keys,
        ["error_code", "request_id", "user_message"],
        "{}",
        a.json
    );
}

async fn create_project(t: &FreshDb, owner: UserId, namespace: NamespaceId, path: &str) -> i64 {
    let created = call(
        router(&t.db),
        "POST",
        "/api/v1/projects",
        session(owner, false),
        Some(json!({"namespace_id": namespace.get(), "path": path, "display_name": path})),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    created.json["id"].as_i64().expect("id")
}

async fn path_of(t: &FreshDb, project: i64) -> String {
    let rows = t
        .json_rows(&format!(
            "SELECT n.full_path AS p FROM forge.namespaces n \
             WHERE n.kind = 'project' AND n.owner_id = {project}"
        ))
        .await;
    rows[0]["p"].as_str().expect("path").to_owned()
}

async fn moves(t: &FreshDb) -> i64 {
    let rows = t
        .json_rows("SELECT count(*) AS n FROM forge.audit_log WHERE action IN ('project.moved', 'group.moved', 'user.renamed')")
        .await;
    rows[0]["n"].as_i64().expect("n")
}

/// alice owns acme (and its project web) and beta; maint maintains acme;
/// dev is only a Developer of beta; bob is a stranger; root an administrator.
struct World {
    t: FreshDb,
    alice: users::Created,
    maint: users::Created,
    dev: users::Created,
    bob: users::Created,
    root: users::Created,
    acme: CreatedGroup,
    beta: CreatedGroup,
    web: i64,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let maint = user(&t, "maint", false).await;
    let dev = user(&t, "dev", false).await;
    let bob = user(&t, "bob", false).await;
    let root = user(&t, "root", true).await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    member(&t, acme.group_id, maint.user_id, Role::Maintainer).await;
    member(&t, beta.group_id, dev.user_id, Role::Developer).await;
    let web = create_project(&t, alice.user_id, acme.namespace_id, "web").await;
    World {
        t,
        alice,
        maint,
        dev,
        bob,
        root,
        acme,
        beta,
        web,
    }
}

#[tokio::test]
async fn a_project_moves_and_its_old_path_redirects_a_reader_only() {
    let w = world().await;
    let r = || router(&w.t.db);
    let uri = format!("/api/v1/projects/{}/transfer", w.web);
    let moved = call(
        r(),
        "POST",
        &uri,
        session(w.alice.user_id, false),
        Some(json!({"namespace_id": w.beta.namespace_id.get(), "path": "site"})),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json);
    assert_eq!(moved.json["id"], json!(w.web));
    assert_eq!(moved.json["kind"], "project");
    assert_eq!(moved.json["old_path"], "acme/web");
    assert_eq!(moved.json["path"], "beta/site");
    assert_eq!(moved.json["tombstones"], 1);
    assert_eq!(moved.json["projects"], 1);
    assert_eq!(path_of(&w.t, w.web).await, "beta/site");
    // One project.moved event, integration class, for the project.
    let events =
        w.t.json_rows(
            "SELECT project_id, payload FROM forge.events WHERE event_type = 'project.moved'",
        )
        .await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["project_id"], json!(w.web));

    // The old path: a clone by the Owner is redirected (302, no-store) to
    // the new path; a stranger gets the absent 404 (no redirect, so the new
    // path stays hidden); a push through it is refused.
    let advert = "/acme/web.git/info/refs?service=git-upload-pack";
    let owner = call(r(), "GET", advert, session(w.alice.user_id, false), None).await;
    assert_eq!(owner.status, StatusCode::FOUND, "{}", owner.json);
    assert_eq!(
        owner.location.as_deref(),
        Some("/beta/site.git/info/refs?service=git-upload-pack")
    );
    let stranger = call(r(), "GET", advert, session(w.bob.user_id, false), None).await;
    assert_eq!(
        (stranger.status, code(&stranger)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert!(stranger.location.is_none());
    let push = call(
        r(),
        "GET",
        "/acme/web.git/info/refs?service=git-receive-pack",
        session(w.alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(
        (push.status, code(&push)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );

    // The old path is blocked: a new project there is 409 (tombstoned)
    // until the Owner releases the tombstone; then it can be used again.
    let taken = call(
        r(),
        "POST",
        "/api/v1/projects",
        session(w.alice.user_id, false),
        Some(
            json!({"namespace_id": w.acme.namespace_id.get(), "path": "web", "display_name": "w"}),
        ),
    )
    .await;
    assert_eq!(
        (taken.status, code(&taken)),
        (StatusCode::CONFLICT, "FG-REPO-0009")
    );
    let release = "/api/v1/namespaces/tombstones/release";
    let body = Some(json!({"path": "acme/web"}));
    for (who, want) in [
        (
            session(w.bob.user_id, false),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
        (
            session(w.dev.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.alice.user_id, false, Scope::Api, true),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.alice.user_id, false, Scope::ReadApi, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
    ] {
        let a = call(r(), "POST", release, who, body.clone()).await;
        assert_eq!((a.status, code(&a)), want, "{}", a.json);
        only_user_message(&a);
    }
    let released = call(
        r(),
        "POST",
        release,
        session(w.alice.user_id, false),
        body.clone(),
    )
    .await;
    assert_eq!(released.status, StatusCode::OK, "{}", released.json);
    assert_eq!(released.json, json!({"path": "acme/web", "kind": "moved"}));
    let again = call(r(), "POST", release, session(w.alice.user_id, false), body).await;
    assert_eq!(
        (again.status, code(&again)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    create_project(&w.t, w.alice.user_id, w.acme.namespace_id, "web").await;
    // A malformed path is 400.
    let bad = call(
        r(),
        "POST",
        release,
        session(w.root.user_id, true),
        Some(json!({"path": "a//b"})),
    )
    .await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_project_transfer_follows_the_matrix() {
    let w = world().await;
    let r = || router(&w.t.db);
    let uri = format!("/api/v1/projects/{}/transfer", w.web);
    let to_beta = || Some(json!({"namespace_id": w.beta.namespace_id.get()}));
    // Denied, nothing moved: a Maintainer (403), the Owner's read-only
    // token (403 scope), an agent with the Owner's api token (403, D5), an
    // administrator's token without `admin` (the private project is not
    // visible to it: 404), a stranger (404).
    for (who, want) in [
        (
            session(w.maint.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.alice.user_id, false, Scope::ReadApi, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
        (
            token(w.alice.user_id, false, Scope::Api, true),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.root.user_id, true, Scope::Api, false),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
        (
            session(w.bob.user_id, false),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
    ] {
        let a = call(r(), "POST", &uri, who, to_beta()).await;
        assert_eq!((a.status, code(&a)), want, "{}", a.json);
        only_user_message(&a);
    }
    assert_eq!(moves(&w.t).await, 0);
    // Targets: an absent namespace and a project's namespace are the same
    // 404 as one not visible; the top of a stranger's personal namespace is
    // 403; a group where the Owner is only... (dev's role) is decided on.
    let as_alice = || session(w.alice.user_id, false);
    for ns in [9_999_i64] {
        let a = call(
            r(),
            "POST",
            &uri,
            as_alice(),
            Some(json!({"namespace_id": ns})),
        )
        .await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{ns}"
        );
    }
    let project_ns =
        w.t.json_rows(&format!(
            "SELECT id FROM forge.namespaces WHERE kind = 'project' AND owner_id = {}",
            w.web
        ))
        .await[0]["id"]
            .as_i64()
            .expect("id");
    let a = call(
        r(),
        "POST",
        &uri,
        as_alice(),
        Some(json!({"namespace_id": project_ns})),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let a = call(
        r(),
        "POST",
        &uri,
        as_alice(),
        Some(json!({"namespace_id": w.bob.namespace_id.get()})),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    // Malformed ids and bodies are 400.
    for bad in [
        "/api/v1/projects/01/transfer",
        "/api/v1/projects/0/transfer",
    ] {
        let a = call(r(), "POST", bad, session(w.root.user_id, true), to_beta()).await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
            "{bad}"
        );
    }
    for body in [
        json!({"namespace_id": 0}),
        json!({"namespace_id": w.beta.namespace_id.get(), "x": 1}),
    ] {
        let a = call(r(), "POST", &uri, as_alice(), Some(body.clone())).await;
        assert_eq!(a.status, StatusCode::BAD_REQUEST, "{body}");
    }
    // An absent project is 404 for an administrator too.
    let a = call(
        r(),
        "POST",
        "/api/v1/projects/9999/transfer",
        session(w.root.user_id, true),
        to_beta(),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(moves(&w.t).await, 0);
    // Allowed: an administrator (session) moves any project anywhere it may
    // live; the Owner with an api token moves it back.
    let a = call(r(), "POST", &uri, session(w.root.user_id, true), to_beta()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["path"], "beta/web");
    let a = call(
        r(),
        "POST",
        &uri,
        token(w.alice.user_id, false, Scope::Api, false),
        Some(json!({"namespace_id": w.acme.namespace_id.get(), "path": "web2"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["path"], "acme/web2");
    // A path taken by a sibling is 409; its own old path is tombstoned
    // (409) unless reclaimed.
    create_project(&w.t, w.alice.user_id, w.acme.namespace_id, "other").await;
    let a = call(
        r(),
        "POST",
        &uri,
        as_alice(),
        Some(json!({"path": "other"})),
    )
    .await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "FG-REPO-0009"));
    let a = call(r(), "POST", &uri, as_alice(), Some(json!({"path": "web"}))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "FG-REPO-0009"));
    let a = call(
        r(),
        "POST",
        &uri,
        as_alice(),
        Some(json!({"path": "web", "reclaim": true})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(
        (
            a.json["path"].clone(),
            a.json["reclaimed_tombstones"].clone()
        ),
        (json!("acme/web"), json!(1))
    );
    // Nothing to change is 409 (same_location).
    let a = call(r(), "POST", &uri, as_alice(), Some(json!({}))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "FG-REPO-0015"));
    w.t.drop_now().await;
}

/// A rename keeps the parent and is decided on the caller's role on the
/// project: an Owner who is only a Developer of the group renames it, but
/// cannot move it to another group where they are a Developer.
#[tokio::test]
async fn a_rename_is_decided_on_the_projects_owner_not_the_parent() {
    let w = world().await;
    let r = || router(&w.t.db);
    // dev creates nothing in beta (Developer); alice makes dev a direct
    // Owner of a project in beta.
    let site = create_project(&w.t, w.alice.user_id, w.beta.namespace_id, "site").await;
    forge_db::projects::add_member(
        &w.t.db,
        &ctx("req-dev"),
        ProjectId::new(site),
        w.dev.user_id,
        Role::Owner,
    )
    .await
    .expect("dev owns site");
    let uri = format!("/api/v1/projects/{site}/transfer");
    let as_dev = || session(w.dev.user_id, false);
    let a = call(
        r(),
        "POST",
        &uri,
        as_dev(),
        Some(json!({"path": "renamed"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["path"], "beta/renamed");
    // Naming the current parent explicitly is the same rename.
    let a = call(
        r(),
        "POST",
        &uri,
        as_dev(),
        Some(json!({"namespace_id": w.beta.namespace_id.get(), "path": "again"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    // Another group: dev has no role in acme → 403.
    let a = call(
        r(),
        "POST",
        &uri,
        as_dev(),
        Some(json!({"namespace_id": w.acme.namespace_id.get()})),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_pending_project_is_410_to_its_owner_and_404_to_others() {
    let w = world().await;
    let r = || router(&w.t.db);
    let deleted = call(
        r(),
        "DELETE",
        &format!("/api/v1/projects/{}", w.web),
        session(w.alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let uri = format!("/api/v1/projects/{}/transfer", w.web);
    let body = Some(json!({"namespace_id": w.beta.namespace_id.get()}));
    let a = call(
        r(),
        "POST",
        &uri,
        session(w.alice.user_id, false),
        body.clone(),
    )
    .await;
    assert_eq!((a.status, code(&a)), (StatusCode::GONE, "FG-REPO-0006"));
    let b = call(r(), "POST", &uri, session(w.bob.user_id, false), body).await;
    assert_eq!(
        (b.status, code(&b)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(moves(&w.t).await, 0);
    w.t.drop_now().await;
}

/// The 11th move of one project within 24 hours is 429 (administrators
/// included); a project left without an active Owner is 409.
#[tokio::test]
async fn the_api_enforces_the_rate_limit_and_the_owner_invariant() {
    let w = world().await;
    let r = || router(&w.t.db);
    let uri = format!("/api/v1/projects/{}/transfer", w.web);
    for n in 0..10 {
        let a = call(
            r(),
            "POST",
            &uri,
            session(w.alice.user_id, false),
            Some(json!({"path": format!("w{n}")})),
        )
        .await;
        assert_eq!(a.status, StatusCode::OK, "move {n}: {}", a.json);
    }
    for who in [
        session(w.alice.user_id, false),
        session(w.root.user_id, true),
    ] {
        let a = call(r(), "POST", &uri, who, Some(json!({"path": "w-late"}))).await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::TOO_MANY_REQUESTS, "FG-REPO-0018")
        );
        only_user_message(&a);
    }
    assert_eq!(moves(&w.t).await, 10);

    // No active Owner left: alice owns `api` only through acme (her
    // direct row removed); delta has no direct Owner (only possible through
    // the database) and alice maintains it, so she may move api there, but
    // api would have no active Owner afterwards: 409, nothing written.
    let api = create_project(&w.t, w.alice.user_id, w.acme.namespace_id, "api").await;
    w.t.commit_audited(
        ("project", api),
        &[format!(
            "DELETE FROM forge.project_members WHERE project_id = {api}"
        )],
    )
    .await;
    let delta = group(&w.t, "delta", None, w.root.user_id).await;
    member(&w.t, delta.group_id, w.alice.user_id, Role::Maintainer).await;
    w.t.commit_audited(
        ("group", delta.group_id.get()),
        &[format!(
            "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
            delta.group_id.get(),
            w.root.user_id.get()
        )],
    )
    .await;
    let before = moves(&w.t).await;
    let a = call(
        r(),
        "POST",
        &format!("/api/v1/projects/{api}/transfer"),
        session(w.alice.user_id, false),
        Some(json!({"namespace_id": delta.namespace_id.get()})),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::CONFLICT, "FG-REPO-0015"),
        "{}",
        a.json
    );
    only_user_message(&a);
    assert_eq!(moves(&w.t).await, before, "nothing written");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_group_transfer_follows_the_matrix() {
    let w = world().await;
    let r = || router(&w.t.db);
    let platform = group(&w.t, "platform", Some(w.acme.namespace_id), w.alice.user_id).await;
    let uri = format!("/api/v1/groups/{}/transfer", platform.group_id.get());
    let to_beta = || Some(json!({"namespace_id": w.beta.namespace_id.get()}));
    // Denied: a Maintainer (inherited from acme), a stranger (groups are
    // visible: 403), an agent, a read-only token; dev is only a Developer of
    // the target, alice to the top level, a body naming both.
    for (who, body, want) in [
        (
            session(w.maint.user_id, false),
            to_beta(),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            session(w.bob.user_id, false),
            to_beta(),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.alice.user_id, false, Scope::Api, true),
            to_beta(),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.alice.user_id, false, Scope::ReadApi, false),
            to_beta(),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
        (
            token(w.root.user_id, true, Scope::Api, false),
            Some(json!({"top_level": true})),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            session(w.alice.user_id, false),
            Some(json!({"top_level": true})),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            session(w.alice.user_id, false),
            Some(json!({"namespace_id": w.alice.namespace_id.get()})),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            session(w.alice.user_id, false),
            Some(json!({"namespace_id": 9999})),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
        (
            session(w.alice.user_id, false),
            Some(json!({"namespace_id": w.beta.namespace_id.get(), "top_level": true})),
            (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
        ),
    ] {
        let a = call(r(), "POST", &uri, who, body).await;
        assert_eq!((a.status, code(&a)), want, "{}", a.json);
    }
    let bad = call(
        r(),
        "POST",
        "/api/v1/groups/x/transfer",
        session(w.root.user_id, true),
        to_beta(),
    )
    .await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let absent = call(
        r(),
        "POST",
        "/api/v1/groups/9999/transfer",
        session(w.root.user_id, true),
        to_beta(),
    )
    .await;
    assert_eq!(
        (absent.status, code(&absent)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(moves(&w.t).await, 0);
    // Allowed: alice (Owner on both sides); the subtree moves with it.
    let api = create_project(&w.t, w.alice.user_id, platform.namespace_id, "api").await;
    let a = call(
        r(),
        "POST",
        &uri,
        session(w.alice.user_id, false),
        to_beta(),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(
        (a.json["kind"].clone(), a.json["path"].clone()),
        (json!("group"), json!("beta/platform"))
    );
    assert_eq!(
        (a.json["tombstones"].clone(), a.json["projects"].clone()),
        (json!(2), json!(1))
    );
    assert_eq!(path_of(&w.t, api).await, "beta/platform/api");
    let events =
        w.t.json_rows("SELECT project_id FROM forge.events WHERE event_type = 'group.moved'")
            .await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["project_id"], Value::Null, "instance-wide");
    // A subgroup Owner without any role on the parent renames it.
    member(&w.t, platform.group_id, w.dev.user_id, Role::Owner).await;
    w.t.commit_audited(
        ("group", w.beta.group_id.get()),
        &[format!(
            "DELETE FROM forge.group_members WHERE group_id = {} AND user_id = {}",
            w.beta.group_id.get(),
            w.dev.user_id.get()
        )],
    )
    .await;
    let a = call(
        r(),
        "POST",
        &uri,
        session(w.dev.user_id, false),
        Some(json!({"path": "plat"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["path"], "beta/plat");
    // To the top level: administrators only (session, or an admin token).
    let a = call(
        r(),
        "POST",
        &uri,
        admin_token(w.root.user_id),
        Some(json!({"top_level": true})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["path"], "plat");
    // A top-level group's rename is a move to the top level: its Owner
    // cannot, an administrator can.
    let a = call(
        r(),
        "POST",
        &uri,
        session(w.dev.user_id, false),
        Some(json!({"path": "p2"})),
    )
    .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let a = call(
        r(),
        "POST",
        &uri,
        session(w.root.user_id, true),
        Some(json!({"path": "p2"})),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_user_rename_is_for_administrators_only() {
    let w = world().await;
    let r = || router(&w.t.db);
    let uri = format!("/api/v1/users/{}/rename", w.bob.user_id.get());
    let body = || Some(json!({"username": "robert"}));
    for (who, want) in [
        (
            session(w.bob.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            session(w.alice.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.root.user_id, true, Scope::Api, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            admin_token(w.root.user_id).as_agent(true),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        (
            token(w.root.user_id, true, Scope::ReadApi, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
    ] {
        let a = call(r(), "POST", &uri, who, body()).await;
        assert_eq!((a.status, code(&a)), want, "{}", a.json);
    }
    let bad = call(
        r(),
        "POST",
        "/api/v1/users/-1/rename",
        session(w.root.user_id, true),
        body(),
    )
    .await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let absent = call(
        r(),
        "POST",
        "/api/v1/users/9999/rename",
        session(w.root.user_id, true),
        body(),
    )
    .await;
    assert_eq!(
        (absent.status, code(&absent)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let taken = call(
        r(),
        "POST",
        &uri,
        session(w.root.user_id, true),
        Some(json!({"username": "acme"})),
    )
    .await;
    assert_eq!(
        (taken.status, code(&taken)),
        (StatusCode::CONFLICT, "FG-REPO-0009")
    );
    assert_eq!(moves(&w.t).await, 0);
    let a = call(r(), "POST", &uri, admin_token(w.root.user_id), body()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(
        (
            a.json["kind"].clone(),
            a.json["old_path"].clone(),
            a.json["path"].clone()
        ),
        (json!("user"), json!("bob"), json!("robert"))
    );
    let rows =
        w.t.json_rows(&format!(
            "SELECT username FROM forge.users WHERE id = {}",
            w.bob.user_id.get()
        ))
        .await;
    assert_eq!(rows[0]["username"], "robert");
    let events =
        w.t.json_rows("SELECT count(*) AS n FROM forge.events WHERE event_type = 'user.renamed'")
            .await;
    assert_eq!(events[0]["n"], 1);
    w.t.drop_now().await;
}

/// Release rows the first route test does not cover (D5 MR5 review): a
/// `moved` tombstone is the absent 404 to an administrator's token without
/// `admin` and to the Owner of the old parent who has no role on the
/// target; a `deleted` one is 403 to a parent member who is not its
/// recorded owner and 404 to a caller without a role in its parent.
#[tokio::test]
async fn tombstone_release_denies_by_the_target_and_the_parent() {
    let w = world().await;
    let r = || router(&w.t.db);
    let carol = user(&w.t, "carol", false).await;
    member(&w.t, w.acme.group_id, carol.user_id, Role::Owner).await;
    // web leaves acme for alice's personal namespace: carol (Owner of acme,
    // so of web until now) has no role on it any more.
    let moved = call(
        r(),
        "POST",
        &format!("/api/v1/projects/{}/transfer", w.web),
        session(w.alice.user_id, false),
        Some(json!({"namespace_id": w.alice.namespace_id.get()})),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{}", moved.json);
    let release = "/api/v1/namespaces/tombstones/release";
    let old = || Some(json!({"path": "acme/web"}));
    for who in [
        token(w.root.user_id, true, Scope::Api, false),
        session(carol.user_id, false),
    ] {
        let a = call(r(), "POST", release, who, old()).await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{}",
            a.json
        );
    }
    // An administrator with the `admin` scope sees every target.
    let a = call(r(), "POST", release, admin_token(w.root.user_id), old()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);

    // A deleted tombstone below acme recorded for alice (synthetic row,
    // as the purge writes it).
    w.t.commit_audited(
        ("project", w.web),
        &[format!(
            "INSERT INTO forge.namespace_tombstones (old_path, kind, parent_namespace_id, \
             owner_at_write, cause_type, cause_id) \
             VALUES ('acme/gone', 'deleted', {}, {}, 'project', {})",
            w.acme.namespace_id.get(),
            w.alice.user_id.get(),
            w.web
        )],
    )
    .await;
    let gone = || Some(json!({"path": "acme/gone"}));
    for (who, want) in [
        // carol owns acme but is not the recorded owner
        (
            session(carol.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        // maint has a role in acme, not Owner, not recorded
        (
            session(w.maint.user_id, false),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        ),
        // bob has no role in acme: the same 404 as no tombstone
        (
            session(w.bob.user_id, false),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
        (
            token(w.root.user_id, true, Scope::Api, false),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        ),
    ] {
        let a = call(r(), "POST", release, who, gone()).await;
        assert_eq!((a.status, code(&a)), want, "{}", a.json);
        only_user_message(&a);
    }
    // The recorded owner, still Owner of acme, releases it.
    let a = call(
        r(),
        "POST",
        release,
        session(w.alice.user_id, false),
        gone(),
    )
    .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json, json!({"path": "acme/gone", "kind": "deleted"}));
    w.t.drop_now().await;
}

/// A group cannot move below itself or one of its descendants: 409
/// FG-REPO-0015 (`cycle`), nothing written.
#[tokio::test]
async fn a_group_moved_into_its_own_descendant_is_transfer_invalid() {
    let w = world().await;
    let r = || router(&w.t.db);
    let platform = group(&w.t, "platform", Some(w.acme.namespace_id), w.alice.user_id).await;
    let infra = group(&w.t, "infra", Some(platform.namespace_id), w.alice.user_id).await;
    for target in [infra.namespace_id, platform.namespace_id] {
        let a = call(
            r(),
            "POST",
            &format!("/api/v1/groups/{}/transfer", platform.group_id.get()),
            session(w.alice.user_id, false),
            Some(json!({"namespace_id": target.get()})),
        )
        .await;
        // into infra: a cycle; into itself: a cycle too (its own namespace)
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::CONFLICT, "FG-REPO-0015"),
            "{}",
            a.json
        );
        only_user_message(&a);
    }
    assert_eq!(moves(&w.t).await, 0);
    w.t.drop_now().await;
}
