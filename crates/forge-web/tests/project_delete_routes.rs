#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Project delete and restore (D5 MR3) through the product router, against
//! the development database (ADR-0011) with the real loaders and
//! forge-hosting's commands: the registry's proof, then the command's own
//! decision inside its transaction. Pending deletion is 410 to a caller who
//! can see the project and the absent 404 to everyone else, on the API and
//! on Smart HTTP alike.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
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

fn state(db: &Db) -> AppState {
    AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
}

fn router(db: &Db) -> axum::Router {
    let state = state(db).with_hosting(
        Hosting::new(db.clone()).with_deletion_grace(std::time::Duration::from_secs(2 * 86_400)),
    );
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

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session)
}

fn code(a: &Answer) -> &str {
    a.json["error_code"].as_str().unwrap_or("")
}

async fn create_project(t: &FreshDb, owner: &users::Created, path: &str) -> i64 {
    let created = call(
        router(&t.db),
        "POST",
        "/api/v1/projects",
        session(owner.user_id, false),
        Some(
            json!({"namespace_id": owner.namespace_id.get(), "path": path,
                    "display_name": path}),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    created.json["id"].as_i64().expect("id")
}

async fn state_of(t: &FreshDb, id: i64) -> (bool, bool) {
    let rows = t
        .json_rows(&format!(
            "SELECT deleted_at IS NOT NULL AS d, purge_after IS NOT NULL AS p \
             FROM forge.projects WHERE id = {id}"
        ))
        .await;
    (
        rows[0]["d"].as_bool().expect("d"),
        rows[0]["p"].as_bool().expect("p"),
    )
}

#[tokio::test]
async fn a_project_is_deleted_hidden_and_restored_through_the_routes() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let id = create_project(&t, &alice, "web").await;
    let one = format!("/api/v1/projects/{id}");
    let restore = format!("{one}/restore");
    let as_alice = session(alice.user_id, false);
    let as_bob = session(bob.user_id, false);
    let r = || router(&t.db);

    // Restoring a live project: the Owner is allowed to try, the answer is
    // FG-REPO-0019 (409) not_deleted; a stranger sees no project.
    let live = call(r(), "POST", &restore, as_alice.clone(), None).await;
    assert_eq!(
        (live.status, code(&live)),
        (StatusCode::CONFLICT, "FG-REPO-0019")
    );
    let hidden = call(r(), "POST", &restore, as_bob.clone(), None).await;
    assert_eq!(
        (hidden.status, code(&hidden)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );

    // A stranger cannot delete (404); a delete is not a GET (no body needed).
    let denied = call(r(), "DELETE", &one, as_bob.clone(), None).await;
    assert_eq!(
        (denied.status, code(&denied)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(state_of(&t, id).await, (false, false));

    // Delete: 200 with the grace period (2 days in this router).
    let deleted = call(r(), "DELETE", &one, as_alice.clone(), None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.json);
    assert_eq!(deleted.json["id"], json!(id));
    assert!(deleted.json["deleted_at"]
        .as_str()
        .is_some_and(|s| s.ends_with('Z')));
    assert!(deleted.json["purge_after"]
        .as_str()
        .is_some_and(|s| s.ends_with('Z')));
    assert_eq!(state_of(&t, id).await, (true, true));

    // Pending deletion: 410 for the Owner on every other action, the absent
    // 404 for a stranger; a second delete is 410 too.
    for (method, uri, body) in [
        ("GET", one.clone(), None),
        ("DELETE", one.clone(), None),
        ("PATCH", one.clone(), Some(json!({"description": "x"}))),
    ] {
        let a = call(r(), method, &uri, as_alice.clone(), body.clone()).await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::GONE, "FG-REPO-0006"),
            "{method} {uri}"
        );
        let b = call(r(), method, &uri, as_bob.clone(), body).await;
        assert_eq!(
            (b.status, code(&b)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{method} {uri}"
        );
    }

    // Restore: only an Owner; the answer is the project again.
    let stranger = call(r(), "POST", &restore, as_bob.clone(), None).await;
    assert_eq!(
        (stranger.status, code(&stranger)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(state_of(&t, id).await, (true, true));
    let restored = call(r(), "POST", &restore, as_alice.clone(), None).await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.json);
    assert_eq!(restored.json["id"], json!(id));
    assert_eq!(restored.json["role"], "owner");
    assert_eq!(state_of(&t, id).await, (false, false));
    let read = call(r(), "GET", &one, as_alice.clone(), None).await;
    assert_eq!(read.status, StatusCode::OK);
    // Restored twice: 409 not_deleted again.
    let twice = call(r(), "POST", &restore, as_alice, None).await;
    assert_eq!(
        (twice.status, code(&twice)),
        (StatusCode::CONFLICT, "FG-REPO-0019")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn delete_and_restore_follow_the_matrix_for_roles_tokens_and_agents() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let maint = user(&t, "maint", false).await;
    let root = user(&t, "root", true).await;
    let id = create_project(&t, &alice, "web").await;
    forge_db::projects::add_member(
        &t.db,
        &ctx("req-maint"),
        forge_core::ProjectId::new(id),
        maint.user_id,
        Role::Maintainer,
    )
    .await
    .expect("maint maintains web");
    let one = format!("/api/v1/projects/{id}");
    let restore = format!("{one}/restore");
    let r = || router(&t.db);
    let token = |scope: Scope, agent: bool| {
        Principal::authenticated(alice.user_id, false, true, Credential::Token(vec![scope]))
            .as_agent(agent)
    };

    // A Maintainer: 403. A read-only token of the Owner: 403 scope. An
    // agent holding the Owner's api token: 403 (D5 plan: agents denied).
    let a = call(r(), "DELETE", &one, session(maint.user_id, false), None).await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let b = call(r(), "DELETE", &one, token(Scope::ReadApi, false), None).await;
    assert_eq!(b.status, StatusCode::FORBIDDEN, "{}", b.json);
    let c = call(r(), "DELETE", &one, token(Scope::Api, true), None).await;
    assert_eq!(
        (c.status, code(&c)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    assert_eq!(state_of(&t, id).await, (false, false));
    // The Owner with an api token may; an administrator may restore.
    let d = call(r(), "DELETE", &one, token(Scope::Api, false), None).await;
    assert_eq!(d.status, StatusCode::OK, "{}", d.json);
    let e = call(r(), "POST", &restore, session(maint.user_id, false), None).await;
    // A Maintainer sees the pending project: 410 like every other route.
    assert_eq!((e.status, code(&e)), (StatusCode::GONE, "FG-REPO-0006"));
    let f = call(r(), "POST", &restore, token(Scope::Api, true), None).await;
    assert_eq!(
        (f.status, code(&f)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let g = call(r(), "POST", &restore, session(root.user_id, true), None).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.json);
    // An administrator deletes any project; deleted_by is the administrator.
    let h = call(r(), "DELETE", &one, session(root.user_id, true), None).await;
    assert_eq!(h.status, StatusCode::OK, "{}", h.json);
    let rows = t
        .json_rows(&format!(
            "SELECT deleted_by AS by FROM forge.projects WHERE id = {id}"
        ))
        .await;
    assert_eq!(rows[0]["by"], json!(root.user_id.get()));
    // Malformed and absent ids.
    for uri in ["/api/v1/projects/01/restore", "/api/v1/projects/0/restore"] {
        let bad = call(r(), "POST", uri, session(root.user_id, true), None).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{uri}");
    }
    let absent = call(
        r(),
        "POST",
        "/api/v1/projects/9999/restore",
        session(root.user_id, true),
        None,
    )
    .await;
    assert_eq!(
        (absent.status, code(&absent)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    t.drop_now().await;
}

/// A restore that lost to the purge start: FG-REPO-0019 `purging`, 409.
#[tokio::test]
async fn restoring_a_project_whose_repository_is_purging_is_409() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let id = create_project(&t, &alice, "web").await;
    let one = format!("/api/v1/projects/{id}");
    let deleted = call(
        router(&t.db),
        "DELETE",
        &one,
        session(alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK);
    t.commit_audited(
        ("project", id),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now() - interval '2 days', \
             purge_after = now() - interval '1 second' WHERE id = {id}"
        )],
    )
    .await;
    t.commit_audited(
        ("project", id),
        &[format!(
            "UPDATE forge.repositories SET state = 'purging' WHERE project_id = {id}"
        )],
    )
    .await;
    let restore = call(
        router(&t.db),
        "POST",
        &format!("{one}/restore"),
        session(alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(
        (restore.status, code(&restore)),
        (StatusCode::CONFLICT, "FG-REPO-0019")
    );
    assert_eq!(state_of(&t, id).await, (true, true));
    t.drop_now().await;
}

/// Smart HTTP agrees with the API: a clone of a project pending deletion is
/// 410 (FG-REPO-0006) for a caller who can see it and the absent 404 for
/// anyone else, decided by the same `authorize()` before git runs.
#[tokio::test]
async fn a_clone_of_a_project_pending_deletion_is_410_for_members_and_404_for_others() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let id = create_project(&t, &alice, "web").await;
    let deleted = call(
        router(&t.db),
        "DELETE",
        &format!("/api/v1/projects/{id}"),
        session(alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK);
    let advert = "/alice/web.git/info/refs?service=git-upload-pack";
    let mine = call(
        router(&t.db),
        "GET",
        advert,
        session(alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(
        (mine.status, code(&mine)),
        (StatusCode::GONE, "FG-REPO-0006")
    );
    let theirs = call(
        router(&t.db),
        "GET",
        advert,
        session(bob.user_id, false),
        None,
    )
    .await;
    assert_eq!(
        (theirs.status, code(&theirs)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let push = "/alice/web.git/info/refs?service=git-receive-pack";
    let mine = call(
        router(&t.db),
        "GET",
        push,
        session(alice.user_id, false),
        None,
    )
    .await;
    assert_eq!(
        (mine.status, code(&mine)),
        (StatusCode::GONE, "FG-REPO-0006")
    );
    t.drop_now().await;
}
