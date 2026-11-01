#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET /api/v1/namespaces/resolve?path=…&prefix=true` (M2 MR6, ADR-0015
//! "GitHub の形の URL の救済") through the product router with a fake
//! resolver (synthetic paths): the longest prefix that is a project the
//! caller can see, with the rest as `remainder`; **nothing else is ever
//! revealed** — an invisible project, a group alone, a tombstone and an
//! absent path answer byte for byte the same `FG-REPO-0005` (only the
//! request id differs), after resolving every candidate prefix whatever
//! exists; the per-caller rate limit (`FG-WEB-0010`, `Retry-After`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    GroupId, LookupPath, NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord,
    ProjectRepository, RepoId, RepositoryState, RequestId, Resolution, Resolved, ResolvedTarget,
    Role, TombstoneId, UserId, Visibility,
};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const ALICE: UserId = UserId::new(1);
const BOB: UserId = UserId::new(2);

/// Synthetic namespaces: `acme` (group), `acme/web` (internal project),
/// `acme/secret` (private; bob is a Developer), `acme/gone` (private,
/// pending deletion; bob), `old/web` (a moved tombstone to `acme/web`).
struct Fake {
    calls: AtomicUsize,
}

fn resolved(id: i64, visibility: Visibility, role: Option<Role>, pending: bool) -> Resolved {
    Resolved {
        namespace_id: NamespaceId::new(100 + id),
        target: ResolvedTarget::Project {
            id: ProjectId::new(id),
            access: ProjectAccess {
                visibility,
                role,
                pending_deletion: pending,
            },
            repository: ProjectRepository {
                id: RepoId::new(id),
                state: RepositoryState::Ready,
            },
        },
    }
}

fn project(id: i64, visibility: Visibility, role: Option<Role>, pending: bool) -> Resolution {
    Resolution::Live(resolved(id, visibility, role, pending))
}

impl forge_core::NamespaceResolver for Fake {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bob = |r: Role| (viewer == Some(BOB)).then_some(r);
        let answer = match path.full_path().as_str() {
            "acme" => Resolution::Live(Resolved {
                namespace_id: NamespaceId::new(10),
                target: ResolvedTarget::Group {
                    id: GroupId::new(1),
                    viewer_role: None,
                },
            }),
            "acme/web" => project(1, Visibility::Internal, None, false),
            "acme/secret" => project(2, Visibility::Private, bob(Role::Developer), false),
            "acme/gone" => project(3, Visibility::Private, bob(Role::Owner), true),
            "old/web" => Resolution::Moved {
                resolved: resolved(1, Visibility::Internal, None, false),
                tombstone: TombstoneId::new(5),
                current: NamespacePath::parse("acme/web").expect("path"),
            },
            _ => Resolution::Absent,
        };
        Box::pin(async move { Ok(answer) })
    }
}

impl forge_core::ProjectAccessLoader for Fake {
    fn load_project<'a>(
        &'a self,
        _: ProjectId,
        _: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        Box::pin(async { Ok(None) })
    }
}

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

fn router(fake: &Arc<Fake>) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::clone(fake) as Arc<dyn forge_core::NamespaceResolver>,
            projects: Arc::clone(fake) as Arc<dyn forge_core::ProjectAccessLoader>,
        },
    );
    forge_web::app(state, AppConfig::default()).expect("router")
}

struct Answer {
    status: StatusCode,
    headers: Vec<(String, String)>,
    json: Value,
}

impl Answer {
    fn without_request_id(&self) -> Value {
        let mut v = self.json.clone();
        if let Some(m) = v.as_object_mut() {
            m.remove("request_id");
        }
        v
    }
}

async fn get(router: &axum::Router, uri: &str, who: Option<Principal>) -> Answer {
    let mut req = Request::get(uri).body(Body::empty()).expect("request");
    if let Some(who) = who {
        req.extensions_mut().insert(who);
    }
    let res = router.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let mut headers: Vec<(String, String)> = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "x-request-id")
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    headers.sort();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    Answer {
        status,
        headers,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session)
}

fn uri(path: &str) -> String {
    format!("/api/v1/namespaces/resolve?path={path}&prefix=true")
}

