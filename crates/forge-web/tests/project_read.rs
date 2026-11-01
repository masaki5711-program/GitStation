#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `GET|HEAD /api/v1/projects/{id}` (M1 phase 8 MR5) against the real
//! loader (`forge_db::resolve::load_project`, one statement per call) on the
//! development database (ADR-0011), through the product router:
//!
//! - the owner reads the view (current path, role `owner`); a signed-in
//!   outsider gets the same bytes for the private project as for an id that
//!   does not exist; an internal project is readable by the outsider with
//!   `role: null`;
//! - `GET /api/v1/namespaces/resolve` gives the owner the `project_id`
//!   that the project route takes (and the outsider the 404, without it);
//! - pending deletion is `FG-REPO-0006` to the owner, the absent 404 to the
//!   outsider;
//! - one loader call per request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{LookupPath, ProjectId, ProjectRecord, RequestId, Resolution, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_errors::ErrorCode;
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// The database as both lookups, counting the project loader's calls.
struct Counting {
    db: Db,
    loads: AtomicUsize,
}

impl forge_core::NamespaceResolver for Counting {
    fn resolve<'a>(
        &'a self,
        path: &'a LookupPath,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Resolution> {
        forge_core::NamespaceResolver::resolve(&self.db, path, viewer)
    }
}

impl forge_core::ProjectAccessLoader for Counting {
    fn load_project<'a>(
        &'a self,
        project: ProjectId,
        viewer: Option<UserId>,
    ) -> LookupFuture<'a, Option<ProjectRecord>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        forge_core::ProjectAccessLoader::load_project(&self.db, project, viewer)
    }
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

fn app(lookups: &Arc<Counting>) -> axum::Router {
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::clone(lookups) as Arc<dyn forge_core::NamespaceResolver>,
            projects: Arc::clone(lookups) as Arc<dyn forge_core::ProjectAccessLoader>,
        },
    );
    forge_web::app(state, AppConfig::default()).expect("router")
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

async fn project(
    t: &FreshDb,
    owner: &users::Created,
    segment: &str,
    v: Visibility,
) -> CreatedProject {
    projects::create(
        &t.db,
        &ctx(&format!("req-project-{segment}")),
        &NewProject {
            visibility: v,
            description: "synthetic".to_owned(),
            ..NewProject::new(segment, owner.namespace_id, "Web", owner.user_id)
        },
    )
    .await
    .expect("project")
}

/// Status, the body as JSON, and the body / headers with the request id
/// replaced (for byte-identical denials).
struct Answer {
    status: StatusCode,
    json: Value,
    comparable: (StatusCode, Vec<(String, String)>, String),
}

async fn get(router: axum::Router, uri: &str, who: UserId) -> Answer {
    let mut req = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    req.extensions_mut().insert(Principal::authenticated(
        who,
        false,
        true,
        Credential::Session,
    ));
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let id = res
        .headers()
        .get("x-request-id")
        .map(|v| v.to_str().expect("ascii").to_owned())
        .unwrap_or_default();
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
        json: serde_json::from_str(&body).unwrap_or(Value::Null),
        comparable: (status, headers, body.replace(&id, "<id>")),
    }
}

#[tokio::test]
async fn the_project_route_reads_through_the_real_loader() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let web = project(&t, &alice, "web", Visibility::Private).await;
    let open = project(&t, &alice, "open", Visibility::Internal).await;
    let lookups = Arc::new(Counting {
        db: t.db.clone(), // clone: the pool handle is shared
        loads: AtomicUsize::new(0),
    });
    let uri = |p: ProjectId| format!("/api/v1/projects/{}", p.get());

    // resolve → project_id → the project route, for the owner.
    let resolved = get(
        app(&lookups),
        "/api/v1/namespaces/resolve?path=Alice/Web",
        alice.user_id,
    )
    .await;
    assert_eq!(resolved.status, StatusCode::OK);
    assert_eq!(resolved.json["project_id"], json!(web.project_id.get()));
    let mine = get(app(&lookups), &uri(web.project_id), alice.user_id).await;
    assert_eq!(mine.status, StatusCode::OK);
    let view = &mine.json;
    assert_eq!(view["id"], json!(web.project_id.get()));
    assert_eq!(view["namespace_id"], json!(web.namespace_id.get()));
    assert_eq!(view["path"], "alice/web");
    assert_eq!(view["name"], "web");
    assert_eq!(view["display_name"], "Web");
    assert_eq!(view["visibility"], "private");
    assert!(
        view.get("kind").is_none(),
        "the project view no longer carries a kind (ADR-0023)"
    );
    assert_eq!(view["role"], "owner");
    assert!(view["created_at"]
        .as_str()
        .is_some_and(|s| s.ends_with('Z')));

    // The outsider: the private project and a missing id are the same bytes;
    // resolve gives no project_id either.
    let hidden = get(app(&lookups), &uri(web.project_id), bob.user_id).await;
    let absent = get(app(&lookups), &uri(ProjectId::new(9_999)), bob.user_id).await;
    assert_eq!(hidden.json["error_code"], ErrorCode::NotFound.id());
    assert_eq!(hidden.comparable, absent.comparable);
    let resolved = get(
        app(&lookups),
        "/api/v1/namespaces/resolve?path=alice/web",
        bob.user_id,
    )
    .await;
    assert_eq!(resolved.status, StatusCode::NOT_FOUND);
    assert!(resolved.json.get("project_id").is_none());
    let internal = get(app(&lookups), &uri(open.project_id), bob.user_id).await;
    assert_eq!(internal.status, StatusCode::OK);
    assert_eq!(internal.json["role"], Value::Null);
    assert_eq!(
        lookups.loads.load(Ordering::SeqCst),
        4,
        "one load per request"
    );

    // Pending deletion: FG-REPO-0006 to the owner, the absent 404 to bob.
    t.commit_audited(
        ("project", web.project_id.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now(), \
             purge_after = now() + interval '7 days' WHERE id = {}",
            web.project_id.get()
        )],
    )
    .await;
    let deleted = get(app(&lookups), &uri(web.project_id), alice.user_id).await;
    assert_eq!(deleted.json["error_code"], ErrorCode::PendingDeletion.id());
    let deleted_hidden = get(app(&lookups), &uri(web.project_id), bob.user_id).await;
    assert_eq!(deleted_hidden.comparable, absent.comparable);
    t.drop_now().await;
}
