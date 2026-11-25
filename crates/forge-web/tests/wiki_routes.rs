#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! A project's wiki (plan 20 W2, ADR-0040 §5) through the product router,
//! against the development database and a real storage root:
//! `POST /api/v1/projects/{id}/wiki` creates the wiki project, provisions
//! its repository, writes the first commit, protects `main` and links it —
//! the link last, so a failure leaves none; `DELETE …/wiki` unlinks.
//! Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{ProjectId, RepoId, RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::roles::load_project_roles;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::Review;
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
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

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    storage: StorageRoot,
    router: axum::Router,
    /// Olga: the owner project's Owner (her personal namespace).
    olga: Principal,
    olga_id: UserId,
    /// Dave: a Developer of the owner project only.
    dave: Principal,
    dave_id: UserId,
    project: ProjectId,
    repo: RepoId,
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
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", ids[0].namespace_id, "Web", ids[0].user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        web.project_id,
        ids[1].user_id,
        Role::Developer,
    )
    .await
    .expect("member");
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default())
        .with_push(Arc::new(t.db.clone()), Arc::new(t.db.clone()));
    let browser = smart.browse.clone();
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
    .with_review(Review::new(t.db.clone(), Arc::new(browser)));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    World {
        olga: token(ids[0].user_id, &[Scope::Api]),
        olga_id: ids[0].user_id,
        dave: token(ids[1].user_id, &[Scope::Api]),
        dave_id: ids[1].user_id,
        t,
        _home: home,
        _root: root,
        storage,
        router,
        project: web.project_id,
        repo: web.repository_id,
    }
}

impl World {
    fn uri(&self) -> String {
        format!("/api/v1/projects/{}/wiki", self.project.get())
    }

    async fn create(&self, who: &Principal, body: Value) -> (StatusCode, Value) {
        send(
            self.router.clone(),
            "POST",
            &self.uri(),
            who.clone(),
            Some(body),
        )
        .await
    }

    async fn unlink(&self, who: &Principal) -> (StatusCode, Value) {
        send(
            self.router.clone(),
            "DELETE",
            &self.uri(),
            who.clone(),
            None,
        )
        .await
    }

    async fn links(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forge.project_wikis")
            .fetch_one(self.t.db.pool().executor())
            .await
            .expect("count")
    }

    async fn role(&self, project: ProjectId, who: UserId) -> Option<Role> {
        load_project_roles(&self.t.db, project, who)
            .await
            .expect("roles")
            .native
    }

    async fn project_by_path(&self, segment: &str) -> Option<(i64, bool)> {
        sqlx::query_as(
            "SELECT p.id, p.deleted_at IS NOT NULL FROM forge.projects p \
             JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
             WHERE n.segment = $1",
        )
        .bind(segment)
        .fetch_optional(self.t.db.pool().executor())
        .await
        .expect("project")
    }

    async fn repo_of(&self, project: i64) -> RepoId {
        let id: i64 = sqlx::query_scalar("SELECT id FROM forge.repositories WHERE project_id = $1")
            .bind(project)
            .fetch_one(self.t.db.pool().executor())
            .await
            .expect("repo");
        RepoId::new(id)
    }
}

