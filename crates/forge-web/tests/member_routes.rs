#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The member and protected-ref routes of M1 phase 8b MR8 through the
//! product router, against the development database (ADR-0011) with the
//! real loaders and forge-hosting's commands: the registry's proof on the
//! member's / rule's stored role, then the command's own decision inside
//! its transaction (last owner, agents, self-elevation).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::Scope;
use forge_core::{RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::projects::{self, NewProject};
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
    let state = state(db).with_hosting(Hosting::new(db.clone()));
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

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(scopes.to_vec())).as_agent(agent)
}

async fn acme(t: &FreshDb, owner: UserId) -> CreatedGroup {
    groups::create(
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
    .expect("acme")
}

#[tokio::test]
async fn project_members_are_listed_added_changed_and_removed_through_the_routes() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let carol = user(&t, "carol", false).await;
    let gina = user(&t, "gina", false).await;
    let group = acme(&t, alice.user_id).await;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", group.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let p = web.project_id.get();
    for (who, role) in [(bob.user_id, Role::Maintainer), (gina.user_id, Role::Guest)] {
        projects::add_member(&t.db, &ctx("req-add"), web.project_id, who, role)
            .await
            .expect("member");
    }
    let r = || router(&t.db);
    let as_bob = session(bob.user_id, false);
    let members = format!("/api/v1/projects/{p}/members");
    let carol_uri = format!("{members}/{}", carol.user_id.get());

    // A Guest does not read the member list of a private project.
    let guest = call(r(), "GET", &members, session(gina.user_id, false), None).await;
    assert_eq!(
        (guest.status, code(&guest)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let stranger = call(r(), "GET", &members, session(carol.user_id, false), None).await;
    assert_eq!(
        (stranger.status, code(&stranger)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );

    let add = json!({"user_id": carol.user_id.get(), "role": "developer"});
    let added = call(r(), "POST", &members, as_bob.clone(), Some(add.clone())).await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.json);
    assert_eq!(added.json["username"], "carol");
    assert_eq!(added.json["role"], "developer");
    let again = call(r(), "POST", &members, as_bob.clone(), Some(add)).await;
    assert_eq!(
        (again.status, code(&again)),
        (StatusCode::CONFLICT, "FG-REPO-0010")
    );
    // self-elevation and granting above one's own role are refused.
    let bob_uri = format!("{members}/{}", bob.user_id.get());
    for (method, uri, body) in [
        ("PATCH", bob_uri.as_str(), json!({"role": "owner"})),
        ("PATCH", carol_uri.as_str(), json!({"role": "owner"})),
        (
            "POST",
            members.as_str(),
            json!({"user_id": 9999, "role": "owner"}),
        ),
    ] {
        let denied = call(r(), method, uri, as_bob.clone(), Some(body)).await;
        assert_eq!(
            (denied.status, code(&denied)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{method} {uri}"
        );
    }
    // A Developer manages nobody (adding themself as Owner included).
    let as_carol = session(carol.user_id, false);
    let denied = call(
        r(),
        "PATCH",
        &carol_uri,
        as_carol.clone(),
        Some(json!({"role": "owner"})),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let changed = call(
        r(),
        "PATCH",
        &carol_uri,
        as_bob.clone(),
        Some(json!({"role": "maintainer"})),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.json);
    assert_eq!(changed.json["role"], "maintainer");

    // The list: direct members in user id order, paged.
    let page = call(
        r(),
        "GET",
        &format!("{members}?limit=2"),
        as_bob.clone(),
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.json);
    let ids: Vec<i64> = page.json["members"]
        .as_array()
        .expect("members")
        .iter()
        .map(|m| m["user_id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, [alice.user_id.get(), bob.user_id.get()]);
    let next = page.json["next_after"].as_i64().expect("next");
    let rest = call(
        r(),
        "GET",
        &format!("{members}?limit=100&after={next}"),
        as_bob.clone(),
        None,
    )
    .await;
    assert_eq!(rest.json["members"].as_array().expect("rest").len(), 2);
    assert_eq!(rest.json["next_after"], Value::Null);
    let bad = call(
        r(),
        "GET",
        &format!("{members}?limit=0"),
        as_bob.clone(),
        None,
    )
    .await;
    assert_eq!(
        (bad.status, code(&bad)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );

    let removed = call(r(), "DELETE", &carol_uri, as_bob.clone(), None).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let gone = call(r(), "DELETE", &carol_uri, as_bob.clone(), None).await;
    assert_eq!(
        (gone.status, code(&gone)),
        (StatusCode::CONFLICT, "FG-REPO-0010")
    );
    let malformed = call(
        r(),
        "DELETE",
        &format!("{members}/01"),
        as_bob.clone(),
        None,
    )
    .await;
    assert_eq!(
        (malformed.status, code(&malformed)),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    t.drop_now().await;
}

#[tokio::test]
async fn tokens_agents_and_the_last_owner_are_decided_on_the_member_routes() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let group = acme(&t, alice.user_id).await;
    let g = group.group_id.get();
    let members = format!("/api/v1/groups/{g}/members");
    let alice_uri = format!("{members}/{}", alice.user_id.get());
    let r = || router(&t.db);
    let add = json!({"user_id": bob.user_id.get(), "role": "developer"});

    // Every signed-in user reads a group's members.
    let listed = call(r(), "GET", &members, session(bob.user_id, false), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["members"][0]["role"], "owner");

    for (who, expected) in [
        (
            token(alice.user_id, &[Scope::ReadApi], false),
            "FG-AUTH-0002",
        ),
        (
            token(alice.user_id, &[Scope::WriteRepository], false),
            "FG-AUTH-0002",
        ),
        // agents never manage members, an Owner's token included
        (token(alice.user_id, &[Scope::Api], true), "FG-AUTH-0003"),
    ] {
        let denied = call(r(), "POST", &members, who, Some(add.clone())).await;
        assert_eq!(code(&denied), expected, "{}", denied.json);
    }
    // ...but an agent reads the list.
    let read = call(
        r(),
        "GET",
        &members,
        token(alice.user_id, &[Scope::ReadApi], true),
        None,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    let disabled = Principal::authenticated(alice.user_id, false, false, Credential::Session);
    let denied = call(r(), "POST", &members, disabled, Some(add.clone())).await;
    assert_eq!(code(&denied), "FG-AUTH-0004");

    let added = call(
        r(),
        "POST",
        &members,
        token(alice.user_id, &[Scope::Api], false),
        Some(add),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.json);
    // alice is acme's only Owner: neither removed nor demoted (403, decided
    // inside the transaction).
    let as_alice = session(alice.user_id, false);
    for (method, body) in [
        ("DELETE", None),
        ("PATCH", Some(json!({"role": "maintainer"}))),
    ] {
        let refused = call(r(), method, &alice_uri, as_alice.clone(), body).await;
        assert_eq!(
            (refused.status, code(&refused)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{method}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn protected_refs_are_listed_created_changed_and_deleted_through_the_routes() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let dave = user(&t, "dave", false).await;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    for (who, role) in [
        (bob.user_id, Role::Maintainer),
        (dave.user_id, Role::Developer),
    ] {
        projects::add_member(&t.db, &ctx("req-add"), web.project_id, who, role)
            .await
            .expect("member");
    }
    let rules = format!("/api/v1/projects/{}/protected-refs", web.project_id.get());
    let r = || router(&t.db);
    let as_bob = session(bob.user_id, false);
    let as_alice = session(alice.user_id, false);

    let body = json!({"pattern": "refs/heads/main", "push_min_role": "maintainer"});
    let denied = call(
        r(),
        "POST",
        &rules,
        session(dave.user_id, false),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (denied.status, code(&denied)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let agent = call(
        r(),
        "POST",
        &rules,
        token(alice.user_id, &[Scope::Api], true),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        (agent.status, code(&agent)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let created = call(r(), "POST", &rules, as_bob.clone(), Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["push_min_role"], "maintainer");
    let id = created.json["id"].as_i64().expect("id");
    let one = format!("{rules}/{id}");
    let taken = call(
        r(),
        "POST",
        &rules,
        as_bob.clone(),
        Some(json!({"pattern": "refs/heads/MAIN", "push_min_role": "developer"})),
    )
    .await;
    assert_eq!(
        (taken.status, code(&taken)),
        (StatusCode::CONFLICT, "FG-REPO-0017")
    );
    let invalid = call(
        r(),
        "POST",
        &rules,
        as_bob.clone(),
        Some(json!({"pattern": "refs/notes/x", "push_min_role": "developer"})),
    )
    .await;
    assert_eq!(
        (invalid.status, code(&invalid)),
        (StatusCode::BAD_REQUEST, "FG-REPO-0013")
    );
    // Above one's own role: a Maintainer neither sets nor lifts Owner level.
    let raise = json!({"push_min_role": "owner"});
    let denied = call(r(), "PATCH", &one, as_bob.clone(), Some(raise.clone())).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let raised = call(r(), "PATCH", &one, as_alice.clone(), Some(raise)).await;
    assert_eq!(raised.status, StatusCode::OK, "{}", raised.json);
    assert_eq!(raised.json["push_min_role"], "owner");
    let denied = call(r(), "DELETE", &one, as_bob.clone(), None).await;
    assert_eq!(
        (denied.status, code(&denied)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );

    // A Developer reads the rules.
    let listed = call(r(), "GET", &rules, session(dave.user_id, false), None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["rules"][0]["pattern"], "refs/heads/main");

    let deleted = call(r(), "DELETE", &one, as_alice.clone(), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let gone = call(r(), "DELETE", &one, as_alice.clone(), None).await;
    assert_eq!(
        (gone.status, code(&gone)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    let pending = call(
        r(),
        "GET",
        "/api/v1/projects/9999/protected-refs",
        as_alice.clone(),
        None,
    )
    .await;
    assert_eq!(pending.status, StatusCode::NOT_FOUND);
    t.drop_now().await;
}