#[tokio::test]
async fn the_longest_visible_project_prefix_answers_with_the_remainder() {
    let fake = Arc::new(Fake {
        calls: AtomicUsize::new(0),
    });
    let r = router(&fake);
    let a = get(&r, &uri("acme/web/pull/3"), Some(session(ALICE))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(
        a.json,
        json!({
            "namespace_id": 101,
            "kind": "project",
            "canonical_path": "acme/web",
            "moved": false,
            "project_id": 1,
            "remainder": "pull/3",
        })
    );
    // A Guest-or-better member sees the private one; the whole path too.
    let b = get(
        &r,
        &uri("/acme/secret/blob/main/src/lib.rs"),
        Some(session(BOB)),
    )
    .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.json);
    assert_eq!(b.json["canonical_path"], "acme/secret");
    assert_eq!(b.json["remainder"], "blob/main/src/lib.rs");
    let exact = get(&r, &uri("acme/web"), Some(session(ALICE))).await;
    assert_eq!(exact.status, StatusCode::OK);
    assert!(exact.json.get("remainder").is_none());
    // Without prefix=true the full path is resolved exactly (as before).
    let plain = get(
        &r,
        "/api/v1/namespaces/resolve?path=acme/web/pull/3",
        Some(session(ALICE)),
    )
    .await;
    assert_eq!(plain.status, StatusCode::NOT_FOUND);
    let plain = get(
        &r,
        "/api/v1/namespaces/resolve?path=acme/web&prefix=false",
        Some(session(ALICE)),
    )
    .await;
    assert_eq!(plain.status, StatusCode::OK);
}

/// ADR-0015: no prefix answer reveals an invisible project, a group, a
/// tombstone or a project pending deletion — the same bytes as nothing at
/// all — and every candidate is resolved whatever exists.
#[tokio::test]
async fn nothing_but_a_visible_project_is_ever_revealed() {
    let fake = Arc::new(Fake {
        calls: AtomicUsize::new(0),
    });
    let r = router(&fake);
    let absent = get(&r, &uri("nosuch/thing/pull/3"), Some(session(ALICE))).await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    assert_eq!(absent.json["error_code"], "FG-REPO-0005");
    let calls_absent = fake.calls.swap(0, Ordering::SeqCst);
    for path in [
        "acme/secret/pull/3", // a private project of others
        "acme/zzzzzz/pull/3", // a group, nothing below
        "old/web/pull/3",     // a moved tombstone (only exact resolution follows it)
        "acme/gone/pull/3",   // pending deletion, for a member too (below)
    ] {
        let hidden = get(&r, &uri(path), Some(session(ALICE))).await;
        assert_eq!(hidden.status, absent.status, "{path}");
        assert_eq!(
            hidden.without_request_id(),
            absent.without_request_id(),
            "{path}"
        );
        assert_eq!(hidden.headers, absent.headers, "{path}");
        assert_eq!(
            fake.calls.swap(0, Ordering::SeqCst),
            calls_absent,
            "{path}: same statements"
        );
    }
    let gone = get(&r, &uri("acme/gone/pull/3"), Some(session(BOB))).await;
    assert_eq!(gone.without_request_id(), absent.without_request_id());
    assert_eq!(calls_absent, 4, "every leading prefix is resolved");
}

#[tokio::test]
async fn the_prefix_parameter_and_path_are_checked_and_the_scope_applies() {
    let fake = Arc::new(Fake {
        calls: AtomicUsize::new(0),
    });
    let r = router(&fake);
    for query in [
        "path=acme/web&prefix=yes",
        "path=acme/web&prefix=true&prefix=true",
        "prefix=true",
        "path=-x/web&prefix=true",
    ] {
        let bad = get(
            &r,
            &format!("/api/v1/namespaces/resolve?{query}"),
            Some(session(ALICE)),
        )
        .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(bad.json["error_code"], "FG-WEB-0006", "{query}");
    }
    let anon = get(&r, &uri("acme/web/pull/3"), None).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);
    let repo = Principal::authenticated(
        ALICE,
        false,
        true,
        Credential::Token(vec![Scope::ReadRepository]),
    );
    let refused = get(&r, &uri("acme/web/pull/3"), Some(repo)).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.json["error_code"], "FG-AUTH-0002");
}

#[tokio::test]
async fn prefix_resolution_is_rate_limited_per_caller() {
    let fake = Arc::new(Fake {
        calls: AtomicUsize::new(0),
    });
    let r = router(&fake);
    for _ in 0..forge_web::routes::namespaces::PREFIX_LIMIT {
        let ok = get(&r, &uri("acme/web/pull/3"), Some(session(ALICE))).await;
        assert_eq!(ok.status, StatusCode::OK);
    }
    fake.calls.store(0, Ordering::SeqCst);
    let limited = get(&r, &uri("acme/web/pull/3"), Some(session(ALICE))).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json["error_code"], "FG-WEB-0010");
    let retry = limited
        .headers
        .iter()
        .find(|(k, _)| k == "retry-after")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .expect("Retry-After");
    assert!((1..=60).contains(&retry), "{retry}");
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        0,
        "refused before any lookup"
    );
    // Another caller has its own window; exact resolution is not limited.
    let bob = get(&r, &uri("acme/web/pull/3"), Some(session(BOB))).await;
    assert_eq!(bob.status, StatusCode::OK);
    let exact = get(
        &r,
        "/api/v1/namespaces/resolve?path=acme/web",
        Some(session(ALICE)),
    )
    .await;
    assert_eq!(exact.status, StatusCode::OK);
}
