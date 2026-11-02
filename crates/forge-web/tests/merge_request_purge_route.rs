#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `POST /api/v1/projects/{id}/merge-requests/{iid}/version-refs/purge`
//! (M3 R1b, ADR-0024 §2) through the product router, against the
//! development database and a real (empty) bare repository: an
//! administrator's session reaches the command; every token — the `admin`
//! scope and an agent's included — is refused before the lookup; a
//! non-administrator is refused; an unknown version is FG-WEB-0006.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, RefName, RepoId, RequestId, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{BranchProbe, Review, VersionRefs};
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

/// Every branch exists at one head.
struct AnyBranch;

impl BranchProbe for AnyBranch {
    fn branch_head<'a>(
        &'a self,
        _repo: RepoId,
        _branch: &'a RefName,
        _caller: &'a str,
    ) -> LookupFuture<'a, ObjectId> {
        Box::pin(async { ObjectId::parse(&"1".repeat(40)) })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn call(router: axum::Router, uri: &str, who: Principal, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    req.extensions_mut().insert(who);
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn session(user: UserId, admin: bool) -> Principal {
    Principal::authenticated(user, admin, true, Credential::Session)
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(user, true, true, Credential::Token(scopes.to_vec())).as_agent(agent)
}

#[tokio::test]
async fn only_an_administrators_session_purges_version_refs() {
    let t = FreshDb::migrated().await;
    let admin = users::create(
        &t.db,
        &ctx("req-admin"),
        &NewUser {
            is_admin: true,
            ..NewUser::human("root", "root", "root@example.test")
        },
    )
    .await
    .expect("admin");
    let dev = users::create(
        &t.db,
        &ctx("req-dev"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dev");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", dev.namespace_id, "Web", dev.user_id),
    )
    .await
    .expect("web");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_review(
        Review::new(t.db.clone(), Arc::new(AnyBranch))
            .with_version_refs(VersionRefs::new(git, storage)),
    );
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let p = web.project_id.get();
    let (status, opened) = call(
        router.clone(),
        &format!("/api/v1/projects/{p}/merge-requests"),
        session(dev.user_id, false),
        json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let uri = format!("/api/v1/projects/{p}/merge-requests/1/version-refs/purge");

    for who in [
        token(admin.user_id, &[Scope::Api, Scope::Admin], false),
        token(admin.user_id, &[Scope::Api, Scope::Admin], true),
    ] {
        let (status, body) = call(router.clone(), &uri, who, json!({})).await;
        assert_eq!(
            (status, body["error_code"].as_str()),
            (StatusCode::FORBIDDEN, Some("FG-AUTH-0002")),
            "{body}"
        );
    }
    let (status, body) = call(router.clone(), &uri, session(dev.user_id, false), json!({})).await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::FORBIDDEN, Some("FG-AUTH-0003")),
        "the author, not an administrator: {body}"
    );
    let (status, body) = call(
        router.clone(),
        &uri,
        session(admin.user_id, true),
        json!({"versions": [7]}),
    )
    .await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("FG-WEB-0006")),
        "{body}"
    );
    // No version recorded yet: nothing to purge, the refs checked anyway.
    let (status, body) = call(
        router.clone(),
        &uri,
        session(admin.user_id, true),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"iid": 1, "purged": [], "already_purged": []}));
    let audit = t
        .json_rows(
            "SELECT count(*) AS n FROM forge.audit_log \
             WHERE action = 'merge_request.version_refs_purged'",
        )
        .await;
    assert_eq!(audit[0]["n"], 0, "nothing marked, nothing audited");
    t.drop_now().await;
}
