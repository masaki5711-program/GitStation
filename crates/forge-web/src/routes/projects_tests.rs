//! `GET|HEAD /api/v1/projects/{id}` (M1 phase 8 MR5) with a scripted
//! loader that counts its calls: the view's shape, `HEAD` without a body,
//! byte-identical denials for absent / invisible / pending deletion seen by
//! a non-member, one 400 for every malformed id, exactly one loader call per request that
//! reaches the loader (none before authentication or for a malformed id),
//! token scopes, and the loader's failures.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use forge_auth::{Credential, Principal, Scope};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{
    NamespaceId, NamespacePath, ProjectAccess, ProjectId, ProjectRecord, RepositoryKind, Role,
    UserId, Visibility,
};
use forge_errors::{ErrorCode, ForgeError};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::app::{build, AppConfig, AppState, Lookups};
use crate::context::REQUEST_ID_HEADER;

const READABLE: i64 = 1; // private, the caller is Reporter
const HIDDEN: i64 = 2; // private, no role
const GUEST_ONLY: i64 = 3; // private, the caller is Guest
const DELETED_MEMBER: i64 = 4; // private, pending deletion, Developer
const DELETED_HIDDEN: i64 = 5; // private, pending deletion, no role
const INTERNAL: i64 = 6; // internal, no role
const WRONG_ID: i64 = 7; // the loader answers for another project

/// Answers from a script by id (everything else is absent) and counts
/// every call.
#[derive(Default)]
struct Script {
    answers: HashMap<i64, ProjectRecord>,
    calls: AtomicUsize,
    hang: bool,
    fail: Option<ErrorCode>,
}

impl Script {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl forge_core::NamespaceResolver for Script {
    fn resolve<'a>(
        &'a self,
        _: &'a forge_core::LookupPath,
        _: Option<UserId>,
    ) -> LookupFuture<'a, forge_core::Resolution> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "not_this_route"))
        })
    }
}

impl forge_core::ProjectAccessLoader for Script {
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        _viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answers.get(&project.get()).cloned();
        let (hang, fail) = (self.hang, self.fail);
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            match fail {
                Some(code) => Err(ForgeError::new(code).with_context("reason", "scripted")),
                None => Ok(answer),
            }
        })
    }
}

fn record(id: i64, visibility: Visibility, role: Option<Role>, deleted: bool) -> ProjectRecord {
    ProjectRecord {
        id: ProjectId::new(id),
        access: ProjectAccess {
            visibility,
            role,
            pending_deletion: deleted,
        },
        namespace_id: NamespaceId::new(id * 10),
        path: NamespacePath::parse(&format!("acme/p{id}")).expect("path"),
        display_name: format!("Project {id}"),
        description: "synthetic".to_owned(),
        kind: RepositoryKind::Spec,
        created_at: "2026-10-31T09:00:00Z".to_owned(),
    }
}

fn script() -> Script {
    let p = Visibility::Private;
    let mut answers = HashMap::new();
    for r in [
        record(READABLE, p, Some(Role::Reporter), false),
        record(HIDDEN, p, None, false),
        record(GUEST_ONLY, p, Some(Role::Guest), false),
        record(DELETED_MEMBER, p, Some(Role::Developer), true),
        record(DELETED_HIDDEN, p, None, true),
        record(INTERNAL, Visibility::Internal, None, false),
    ] {
        answers.insert(r.id.get(), r);
    }
    answers.insert(WRONG_ID, record(READABLE, p, Some(Role::Reporter), false));
    Script {
        answers,
        ..Script::default()
    }
}

fn app(script: &Arc<Script>) -> axum::Router {
    let lookups = Lookups {
        namespaces: Arc::clone(script) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(script) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let state = AppState::new("test", Vec::new(), crate::tests::no_tokens(), lookups)
        .with_lookup_timeout(Duration::from_millis(100));
    build(crate::app::registry(), state, AppConfig::default()).expect("router")
}

fn session(admin: bool, active: bool) -> Principal {
    Principal::authenticated(UserId::new(5), admin, active, Credential::Session)
}

fn member() -> Principal {
    session(false, true)
}

fn token(scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        UserId::new(5),
        false,
        true,
        Credential::Token(scopes.to_vec()),
    )
}

/// Status, headers (name, bytes) and body of an answer without its id.
type Comparable = (StatusCode, Vec<(String, Vec<u8>)>, Vec<u8>);

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }

    fn code(&self) -> Value {
        self.json()["error_code"].clone()
    }

    /// Status, headers and body without the request id (what two denials
    /// must share byte for byte).
    fn without_request_id(&self) -> Comparable {
        let id = self
            .headers
            .get(&REQUEST_ID_HEADER)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .unwrap_or_default();
        assert!(!id.is_empty(), "every answer names its request id");
        let headers = self
            .headers
            .iter()
            .filter(|(k, _)| *k != REQUEST_ID_HEADER)
            .map(|(k, v)| (k.as_str().to_owned(), v.as_bytes().to_vec()))
            .collect();
        let text = String::from_utf8(self.bytes.clone()).expect("utf-8 body");
        (self.status, headers, text.replace(&id, "<id>").into_bytes())
    }
}

