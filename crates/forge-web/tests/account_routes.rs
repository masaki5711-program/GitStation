#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The account routes of M2 MR6 through the product router against the
//! development database (ADR-0011), with the real loaders and commands:
//! tokens (`/api/v1/user/tokens`), sessions (`/api/v1/user/sessions`,
//! `/api/v1/admin/users/{id}/sessions`), the user search and the user
//! administration — the permission matrix's rows over HTTP (allow and
//! deny), a token never minting a token, another user's token or session
//! answered byte for byte like an absent one, the admin list's cursor
//! bound to its filter, and an HTTP disable that never passes the
//! administration guard. Principals are inserted as request extensions
//! (the authentication layer is tested elsewhere). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::{CursorKeys, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, NewGroup};
use forge_db::test_support::{issue_session, FreshDb};
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_hosting::Hosting;
use forge_web::{AppConfig, AppState, Lists, Lookups};
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
    let keys = Arc::new(CursorKeys::parse(&format!("3 {}\n", "6b".repeat(32))).expect("keys"));
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(db.clone()),
            projects: Arc::new(db.clone()),
        },
    )
    .with_hosting(Hosting::new(db.clone()))
    .with_lists(Lists::new(Arc::new(db.clone()), Arc::new(db.clone()), keys));
    forge_web::app(state, AppConfig::default()).expect("router")
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn user(t: &FreshDb, name: &str, admin: bool) -> UserId {
    let mut new = NewUser::human(
        name,
        &format!("Synthetic {name}"),
        &format!("{name}@example.test"),
    );
    new.is_admin = admin;
    users::create(&t.db, &ctx(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
        .user_id
}

struct Answer {
    status: StatusCode,
    json: Value,
    raw: Vec<u8>,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }

    /// The body without its request id (each request has its own).
    fn without_request_id(&self) -> Value {
        let mut v = self.json.clone();
        if let Some(m) = v.as_object_mut() {
            m.remove("request_id");
        }
        v
    }
}

async fn call(
    db: &Db,
    method: &str,
    uri: &str,
    who: Option<Principal>,
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
    if let Some(who) = who {
        req.extensions_mut().insert(who);
    }
    let res = router(db).oneshot(req).await.expect("infallible");
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
        raw,
    }
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session)
}

fn token(user: UserId, admin: bool, scopes: &[Scope]) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Token(scopes.to_vec()))
}

