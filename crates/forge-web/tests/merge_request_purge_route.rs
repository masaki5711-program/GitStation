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
    Principal::authenticated(
        user,
        admin,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    )
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Principal {
    Principal::authenticated(
        user,
        true,
        true,
        Credential::Token(scopes.to_vec()),
        forge_auth::AccountKind::Human,
    )
    .as_agent(agent)
}

/// The world of a test: an administrator, the project's owner `dev` (who
/// opens merge request 1 from `feature` into `main`), a real bare
/// repository with those branches, and the router with forge-review's
/// version refs.
struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    router: axum::Router,
    refs: VersionRefs,
    admin: UserId,
    dev: UserId,
    project: i64,
}

/// Plumbing on the bare repository (test setup only; synthetic content).
fn plumb(repo: &std::path::Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A new commit on `refs/heads/<branch>`.
fn push_commit(repo: &std::path::Path, branch: &str, content: &str) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    let commit = plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None);
    plumb(
        repo,
        &["update-ref", &format!("refs/heads/{branch}"), &commit],
        None,
    );
    commit
}

async fn world() -> World {
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
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    push_commit(&path, "main", "base\n");
    push_commit(&path, "feature", "first\n");
    let refs = VersionRefs::new(git, storage);
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(NoTokens),
        Lookups {
            namespaces: Arc::new(t.db.clone()),
            projects: Arc::new(t.db.clone()),
        },
    )
    .with_review(Review::new(t.db.clone(), Arc::new(AnyBranch)).with_version_refs(refs.clone()));
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
    World {
        t,
        _home: home,
        _root: root,
        path,
        router,
        refs,
        admin: admin.user_id,
        dev: dev.user_id,
        project: p,
    }
}

impl World {
    fn purge_uri(&self) -> String {
        format!(
            "/api/v1/projects/{}/merge-requests/1/version-refs/purge",
            self.project
        )
    }

    /// The version job for merge request 1, as the worker runs it.
    async fn refresh(&self, request: &str) {
        let id: i64 = self
            .t
            .json_rows("SELECT id FROM forge.merge_requests")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("merge request id");
        forge_review::refresh(
            &self.t.db,
            &self.refs,
            forge_review::RefreshRequest {
                merge_request: forge_core::ids::MergeRequestId::new(id),
                pushed_by: None,
            },
            &RequestId::parse(request).expect("id"),
        )
        .await
        .expect("refresh");
    }

    fn hidden(&self) -> String {
        plumb(
            &self.path,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/merge-requests/",
            ],
            None,
        )
    }
}

#[tokio::test]
async fn only_an_administrators_session_purges_version_refs() {
    let w = world().await;
    let (router, admin, dev) = (w.router.clone(), w.admin, w.dev);
    let uri = w.purge_uri();

    for who in [
        token(admin, &[Scope::Api, Scope::Admin], false),
        token(admin, &[Scope::Api, Scope::Admin], true),
    ] {
        let (status, body) = call(router.clone(), &uri, who, json!({})).await;
        assert_eq!(
            (status, body["error_code"].as_str()),
            (StatusCode::FORBIDDEN, Some("FG-AUTH-0002")),
            "{body}"
        );
    }
    let (status, body) = call(router.clone(), &uri, session(dev, false), json!({})).await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::FORBIDDEN, Some("FG-AUTH-0003")),
        "the author, not an administrator: {body}"
    );
    let (status, body) = call(
        router.clone(),
        &uri,
        session(admin, true),
        json!({"versions": [7]}),
    )
    .await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("FG-WEB-0006")),
        "{body}"
    );
    // No version recorded yet: nothing to purge, the refs checked anyway.
    let (status, body) = call(router.clone(), &uri, session(admin, true), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"iid": 1, "purged": [], "already_purged": []}));
    let audit =
        w.t.json_rows(
            "SELECT count(*) AS n FROM forge.audit_log \
             WHERE action = 'merge_request.version_refs_purged'",
        )
        .await;
    assert_eq!(audit[0]["n"], 0, "nothing marked, nothing audited");
    w.t.drop_now().await;
}

/// ADR-0024 §2: an open merge request's current head is never purged —
/// 409 FG-REVIEW-0002, nothing marked, no ref touched.
#[tokio::test]
async fn admin_purge_of_open_head_returns_409_fg_review_0002() {
    let w = world().await;
    w.refresh("req-v1").await;
    assert!(w.hidden().contains("refs/merge-requests/1/v1"));
    for body in [json!({}), json!({"versions": [1]})] {
        let (status, answer) = call(
            w.router.clone(),
            &w.purge_uri(),
            session(w.admin, true),
            body,
        )
        .await;
        assert_eq!(
            (status, answer["error_code"].as_str()),
            (StatusCode::CONFLICT, Some("FG-REVIEW-0002")),
            "{answer}"
        );
        assert!(answer["request_id"].is_string(), "{answer}");
    }
    let marked = w
        .t
        .json_rows("SELECT count(*) AS n FROM forge.merge_request_versions WHERE refs_purged_at IS NOT NULL")
        .await;
    assert_eq!(marked[0]["n"], 0);
    assert!(
        w.hidden().contains("refs/merge-requests/1/v1"),
        "no ref touched"
    );
    w.t.drop_now().await;
}

/// After the branch was rewritten, the old version is purged: 200 with
/// `purged`, its ref gone, one audit row; asked again: `already_purged`.
#[tokio::test]
async fn admin_purge_of_a_stale_version_returns_purged_and_audits() {
    let w = world().await;
    w.refresh("req-v1").await;
    push_commit(&w.path, "feature", "rewritten without the secret\n");
    w.refresh("req-v2").await;
    let (status, answer) = call(
        w.router.clone(),
        &w.purge_uri(),
        session(w.admin, true),
        json!({"versions": [1]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        json!({"iid": 1, "purged": [1], "already_purged": []})
    );
    let refs = w.hidden();
    assert!(!refs.contains("refs/merge-requests/1/v1"), "{refs}");
    assert!(refs.contains("refs/merge-requests/1/v2"), "{refs}");
    let audit =
        w.t.json_rows(
            "SELECT count(*) AS n FROM forge.audit_log \
             WHERE action = 'merge_request.version_refs_purged'",
        )
        .await;
    assert_eq!(audit[0]["n"], 1);
    let (status, again) = call(
        w.router.clone(),
        &w.purge_uri(),
        session(w.admin, true),
        json!({"versions": [1]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(
        again,
        json!({"iid": 1, "purged": [], "already_purged": [1]})
    );
    w.t.drop_now().await;
}
