#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! A person's notes (plan 20 N1, ADR-0040 §6) through the product router,
//! against the development database and a real storage root:
//! `POST /api/v1/user/notes` creates the private project `<user>/notes`,
//! provisions its repository, writes the first commit and points at it —
//! the pointer last, so a failure leaves none; `GET /api/v1/user/notes`
//! reads it. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{CursorKeys, ProjectId, RepoId, RequestId, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::Review;
use forge_web::{AppConfig, AppState, GitHttpLimits, Lists, Lookups, SmartHttp};
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

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

fn token(user: UserId, scopes: &[Scope]) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(scopes.to_vec()),
        forge_auth::AccountKind::Human,
    )
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Principal,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
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

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

fn git_out(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

const NOTES: &str = "/api/v1/user/notes";

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    storage: StorageRoot,
    router: axum::Router,
    /// Olga: the notes' owner.
    olga: Principal,
    olga_id: UserId,
    olga_ns: forge_core::NamespaceId,
    /// Dave: another person.
    dave: Principal,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ids = Vec::new();
    for name in ["olga", "dave"] {
        let u = users::create(
            &t.db,
            &ctx(&format!("req-{name}")),
            &NewUser::human(name, name, &format!("{name}@example.test")),
        )
        .await
        .expect("user");
        ids.push(u);
    }
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default())
        .with_push(Arc::new(t.db.clone()), Arc::new(t.db.clone()));
    let browser = smart.browse.clone();
    let keys = Arc::new(CursorKeys::parse(&format!("3 {}\n", "6b".repeat(32))).expect("keys"));
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_smart_http(smart)
    .with_hosting(forge_hosting::Hosting::new(t.db.clone()))
    .with_review(Review::new(t.db.clone(), Arc::new(browser)))
    .with_lists(
        Lists::new(Arc::new(t.db.clone()), Arc::new(t.db.clone()), keys)
            .with_search(Arc::new(t.db.clone())),
    );
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    World {
        olga: token(ids[0].user_id, &[Scope::Api]),
        olga_id: ids[0].user_id,
        olga_ns: ids[0].namespace_id,
        dave: token(ids[1].user_id, &[Scope::Api]),
        t,
        _home: home,
        _root: root,
        storage,
        router,
    }
}

impl World {
    async fn call(&self, method: &str, uri: &str, who: &Principal) -> (StatusCode, Value) {
        let body = (method != "GET" && method != "DELETE").then(|| json!({}));
        send(self.router.clone(), method, uri, who.clone(), body).await
    }

    async fn pointers(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge.user_notes")
            .fetch_one(self.t.db.pool().executor())
            .await
            .expect("count")
    }

    async fn repo_of(&self, project: i64) -> RepoId {
        let id: i64 = sqlx::query_scalar("SELECT id FROM forge.repositories WHERE project_id = $1")
            .bind(project)
            .fetch_one(self.t.db.pool().executor())
            .await
            .expect("repo");
        RepoId::new(id)
    }

    async fn notes_id_by_path(&self) -> Option<i64> {
        sqlx::query_scalar(
            "SELECT owner_id FROM forge.namespaces WHERE kind = 'project' \
             AND parent_id = $1 AND segment = 'notes'",
        )
        .bind(self.olga_ns.get())
        .fetch_optional(self.t.db.pool().executor())
        .await
        .expect("project")
    }
}

#[tokio::test]
async fn notes_are_created_private_seeded_and_pointed_at() {
    let w = world().await;
    // No notes yet: the invisible project's 404.
    let (status, body) = w.call("GET", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "FG-REPO-0005");
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["path"], "olga/notes");
    assert_eq!(body["owner"], "olga");
    assert_eq!(body["default_branch"], "main");
    assert_eq!(body["replaced_project_id"], Value::Null);
    let id = body["project_id"].as_i64().expect("id");
    // Private.
    let visibility: String =
        sqlx::query_scalar("SELECT visibility FROM forge.projects WHERE id = $1")
            .bind(id)
            .fetch_one(w.t.db.pool().executor())
            .await
            .expect("visibility");
    assert_eq!(visibility, "private");
    // The first commit, on main, with the note type and the readme.
    let repo = w.storage.existing(w.repo_of(id).await).expect("repo");
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/heads/main"]),
        body["commit"]
    );
    let files = git_out(&repo, &["ls-tree", "-r", "--name-only", "refs/heads/main"]);
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        vec![".forge/doctypes/note.yml", "README.md"]
    );
    let doctype = git_out(&repo, &["show", "refs/heads/main:.forge/doctypes/note.yml"]);
    assert!(doctype.contains("owner_kind: user"), "{doctype}");
    assert_eq!(w.pointers().await, 1);
    // Read back.
    let (status, body) = w.call("GET", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], id);
    assert_eq!(body["path"], "olga/notes");
    assert_eq!(body["owner"], "olga");
}

#[tokio::test]
async fn notes_create_twice_is_refused() {
    let w = world().await;
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0006");
    assert_eq!(w.pointers().await, 1);
}

#[tokio::test]
async fn notes_name_taken_is_refused_and_writes_nothing() {
    let w = world().await;
    projects::create(
        &w.t.db,
        &ctx("req-taken"),
        &NewProject::new("notes", w.olga_ns, "Taken", w.olga_id),
    )
    .await
    .expect("taken");
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0006");
    assert_eq!(w.pointers().await, 0);
}