// ── tokens ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_session_creates_lists_and_revokes_its_tokens_and_a_token_never_mints_one() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let root = user(&t, "root", true).await;
    let req =
        json!({"name": "ci", "scopes": ["read_api", "write_repository"], "expires_in_days": 30});

    let created = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        Some(session(alice, false)),
        Some(req.clone()),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let secret = created.json["token"].as_str().expect("the secret, once");
    assert!(secret.starts_with("fgp_"));
    assert_eq!(
        created.json["scopes"],
        json!(["read_api", "write_repository"])
    );
    assert_eq!(created.json["is_agent"], false);
    let id = created.json["id"].as_i64().expect("id");

    // A token never creates a token, whatever its scopes (sessions only,
    // FG-AUTH-0002 session_required).
    for who in [
        token(alice, false, &[Scope::Api]),
        token(root, true, &[Scope::Api, Scope::Admin]),
    ] {
        let refused = call(
            &t.db,
            "POST",
            "/api/v1/user/tokens",
            Some(who),
            Some(req.clone()),
        )
        .await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002")
        );
    }
    // The admin scope only for an administrator.
    let admin_scope = json!({"name": "x", "scopes": ["admin"]});
    let refused = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        Some(session(alice, false)),
        Some(admin_scope.clone()),
    )
    .await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let ok = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        Some(session(root, true)),
        Some(admin_scope),
    )
    .await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.json);
    // Anonymous, unknown scope, unknown field.
    let anon = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        None,
        Some(req.clone()),
    )
    .await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    let bad = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        Some(session(alice, false)),
        Some(json!({"name": "x", "scopes": ["root"]})),
    )
    .await;
    assert_eq!(
        (bad.status, bad.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let extra = call(
        &t.db,
        "POST",
        "/api/v1/user/tokens",
        Some(session(alice, false)),
        Some(json!({"name": "x", "scopes": ["api"], "user_id": 2})),
    )
    .await;
    assert_eq!(extra.status, StatusCode::BAD_REQUEST);

    // The list: no secret; a read_api token reads it, a repository token does not.
    let listed = call(
        &t.db,
        "GET",
        "/api/v1/user/tokens",
        Some(token(alice, false, &[Scope::ReadApi])),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed.json["items"][0]["id"], id);
    assert!(
        !String::from_utf8_lossy(&listed.raw).contains(&secret[20..]),
        "never the secret"
    );
    let repo = call(
        &t.db,
        "GET",
        "/api/v1/user/tokens",
        Some(token(alice, false, &[Scope::ReadRepository])),
        None,
    )
    .await;
    assert_eq!(
        (repo.status, repo.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    let bobs = call(
        &t.db,
        "GET",
        "/api/v1/user/tokens",
        Some(session(bob, false)),
        None,
    )
    .await;
    assert_eq!(bobs.json["items"], json!([]));

    // Another user's token is answered exactly like an absent one.
    let foreign = call(
        &t.db,
        "DELETE",
        &format!("/api/v1/user/tokens/{id}"),
        Some(session(bob, false)),
        None,
    )
    .await;
    let absent = call(
        &t.db,
        "DELETE",
        "/api/v1/user/tokens/999999",
        Some(session(bob, false)),
        None,
    )
    .await;
    assert_eq!(
        (foreign.status, foreign.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert_eq!(foreign.without_request_id(), absent.without_request_id());
    let read_only = call(
        &t.db,
        "DELETE",
        &format!("/api/v1/user/tokens/{id}"),
        Some(token(alice, false, &[Scope::ReadApi])),
        None,
    )
    .await;
    assert_eq!(
        (read_only.status, read_only.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    let revoked = call(
        &t.db,
        "DELETE",
        &format!("/api/v1/user/tokens/{id}"),
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let again = call(
        &t.db,
        "DELETE",
        &format!("/api/v1/user/tokens/{id}"),
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NO_CONTENT, "idempotent");
    let after = call(
        &t.db,
        "GET",
        "/api/v1/user/tokens",
        Some(session(alice, false)),
        None,
    )
    .await;
    assert!(after.json["items"][0]["revoked_at"].is_string());
    let bad_id = call(
        &t.db,
        "DELETE",
        "/api/v1/user/tokens/01",
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(
        (bad_id.status, bad_id.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    t.drop_now().await;
}

// ── sessions ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn sessions_are_listed_and_ended_by_their_owner_and_by_the_administration() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let bob = user(&t, "bob", false).await;
    let root = user(&t, "root", true).await;
    let a1 = issue_session(&t.db, alice).await;
    let _a2 = issue_session(&t.db, alice).await;
    let b1 = issue_session(&t.db, bob).await;
    let b1_id = b1.token.public_id().as_str().to_owned();

    let own = call(
        &t.db,
        "GET",
        "/api/v1/user/sessions",
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.json);
    assert_eq!(own.json["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(own.json["truncated"], false);
    let text = String::from_utf8_lossy(&own.raw);
    assert!(!text.contains("secret") && !text.contains(&b1_id));
    // Tokens never see or end a browser session (FG-AUTH-0002), admins' too.
    for who in [
        token(alice, false, &[Scope::Api]),
        token(root, true, &[Scope::Api, Scope::Admin]),
    ] {
        let refused = call(&t.db, "GET", "/api/v1/user/sessions", Some(who), None).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002")
        );
    }
    let anon = call(&t.db, "GET", "/api/v1/user/sessions", None, None).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);

    // Another user's session through the own route: the same 404 as an
    // absent one, for an administrator too.
    let one = |id: &str| format!("/api/v1/user/sessions/{id}");
    let absent = call(
        &t.db,
        "DELETE",
        &one("0000000000000000"),
        Some(session(alice, false)),
        None,
    )
    .await;
    for who in [session(alice, false), session(root, true)] {
        let foreign = call(&t.db, "DELETE", &one(&b1_id), Some(who), None).await;
        assert_eq!(
            (foreign.status, foreign.code()),
            (StatusCode::NOT_FOUND, "FG-REPO-0005")
        );
        assert_eq!(foreign.without_request_id(), absent.without_request_id());
    }
    let malformed = call(
        &t.db,
        "DELETE",
        &one("XYZ"),
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(
        (malformed.status, malformed.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let ended = call(
        &t.db,
        "DELETE",
        &one(a1.token.public_id().as_str()),
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(ended.status, StatusCode::NO_CONTENT);
    let own = call(
        &t.db,
        "GET",
        "/api/v1/user/sessions",
        Some(session(alice, false)),
        None,
    )
    .await;
    assert_eq!(own.json["items"].as_array().map(Vec::len), Some(1));
    let reason = t
        .json_rows(&format!(
            "SELECT revoked_reason FROM forge.web_sessions WHERE public_id = '{}'",
            a1.token.public_id().as_str()
        ))
        .await;
    assert_eq!(reason[0]["revoked_reason"], "logout");

    // The administration's route.
    let admin = format!("/api/v1/admin/users/{}/sessions", bob.get());
    for (who, code) in [
        (session(alice, false), "FG-AUTH-0003"),
        (token(root, true, &[Scope::Api]), "FG-AUTH-0003"),
    ] {
        let refused = call(&t.db, "GET", &admin, Some(who), None).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, code)
        );
    }
    let listed = call(
        &t.db,
        "GET",
        &admin,
        Some(token(root, true, &[Scope::ReadApi, Scope::Admin])),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.json);
    assert_eq!(listed.json["items"][0]["public_id"], b1_id.as_str());
    assert_eq!(listed.json["items"][0]["current"], false);
    let agent = token(root, true, &[Scope::Api, Scope::Admin]).as_agent(true);
    let refused = call(&t.db, "DELETE", &admin, Some(agent), None).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let all = call(&t.db, "DELETE", &admin, Some(session(root, true)), None).await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.json);
    assert_eq!(
        all.json,
        json!({"user_id": bob.get(), "revoked_sessions": 1})
    );
    let audit = t
        .json_rows(&format!(
            "SELECT actor_kind, actor_user_id, details FROM forge.audit_log \
             WHERE action = 'session.revoked_all' AND resource_id = {}",
            bob.get()
        ))
        .await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["actor_user_id"], root.get());
    assert_eq!(audit[0]["details"]["reason"], "admin");
    let unknown = call(
        &t.db,
        "DELETE",
        "/api/v1/admin/users/999999/sessions",
        Some(session(root, true)),
        None,
    )
    .await;
    assert_eq!(
        (unknown.status, unknown.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    t.drop_now().await;
}

// ── users ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_signed_in_user_searches_names_and_only_administrators_list_users() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice", false).await;
    let _albert = user(&t, "albert", false).await;
    let root = user(&t, "root", true).await;
    let found = call(
        &t.db,
        "GET",
        "/api/v1/users?search=al",
        Some(token(alice, false, &[Scope::ReadApi])),
        None,
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.json);
    let items = found.json["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    let keys: Vec<&String> = items[0].as_object().expect("object").keys().collect();
    assert_eq!(keys, ["display_name", "id", "username"], "names only");
    for (uri, code) in [
        ("/api/v1/users?search=a", "FG-WEB-0006"),
        ("/api/v1/users", "FG-WEB-0006"),
        ("/api/v1/users?search=al&page=2", "FG-WEB-0006"),
    ] {
        let bad = call(&t.db, "GET", uri, Some(session(alice, false)), None).await;
        assert_eq!(
            (bad.status, bad.code()),
            (StatusCode::BAD_REQUEST, code),
            "{uri}"
        );
    }
    let repo = call(
        &t.db,
        "GET",
        "/api/v1/users?search=al",
        Some(token(alice, false, &[Scope::ReadRepository])),
        None,
    )
    .await;
    assert_eq!(
        (repo.status, repo.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    let anon = call(&t.db, "GET", "/api/v1/users?search=al", None, None).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);

    // The administration list.
    for who in [session(alice, false), token(root, true, &[Scope::ReadApi])] {
        let refused = call(&t.db, "GET", "/api/v1/admin/users", Some(who), None).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003")
        );
    }
    let admin = token(root, true, &[Scope::ReadApi, Scope::Admin]);
    let page = call(
        &t.db,
        "GET",
        "/api/v1/admin/users?limit=2",
        Some(admin.clone()),
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.json);
    assert_eq!(page.json["items"].as_array().map(Vec::len), Some(2));
    assert_eq!(page.json["has_more"], true);
    let cursor = page.json["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_owned();
    let next = call(
        &t.db,
        "GET",
        &format!("/api/v1/admin/users?limit=2&cursor={cursor}"),
        Some(admin.clone()),
        None,
    )
    .await;
    assert_eq!(next.json["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(next.json["items"][0]["username"], "root");
    assert_eq!(next.json["has_more"], false);
    // The cursor is bound to its filter (and its caller).
    let other = call(
        &t.db,
        "GET",
        &format!("/api/v1/admin/users?limit=2&state=active&cursor={cursor}"),
        Some(admin),
        None,
    )
    .await;
    assert_eq!(
        (other.status, other.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006")
    );
    let disabled = call(
        &t.db,
        "GET",
        "/api/v1/admin/users?state=disabled",
        Some(session(root, true)),
        None,
    )
    .await;
    assert_eq!(disabled.json["items"], json!([]));
    t.drop_now().await;
}

/// An HTTP disable never passes the administration guard: the last
/// administrator and the last Owner of a group stay active, nothing is
/// written; a body asking for force is not part of the route.
#[tokio::test]
async fn an_http_disable_never_passes_the_administration_guard() {
    let t = FreshDb::migrated().await;
    let root = user(&t, "root", true).await;
    let bob = user(&t, "bob", false).await;
    let carol = user(&t, "carol", false).await;
    groups::create(
        &t.db,
        &ctx("req-group"),
        &NewGroup {
            segment: "acme".to_owned(),
            parent: None,
            display_name: "Acme".to_owned(),
            description: String::new(),
            owner: bob,
        },
    )
    .await
    .expect("group");
    let uri = |u: UserId| format!("/api/v1/admin/users/{}/disable", u.get());
    // Not an administrator; an agent; a token without the admin scope.
    for (who, code) in [
        (session(carol, false), "FG-AUTH-0003"),
        (
            token(root, true, &[Scope::Api, Scope::Admin]).as_agent(true),
            "FG-AUTH-0003",
        ),
        (token(root, true, &[Scope::Api]), "FG-AUTH-0003"),
        (
            token(root, true, &[Scope::ReadApi, Scope::Admin]),
            "FG-AUTH-0002",
        ),
    ] {
        let refused = call(&t.db, "POST", &uri(carol), Some(who), None).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, code)
        );
    }
    // The last administrator, the last Owner: FG-AUTH-0009, nothing written.
    let last_admin = call(&t.db, "POST", &uri(root), Some(session(root, true)), None).await;
    assert_eq!(
        (last_admin.status, last_admin.code()),
        (StatusCode::CONFLICT, "FG-AUTH-0009")
    );
    let last_owner = call(&t.db, "POST", &uri(bob), Some(session(root, true)), None).await;
    assert_eq!(
        (last_owner.status, last_owner.code()),
        (StatusCode::CONFLICT, "FG-AUTH-0009")
    );
    // A body (asking for force) does not change the route: still refused.
    let forced = call(
        &t.db,
        "POST",
        &uri(bob),
        Some(session(root, true)),
        Some(json!({"force": true})),
    )
    .await;
    assert_eq!(forced.code(), "FG-AUTH-0009");
    let states = t
        .json_rows("SELECT username, state FROM forge.users ORDER BY id")
        .await;
    assert!(states.iter().all(|r| r["state"] == "active"), "{states:?}");
    let disables = t
        .json_rows("SELECT id FROM forge.audit_log WHERE action = 'user.disabled'")
        .await;
    assert!(disables.is_empty());
    // A disable the guard allows: the user's sessions end with it.
    let _s = issue_session(&t.db, carol).await;
    let ok = call(&t.db, "POST", &uri(carol), Some(session(root, true)), None).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.json);
    assert_eq!(
        ok.json,
        json!({"user_id": carol.get(), "outcome": "disabled"})
    );
    let again = call(&t.db, "POST", &uri(carol), Some(session(root, true)), None).await;
    assert_eq!(again.json["outcome"], "already_disabled");
    let live = t
        .json_rows(&format!(
            "SELECT id FROM forge.web_sessions WHERE user_id = {} AND revoked_at IS NULL",
            carol.get()
        ))
        .await;
    assert!(live.is_empty());
    let unknown = call(
        &t.db,
        "POST",
        "/api/v1/admin/users/999999/disable",
        Some(session(root, true)),
        None,
    )
    .await;
    assert_eq!(
        (unknown.status, unknown.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    t.drop_now().await;
}