#[tokio::test]
async fn wiki_is_created_seeded_protected_and_linked() {
    let w = world().await;
    let (status, body) = w.create(&w.olga, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["wiki_path"], "olga/web-wiki");
    assert_eq!(body["display_name"], "Web Wiki");
    assert_eq!(body["first_page"], "WIKI-0001.md");
    let wiki = ProjectId::new(body["wiki_project_id"].as_i64().expect("id"));
    // The first commit, on main, with the two files.
    let repo = w
        .storage
        .existing(w.repo_of(wiki.get()).await)
        .expect("repo");
    assert_eq!(
        git_out(&repo, &["rev-parse", "refs/heads/main"]),
        body["commit"]
    );
    let files = git_out(&repo, &["ls-tree", "-r", "--name-only", "refs/heads/main"]);
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        vec![".forge/doctypes/wiki.page.yml", "WIKI-0001.md"]
    );
    // main is protected for Developers (no force push, no deletion).
    let rule: Option<String> = sqlx::query_scalar(
        "SELECT push_min_role FROM forge.protected_refs WHERE project_id = $1 \
         AND pattern = 'refs/heads/main'",
    )
    .bind(wiki.get())
    .fetch_optional(w.t.db.pool().executor())
    .await
    .expect("rule");
    assert_eq!(rule.as_deref(), Some("developer"));
    // Linked: Dave inherits Developer on the wiki; audited on the owner.
    assert_eq!(w.links().await, 1);
    assert_eq!(w.role(wiki, w.dave_id).await, Some(Role::Developer));
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE action = 'project.wiki_linked' \
         AND resource_id = $1",
    )
    .bind(w.project.get())
    .fetch_one(w.t.db.pool().executor())
    .await
    .expect("audit");
    assert_eq!(audited, 1);
    // A second wiki is refused.
    let (status, body) = w.create(&w.olga, json!({"path": "web-wiki-2"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0003");
}

#[tokio::test]
async fn committed_seed_passes_doc_lint() {
    let w = world().await;
    let (status, body) = w
        .create(&w.olga, json!({"display_name": "設備: \"手順\""}))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let wiki = body["wiki_project_id"].as_i64().expect("id");
    let repo = w.storage.existing(w.repo_of(wiki).await).expect("repo");
    let doctype = git_out(
        &repo,
        &["show", "refs/heads/main:.forge/doctypes/wiki.page.yml"],
    );
    let page = git_out(&repo, &["show", "refs/heads/main:WIKI-0001.md"]);
    let reports = forge_doc::lint::lint(
        &[forge_doc::lint::Source {
            path: ".forge/doctypes/wiki.page.yml",
            bytes: doctype.as_bytes(),
        }],
        &[forge_doc::lint::Source {
            path: "WIKI-0001.md",
            bytes: page.as_bytes(),
        }],
    );
    assert!(reports.is_empty(), "{reports:?}");
}

#[tokio::test]
async fn name_collision_is_know_d() {
    let w = world().await;
    let olga_ns: i64 =
        sqlx::query_scalar("SELECT id FROM forge.namespaces WHERE kind = 'user' AND owner_id = $1")
            .bind(w.olga_id.get())
            .fetch_one(w.t.db.pool().executor())
            .await
            .expect("ns");
    projects::create(
        &w.t.db,
        &ctx("req-taken"),
        &NewProject::new(
            "web-wiki",
            forge_core::NamespaceId::new(olga_ns),
            "Taken",
            w.olga_id,
        ),
    )
    .await
    .expect("taken");
    let (status, body) = w.create(&w.olga, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0003");
    assert_eq!(w.links().await, 0);
    // Another path works.
    let (status, body) = w.create(&w.olga, json!({"path": "handbook"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["wiki_path"], "olga/handbook");
}

#[tokio::test]
async fn provision_failure_leaves_no_link() {
    let w = world().await;
    // The next repository's directory is taken by something forge did not
    // make: provisioning refuses to adopt it (FG-REPO-0004).
    let next = RepoId::new(w.repo.get() + 1);
    let occupied = w.storage.path_for(next);
    std::fs::create_dir_all(&occupied).expect("occupy");
    std::fs::write(occupied.join("foreign.txt"), "not forge's").expect("occupy");
    let (status, body) = w.create(&w.olga, json!({})).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0004");
    // No link: nobody inherits a role on the half-made project.
    assert_eq!(w.links().await, 0);
    let (wiki, _) = w
        .project_by_path("web-wiki")
        .await
        .expect("half-made project");
    assert_eq!(w.repo_of(wiki).await, next, "the predicted repository");
    assert_eq!(w.role(ProjectId::new(wiki), w.dave_id).await, None);
}

#[tokio::test]
async fn unlink_keeps_wiki_project() {
    let w = world().await;
    let (status, body) = w.create(&w.olga, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let wiki = ProjectId::new(body["wiki_project_id"].as_i64().expect("id"));
    // A Developer may not unlink.
    let (status, body) = w.unlink(&w.dave).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = w.unlink(&w.olga).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["wiki_project_id"], wiki.get());
    assert_eq!(w.links().await, 0);
    // The wiki project and its history stay; Dave's inherited role is gone.
    assert_eq!(
        w.project_by_path("web-wiki").await,
        Some((wiki.get(), false))
    );
    let repo = w
        .storage
        .existing(w.repo_of(wiki.get()).await)
        .expect("repo");
    assert!(!git_out(&repo, &["rev-parse", "refs/heads/main"]).is_empty());
    assert_eq!(w.role(wiki, w.dave_id).await, None);
    let (status, body) = w.unlink(&w.olga).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "FG-KNOW-0003");
}

#[tokio::test]
async fn owner_project_deleted_keeps_wiki() {
    let w = world().await;
    let (status, body) = w.create(&w.olga, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let wiki = body["wiki_project_id"].as_i64().expect("id");
    // The owner enters its grace period: the wiki and the link stay.
    let (status, body) = send(
        w.router.clone(),
        "DELETE",
        &format!("/api/v1/projects/{}", w.project.get()),
        w.olga.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(w.project_by_path("web-wiki").await, Some((wiki, false)));
    assert_eq!(w.links().await, 1);
    // The owner's purge removes the link only.
    w.t.commit_audited(
        ("project", w.project.get()),
        &[format!(
            "UPDATE forge.projects SET deleted_at = now() - interval '2 days', \
             purge_after = now() - interval '1 second' WHERE id = {}",
            w.project.get()
        )],
    )
    .await;
    let _started = projects::purge::start(&w.t.db, &ctx("req-start"), w.project)
        .await
        .expect("start");
    let finished = projects::purge::finish(&w.t.db, &ctx("req-finish"), w.project, true)
        .await
        .expect("finish");
    assert!(
        matches!(finished, projects::purge::Finished::Purged(_)),
        "{finished:?}"
    );
    assert_eq!(w.links().await, 0);
    assert_eq!(w.project_by_path("web-wiki").await, Some((wiki, false)));
}

#[tokio::test]
async fn developer_and_agent_cannot_create_a_wiki() {
    let w = world().await;
    let (status, body) = w.create(&w.dave, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let agent = token(w.olga_id, &[Scope::Api]).as_agent(true);
    let (status, body) = w.create(&agent, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(w.project_by_path("web-wiki").await, None, "nothing created");
}

/// A Maintainer of the owner project who may not create projects in its
/// namespace (Olga's personal namespace) is refused before anything is
/// written: `ProjectCreate` is decided with the caller's own principal.
#[tokio::test]
async fn maintainer_without_project_create_gets_nothing() {
    let w = world().await;
    projects::remove_member(
        &w.t.db,
        &ctx("req-rm"),
        w.project,
        w.dave_id,
        Role::Developer,
    )
    .await
    .expect("remove");
    projects::add_member(
        &w.t.db,
        &ctx("req-mt"),
        w.project,
        w.dave_id,
        Role::Maintainer,
    )
    .await
    .expect("maintainer");
    let (status, body) = w.create(&w.dave, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(w.project_by_path("web-wiki").await, None, "nothing created");
    assert_eq!(w.links().await, 0);
}
