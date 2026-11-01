#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET /api/v1/user` (`UserSelfRead`, M1 phase 8b MR6) through the product
//! router with a fake authenticator (synthetic identities): the caller's
//! own account from the token's authentication, the matrix rows over HTTP
//! (anonymous 401, wrong scope 403, human / admin / agent 200) and no
//! shared caching.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_core::authenticator::{AuthFuture, Authenticator, TokenIdentity};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{LookupPath, ProjectId, ProjectRecord, RequestId, Resolution, Scope, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

// Synthetic tokens in the real format.
const HUMAN: &str =
    "fgp_00000000000000a1_1111111111111111111111111111111111111111111111111111111111111111";
const ADMIN: &str =
    "fgp_00000000000000a2_2222222222222222222222222222222222222222222222222222222222222222";
const AGENT: &str =
    "fgp_00000000000000a3_3333333333333333333333333333333333333333333333333333333333333333";
const REPO_ONLY: &str =
    "fgp_00000000000000a4_4444444444444444444444444444444444444444444444444444444444444444";
const DISABLED: &str =
    "fgp_00000000000000a5_5555555555555555555555555555555555555555555555555555555555555555";

struct Fake;

fn identity(user: i64, is_admin: bool, is_agent: bool, scopes: &[Scope]) -> TokenIdentity {
    TokenIdentity {
        token_id: user * 10,
        user_id: UserId::new(user),
        username: format!("user{user}"),
        display_name: format!("Synthetic User {user}"),
        is_admin,
        scopes: scopes.to_vec(),
        is_agent,
    }
}

impl Authenticator for Fake {
    fn authenticate<'a>(&'a self, _: &'a RequestId, presented: &'a str) -> AuthFuture<'a> {
        let result = match presented {
            HUMAN => Ok(identity(1, false, false, &[Scope::ReadApi])),
            ADMIN => Ok(identity(2, true, false, &[Scope::Api, Scope::Admin])),
            AGENT => Ok(identity(
                3,
                false,
                true,
                &[Scope::ReadApi, Scope::WriteRepository],
            )),
            REPO_ONLY => Ok(identity(4, false, false, &[Scope::ReadRepository])),
            DISABLED => Err(ForgeError::new(ErrorCode::AccountDisabled)
                .with_context("reason", "owner_disabled")),
            _ => Err(ForgeError::new(ErrorCode::Unauthenticated).with_context("reason", "unknown")),
        };
        Box::pin(async move { result })
    }
}

struct NoLookups;

impl forge_core::NamespaceResolver for NoLookups {
    fn resolve<'a>(&'a self, _: &'a LookupPath, _: Option<UserId>) -> LookupFuture<'a, Resolution> {
        Box::pin(async { Ok(Resolution::Absent) })
    }
}

impl forge_core::ProjectAccessLoader for NoLookups {
    fn load_project<'a>(
        &'a self,
        _: ProjectId,
        _: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

async fn get(token: Option<&str>) -> (StatusCode, Option<String>, Value) {
    let lookups = Lookups {
        namespaces: Arc::new(NoLookups),
        projects: Arc::new(NoLookups),
    };
    let state = AppState::new("test", Vec::new(), Arc::new(Fake), lookups);
    let app = forge_web::app(state, AppConfig::default()).expect("router");
    let mut req = Request::get("/api/v1/user");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let res = app
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = res.status();
    let cache = res
        .headers()
        .get("cache-control")
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        cache,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_human_an_administrator_and_an_agent_read_their_own_account() {
    let (status, cache, body) = get(Some(HUMAN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cache.as_deref(), Some("private, no-store"));
    assert_eq!(
        body,
        json!({
            "id": 1,
            "username": "user1",
            "display_name": "Synthetic User 1",
            "is_admin": false,
            "is_agent": false,
            "token_scopes": ["read_api"],
        })
    );
    let (status, _, admin) = get(Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(admin["is_admin"], true);
    assert_eq!(admin["token_scopes"], json!(["api", "admin"]));
    let (status, _, agent) = get(Some(AGENT)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(agent["id"], 3);
    assert_eq!(agent["is_agent"], true);
}

#[tokio::test]
async fn anonymous_disabled_and_repository_only_tokens_are_refused() {
    let (status, _, body) = get(None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error_code"], ErrorCode::Unauthenticated.id());
    let (status, _, body) = get(Some(DISABLED)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error_code"], ErrorCode::AccountDisabled.id());
    let (status, _, body) = get(Some(REPO_ONLY)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error_code"], ErrorCode::InsufficientScope.id());
    assert!(body.get("username").is_none());
}
