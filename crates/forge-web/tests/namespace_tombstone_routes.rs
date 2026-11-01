#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Route-level tombstone rows (ADR-0015 "名前の変更、移管、削除と墓標"; the
//! pure permission matrix cannot express them): the dispatcher and
//! `GET /api/v1/namespaces/resolve` against the real resolver
//! (`forge_db::resolve`, one statement per call) on the development
//! database (ADR-0011).
//!
//! Rows here (M1 phase 8 MR4):
//! - a `moved` tombstone redirects (`302`, `no-store`) only after
//!   `authorize()` allowed; an invisible target behind it is the absent 404;
//! - a `deleted` tombstone, and a `moved` one whose target was purged, are
//!   the absent 404 (`tombstone_deleted`), as is a POST through a `moved`
//!   one (`tombstone_post`, never redirected);
//! - `PathResolve` answers a moved path's current path to a caller who can
//!   see it, the absent 404 to anyone else;
//! - one resolver call per request, whatever the outcome.
//!
//! Rows still to come, with the routes they need: create after delete → 409
//! and A → B → A → 409 (the namespace write endpoints, phase 8b / D5);
//! `info/refs?service=git-receive-pack` on a tombstone → 404 and
//! `?service=git-upload-pack` → 302 (Smart HTTP, phase 9). D5 has no move
//! or delete command yet, so moves and tombstones are written as forge_app
//! with their audit rows (`FreshDb::commit_audited`), like forge-db's
//! resolver tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Action, Credential, Principal, ProjectCtx};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::web_path::RouteWord;
use forge_core::{LookupPath, ProjectAccess, ProjectId, RequestId, Resolution, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_web::registry::{Dispatched, RouteScope, ScopedRoute, Verb};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The database as the resolver, counting calls.
struct Counting {
    db: Db,
    calls: AtomicUsize,
}

impl forge_core::NamespaceResolver for Counting {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        forge_core::NamespaceResolver::resolve(&self.db, path, viewer)
    }
}

impl forge_core::ProjectAccessLoader for Counting {
    fn project_access<'a>(
        &'a self,
        project: ProjectId,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectAccess>> {
        forge_core::ProjectAccessLoader::project_access(&self.db, project, viewer)
    }
}

/// `raw` on projects (the product routes it with RepoRead in a later phase):
/// echoes the proof's project.
async fn content(_: AppState, request: Dispatched<ProjectCtx>) -> axum::Json<Value> {
    axum::Json(json!({ "project": request.proof().id().get() }))
}

fn app(resolver: &Arc<Counting>) -> axum::Router {
    let raw = ScopedRoute::Word(RouteWord::Raw);
    let registry = forge_web::route_registry()
        .scoped(
            RouteScope::Project,
            Verb::Get,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        )
        .scoped(
            RouteScope::Project,
            Verb::Post,
            raw,
            None,
            Action::RepoRead,
            "raw",
            content,
        );
    let lookups = Lookups {
        namespaces: Arc::clone(resolver) as Arc<dyn forge_core::NamespaceResolver>,
        projects: Arc::clone(resolver) as Arc<dyn forge_core::ProjectAccessLoader>,
    };
    let state = AppState::new("test", Vec::new(), Arc::new(NoTokens), lookups);
    forge_web::app::with_registry(registry, state, AppConfig::default()).expect("router")
}

/// No token store (principals are inserted directly).
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

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
}

/// alice owns the private project `alice/web`, renamed to `alice/site`
/// (a `moved` tombstone at `alice/web`); `alice/old` is a `deleted`
/// tombstone, `alice/purged` a `moved` one whose target is gone. bob is a
/// signed-in outsider.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let web = projects::create(
        &t.db,
        &ctx("req-project-web"),
        &NewProject {
            visibility: Visibility::Private,
            ..NewProject::new("web", alice.namespace_id, "Web", alice.user_id)
        },
    )
    .await
    .expect("project");
    let (pid, ns, uid) = (
        web.project_id.get(),
        web.namespace_id.get(),
        alice.user_id.get(),
    );
    t.commit_audited(
        ("project", pid),
        &[
            format!(
                "UPDATE forge.namespaces SET segment = 'site', full_path = 'alice/site' WHERE id = {ns}"
            ),
            format!(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, namespace_id, cause_type, \
                 cause_id) VALUES ('alice/web', 'moved', {ns}, 'project', {pid})"
            ),
        ],
    )
    .await;
    t.commit_audited(
        ("user", uid),
        &[
            format!(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
                 VALUES ('alice/old', 'deleted', 'user', {uid})"
            ),
            format!(
                "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
                 VALUES ('alice/purged', 'moved', 'user', {uid})"
            ),
        ],
    )
    .await;
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session)
}