async fn send(script: &Arc<Script>, method: &str, uri: &str, who: Option<Principal>) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(p) = who {
        req.extensions_mut().insert(p);
    }
    let res = app(script).oneshot(req).await.expect("infallible");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    Answer {
        status,
        headers,
        bytes,
    }
}

fn uri(id: impl std::fmt::Display) -> String {
    format!("/api/v1/projects/{id}")
}

#[tokio::test]
async fn a_reader_gets_the_project_view_and_it_is_never_shared_cached() {
    let script = Arc::new(script());
    let answer = send(&script, "GET", &uri(READABLE), Some(member())).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(
        answer.json(),
        json!({
            "id": READABLE,
            "namespace_id": READABLE * 10,
            "path": "acme/p1",
            "name": "p1",
            "display_name": "Project 1",
            "description": "synthetic",
            "visibility": "private",
            "kind": "spec",
            "created_at": "2026-10-31T09:00:00Z",
            "role": "reporter",
        })
    );
    assert_eq!(
        answer.headers.get("cache-control").expect("cache-control"),
        "private, no-store"
    );
    assert_eq!(
        answer.headers.get("vary").expect("vary"),
        "Authorization, Cookie"
    );
    assert_eq!(script.calls(), 1, "one loader call");
}

#[tokio::test]
async fn head_answers_the_same_headers_without_a_body() {
    let script = Arc::new(script());
    let get = send(&script, "GET", &uri(READABLE), Some(member())).await;
    let head = send(&script, "HEAD", &uri(READABLE), Some(member())).await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.bytes.is_empty(), "HEAD has no body");
    for name in ["cache-control", "vary", "content-type"] {
        assert_eq!(head.headers.get(name), get.headers.get(name), "{name}");
    }
    let denied = send(&script, "HEAD", &uri(HIDDEN), Some(member())).await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    assert!(denied.bytes.is_empty(), "a denied HEAD has no body either");
    assert_eq!(script.calls(), 3, "one loader call per request");
}

/// No such id, a project the caller cannot see and a project pending
/// deletion the caller cannot see: the same bytes but the request id. Each
/// costs exactly one loader call.
#[tokio::test]
async fn absent_invisible_and_malformed_ids_are_identical_denials() {
    let script = Arc::new(script());
    let mut answers = Vec::new();
    for (i, id) in [999, HIDDEN, DELETED_HIDDEN].into_iter().enumerate() {
        let answer = send(&script, "GET", &uri(id), Some(member())).await;
        assert_eq!(answer.code(), ErrorCode::NotFound.id(), "{id}");
        assert_eq!(script.calls(), i + 1, "{id}");
        answers.push(answer.without_request_id());
    }
    // An administrator gets the same 404 for an id that does not exist.
    let admin = send(&script, "GET", &uri(999), Some(session(true, true))).await;
    answers.push(admin.without_request_id());
    assert!(answers.iter().all(|a| a == &answers[0]));
}

/// An id that is not a plain positive decimal is FG-WEB-0006 (phase 8b
/// MR6), decided from the string without a lookup: byte-identical whether
/// its digits would name a readable (`01`), an invisible (`02`) or no
/// project (`0999`), for a member, an administrator and a wrong-scope token.
#[tokio::test]
async fn a_malformed_id_is_the_same_400_whatever_it_would_name() {
    let script = Arc::new(script());
    let mut answers = Vec::new();
    let repo_only = token(&[Scope::ReadRepository]);
    for raw in [
        "01",
        "02",
        "0999",
        "0",
        "+1",
        "-1",
        "%31",
        "1e3",
        "abc",
        "9223372036854775808",
    ] {
        for who in [member(), session(true, true), repo_only.clone()] {
            let answer = send(&script, "GET", &uri(raw), Some(who)).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{raw}");
            assert_eq!(answer.code(), ErrorCode::RequestInvalid.id(), "{raw}");
            answers.push(answer.without_request_id());
        }
    }
    assert!(answers.iter().all(|a| a == &answers[0]));
    assert_eq!(script.calls(), 0, "a malformed id is not looked up");
}

/// Pending deletion and the Guest role are decided by `authorize()` as the
/// matrix says: a member who can see the project gets FG-REPO-0006, a
/// Guest FG-AUTH-0003.
#[tokio::test]
async fn pending_deletion_and_the_guest_role_follow_the_matrix() {
    let script = Arc::new(script());
    let deleted = send(&script, "GET", &uri(DELETED_MEMBER), Some(member())).await;
    assert_eq!(deleted.code(), ErrorCode::PendingDeletion.id());
    let admin = send(
        &script,
        "GET",
        &uri(DELETED_MEMBER),
        Some(session(true, true)),
    )
    .await;
    assert_eq!(admin.code(), ErrorCode::PendingDeletion.id());
    let guest = send(&script, "GET", &uri(GUEST_ONLY), Some(member())).await;
    assert_eq!(guest.code(), ErrorCode::Forbidden.id());
    assert_eq!(script.calls(), 3);
}