/// Plan 20 N1: another person reads nothing of the notes — not the
/// project, not its files, not the pointer.
#[tokio::test]
async fn other_user_cannot_read_notes() {
    let w = world().await;
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["project_id"].as_i64().expect("id");
    for uri in [
        format!("/api/v1/projects/{id}"),
        format!("/api/v1/projects/{id}/repository/tree?ref=main"),
        format!("/api/v1/projects/{id}/repository/blob?ref=main&path=README.md"),
        format!("/api/v1/projects/{id}/documents"),
    ] {
        let (status, body) = w.call("GET", &uri, &w.dave).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert_eq!(code(&body), "FG-REPO-0005", "{uri}");
    }
    // Dave's own pointer is his (none): never Olga's.
    let (status, body) = w.call("GET", NOTES, &w.dave).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "FG-REPO-0005");
    // Olga reads her own.
    let (status, body) = w
        .call(
            "GET",
            &format!("/api/v1/projects/{id}/repository/blob?ref=main&path=README.md"),
            &w.olga,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Plan 20 N1 with K4 on main: the notes project is indexed like any
/// project, and the search shows it to its owner only.
#[tokio::test]
async fn notes_not_in_others_search() {
    let w = world().await;
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["project_id"].as_i64().expect("id");
    forge_db::search::reindex(
        &w.t.db,
        &RequestId::parse("req-reindex").expect("id"),
        Some(ProjectId::new(id)),
    )
    .await
    .expect("reindex");
    let hits = |body: &Value| -> Vec<i64> {
        body["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|h| h["project_id"].as_i64())
            .collect()
    };
    let (status, body) = w.call("GET", "/api/v1/search?q=notes", &w.olga).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        hits(&body).contains(&id),
        "the owner finds her notes: {body}"
    );
    let (status, body) = w.call("GET", "/api/v1/search?q=notes", &w.dave).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!hits(&body).contains(&id), "{body}");
    assert!(!body.to_string().contains("olga/notes"), "{body}");
    let (status, body) = w
        .call(
            "GET",
            &format!("/api/v1/search?q=notes&project={id}"),
            &w.dave,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(hits(&body).is_empty(), "{body}");
}

/// Plan 20 N1: a pointer whose project was moved out of `<user>/notes`
/// (or deleted) is FG-KNOW-0005; creating the notes again replaces it.
#[tokio::test]
async fn moved_notes_project_is_know_f() {
    let w = world().await;
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let old = body["project_id"].as_i64().expect("id");
    // Rename it within her namespace (to share it, or by mistake).
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/projects/{old}/transfer"),
        w.olga.clone(),
        Some(json!({"namespace_id": w.olga_ns.get(), "path": "old-notes"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w.call("GET", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0005");
    // Create again: new <olga>/notes, the stale pointer replaced.
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["replaced_project_id"], old);
    let new = body["project_id"].as_i64().expect("id");
    assert_ne!(new, old);
    assert_eq!(w.pointers().await, 1);
    let (status, body) = w.call("GET", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["project_id"], new);
    // Deleted (pending deletion): stale too.
    let (status, body) = w
        .call("DELETE", &format!("/api/v1/projects/{new}"), &w.olga)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w.call("GET", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0005");
    // The stale pointer is told only after the decision: a token without
    // read_api is refused first.
    let narrow = token(w.olga_id, &[Scope::WriteRepository]);
    let (status, body) = w.call("GET", NOTES, &narrow).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_ne!(code(&body), "FG-KNOW-0005");
}

/// A service account's token is refused (people only); an agent's token
/// too; nothing is created.
#[tokio::test]
async fn notes_create_refused_for_agents_and_service_accounts() {
    let w = world().await;
    let agent = token(w.olga_id, &[Scope::Api]).as_agent(true);
    let (status, body) = w.call("POST", NOTES, &agent).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code(&body), "FG-AUTH-0003");
    let (status, body) = send(
        w.router.clone(),
        "POST",
        NOTES,
        Principal::anonymous(),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let read_only = token(w.olga_id, &[Scope::ReadApi]);
    let (status, body) = w.call("POST", NOTES, &read_only).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(w.pointers().await, 0);
    assert_eq!(w.notes_id_by_path().await, None);
}

/// A failure after the project was created writes no pointer
/// (FG-KNOW-0007); the half-made project stays for the person to delete.
#[tokio::test]
async fn notes_provision_failure_writes_no_pointer() {
    let w = world().await;
    // The next repository's directory is taken by something forge did not
    // make: provisioning refuses to adopt it.
    let max: i64 = sqlx::query_scalar("SELECT COALESCE(max(id), 0) FROM forge.repositories")
        .fetch_one(w.t.db.pool().executor())
        .await
        .expect("max");
    let occupied = w.storage.path_for(RepoId::new(max + 1));
    std::fs::create_dir_all(&occupied).expect("occupy");
    std::fs::write(occupied.join("foreign.txt"), "not forge's").expect("occupy");
    let (status, body) = w.call("POST", NOTES, &w.olga).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0007");
    assert_eq!(w.pointers().await, 0);
    let half = w.notes_id_by_path().await.expect("half-made project");
    assert_eq!(w.repo_of(half).await, RepoId::new(max + 1));
}