struct Answer {
    status: StatusCode,
    location: Option<String>,
    cache_control: Option<String>,
    /// The body with the request id replaced (byte-identical denials).
    body: String,
    headers: Vec<(String, String)>,
}

async fn send(router: axum::Router, method: &str, uri: &str, who: UserId) -> Answer {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(session(who));
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let header = |name: &str| {
        res.headers()
            .get(name)
            .map(|v| v.to_str().expect("ascii").to_owned())
    };
    let location = header("location");
    let cache_control = header("cache-control");
    let id = header("x-request-id").unwrap_or_default();
    let headers = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "x-request-id")
        .map(|(k, v)| (k.to_string(), v.to_str().expect("ascii").to_owned()))
        .collect();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8");
    Answer {
        status,
        location,
        cache_control,
        body: if id.is_empty() {
            body
        } else {
            body.replace(&id, "<id>")
        },
        headers,
    }
}

fn counting(t: &FreshDb) -> Arc<Counting> {
    Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        calls: AtomicUsize::new(0),
    })
}

fn same(a: &Answer, b: &Answer) {
    assert_eq!(a.status, b.status);
    assert_eq!(a.headers, b.headers);
    assert_eq!(a.body, b.body);
}

#[tokio::test]
async fn a_moved_path_redirects_after_authorize_and_hides_an_invisible_target() {
    let w = world().await;
    let resolver = counting(&w.t);
    let owner = send(
        app(&resolver),
        "GET",
        "/alice/web/-/raw/main/README.md",
        w.alice,
    )
    .await;
    assert_eq!(owner.status, StatusCode::FOUND, "{}", owner.body);
    assert_eq!(
        owner.location.as_deref(),
        Some("/alice/site/-/raw/main/README.md")
    );
    assert_eq!(owner.cache_control.as_deref(), Some("no-store"));
    let live = send(
        app(&resolver),
        "GET",
        "/alice/site/-/raw/main/README.md",
        w.alice,
    )
    .await;
    assert_eq!(live.status, StatusCode::OK);
    assert_eq!(
        live.body,
        json!({ "project": w.web.project_id.get() }).to_string()
    );

    let outsider = send(
        app(&resolver),
        "GET",
        "/alice/web/-/raw/main/README.md",
        w.bob,
    )
    .await;
    let absent = send(
        app(&resolver),
        "GET",
        "/alice/nothing/-/raw/main/README.md",
        w.bob,
    )
    .await;
    assert_eq!(outsider.status, StatusCode::NOT_FOUND);
    assert!(
        outsider.location.is_none(),
        "never redirect to an invisible target"
    );
    assert!(!outsider.body.contains("site"));
    same(&outsider, &absent);
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        4,
        "one lookup per request"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn deleted_tombstones_and_purged_targets_are_the_absent_404() {
    let w = world().await;
    let resolver = counting(&w.t);
    let absent = send(app(&resolver), "GET", "/alice/nothing/-/raw/x", w.alice).await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    for path in ["/alice/old/-/raw/x", "/alice/purged/-/raw/x"] {
        let answer = send(app(&resolver), "GET", path, w.alice).await;
        assert!(answer.location.is_none(), "{path}");
        same(&answer, &absent);
    }
    // A POST through a moved tombstone is never redirected, even for the
    // owner.
    let post = send(app(&resolver), "POST", "/alice/web/-/raw/x", w.alice).await;
    let post_absent = send(app(&resolver), "POST", "/alice/nothing/-/raw/x", w.alice).await;
    assert!(post.location.is_none());
    same(&post, &post_absent);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 5);
    w.t.drop_now().await;
}

#[tokio::test]
async fn path_resolve_follows_a_moved_tombstone_only_for_a_caller_who_can_see_it() {
    let w = world().await;
    let resolver = counting(&w.t);
    let uri = |p: &str| format!("/api/v1/namespaces/resolve?path={p}");
    let owner = send(app(&resolver), "GET", &uri("alice/web"), w.alice).await;
    assert_eq!(owner.status, StatusCode::OK, "{}", owner.body);
    let body: Value = serde_json::from_str(&owner.body).expect("json");
    assert_eq!(body["canonical_path"], "alice/site");
    assert_eq!(body["moved"], true);
    assert_eq!(body["kind"], "project");
    assert_eq!(body["namespace_id"], w.web.namespace_id.get());

    let absent = send(app(&resolver), "GET", &uri("alice/nothing"), w.bob).await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
    for path in ["alice/web", "alice/site", "alice/old", "alice/purged"] {
        let answer = send(app(&resolver), "GET", &uri(path), w.bob).await;
        same(&answer, &absent);
    }
    // A personal namespace is visible to every signed-in user.
    let home = send(app(&resolver), "GET", &uri("alice"), w.bob).await;
    assert_eq!(home.status, StatusCode::OK);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 7);
    w.t.drop_now().await;
}