/// An administrator and a signed-in reader of an internal project read it
/// without a role (`role: null`).
#[tokio::test]
async fn readers_without_a_role_see_role_null() {
    let script = Arc::new(script());
    let admin = send(&script, "GET", &uri(HIDDEN), Some(session(true, true))).await;
    assert_eq!(admin.status, StatusCode::OK);
    assert_eq!(admin.json()["role"], Value::Null);
    let internal = send(&script, "GET", &uri(INTERNAL), Some(member())).await;
    assert_eq!(internal.status, StatusCode::OK);
    assert_eq!(internal.json()["role"], Value::Null);
    assert_eq!(internal.json()["visibility"], "internal");
}

/// Authentication comes before the loader: anonymous and disabled callers
/// get the same answer for every id and cost no lookup.
#[tokio::test]
async fn authentication_comes_before_any_lookup() {
    let script = Arc::new(script());
    let anonymous: Vec<_> = [READABLE, 999]
        .into_iter()
        .map(|id| (id, None))
        .chain([
            (READABLE, Some(session(false, false))),
            (999, Some(session(true, false))),
        ])
        .collect();
    let mut seen = Vec::new();
    for (id, who) in anonymous {
        let disabled = who.is_some();
        let answer = send(&script, "GET", &uri(id), who).await;
        let want = if disabled {
            ErrorCode::AccountDisabled
        } else {
            ErrorCode::Unauthenticated
        };
        assert_eq!(answer.code(), want.id(), "{id}");
        seen.push((disabled, answer.without_request_id()));
    }
    assert_eq!(seen[0].1, seen[1].1, "anonymous: the same for every id");
    assert_eq!(seen[2].1, seen[3].1, "disabled: the same for every id");
    assert_eq!(script.calls(), 0);
}

/// `ProjectRead` needs `read_api` (or `api`) on a token; a token without it
/// is refused by `authorize()` after visibility (an absent id stays the 404).
#[tokio::test]
async fn tokens_need_the_read_api_scope() {
    let script = Arc::new(script());
    for scopes in [&[Scope::ReadApi][..], &[Scope::Api]] {
        let answer = send(&script, "GET", &uri(READABLE), Some(token(scopes))).await;
        assert_eq!(answer.status, StatusCode::OK, "{scopes:?}");
    }
    let repo_only = token(&[Scope::ReadRepository]);
    let visible = send(&script, "GET", &uri(READABLE), Some(repo_only.clone())).await;
    assert_eq!(visible.code(), ErrorCode::InsufficientScope.id());
    let absent = send(&script, "GET", &uri(999), Some(repo_only.clone())).await;
    let hidden = send(&script, "GET", &uri(HIDDEN), Some(repo_only)).await;
    assert_eq!(absent.code(), ErrorCode::NotFound.id());
    assert_eq!(absent.without_request_id(), hidden.without_request_id());
    assert_eq!(script.calls(), 5);
}

#[tokio::test]
async fn a_loader_failure_is_its_own_code_and_a_hang_is_bounded() {
    let failing = Arc::new(Script {
        fail: Some(ErrorCode::DbUnavailable),
        ..script()
    });
    let answer = send(&failing, "GET", &uri(READABLE), Some(member())).await;
    assert_eq!(answer.code(), ErrorCode::DbUnavailable.id());
    let hanging = Arc::new(Script {
        hang: true,
        ..script()
    });
    let answer = send(&hanging, "GET", &uri(READABLE), Some(member())).await;
    assert_eq!(answer.code(), ErrorCode::RequestTimeout.id());
    assert_eq!(hanging.calls(), 1);
}

/// A loader that answers for another project is never decided on.
#[tokio::test]
async fn a_row_for_another_project_is_an_internal_error() {
    let script = Arc::new(script());
    let answer = send(&script, "GET", &uri(WRONG_ID), Some(member())).await;
    assert_eq!(answer.code(), ErrorCode::Internal.id());
}

#[test]
fn the_id_is_read_from_the_raw_path_by_the_permalink_rule() {
    use super::id_param;
    assert_eq!(id_param("/api/v1/projects/42"), Some(ProjectId::new(42)));
    for raw in [
        "/api/v1/projects/",
        "/api/v1/projects/042",
        "/api/v1/projects/%34%32",
        "/api/v1/projects/42/",
        "/api/v1/project/42",
    ] {
        assert_eq!(id_param(raw), None, "{raw}");
    }
}
