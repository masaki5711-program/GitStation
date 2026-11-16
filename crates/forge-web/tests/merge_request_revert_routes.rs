#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The one-step revert route of M4 C10 (plan 04 M-4, ADR-0024 Amendment
//! C10) through the product router, against the development database and
//! a real bare repository: a merge request merged by the real merge job is
//! reverted with `POST …/{iid}/revert` (201, a merge request from
//! `revert/mr-<iid>`); the status's `viewer.can_revert` says who may; who
//! may not is refused before anything is written.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::{RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{Mergeability, Merger, Review, VersionRefs};
use forge_web::SessionCredential;
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

/// Who calls, and the web session the request came with (the merge stores
/// the credential's kind and id; the job reads it again).
#[derive(Clone)]
struct Caller {
    principal: Principal,
    session: Option<i64>,
}

async fn send(
    router: axum::Router,
    method: &str,
    uri: &str,
    who: Caller,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let mut req = req
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .expect("request");
    req.extensions_mut().insert(who.principal);
    if let Some(id) = who.session {
        req.extensions_mut().insert(SessionCredential {
            session_id: id,
            public_id: "00000000000000a1".to_owned(),
            csrf_token: "c5rf".repeat(16),
            idle_expires_at: "2099-01-01T00:00:00Z".to_owned(),
            absolute_expires_at: "2099-01-01T00:00:00Z".to_owned(),
        });
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn token(user: UserId, scopes: &[Scope], agent: bool) -> Caller {
    Caller {
        principal: Principal::authenticated(
            user,
            false,
            true,
            Credential::Token(scopes.to_vec()),
            forge_auth::AccountKind::Human,
        )
        .as_agent(agent),
        session: None,
    }
}

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

fn commit_on(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut listing = String::new();
    let mut sorted: Vec<&(&str, &str)> = files.iter().collect();
    sorted.sort_by_key(|(name, _)| *name);
    for (name, content) in sorted {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    router: axum::Router,
    merger: Merger,
    dev: Caller,
    dev_id: UserId,
    reporter: Caller,
    outsider: Caller,
    project: i64,
    head: String,
}

fn session_of(user: UserId, session: Option<i64>) -> Caller {
    Caller {
        principal: Principal::authenticated(
            user,
            false,
            true,
            Credential::Session,
            forge_auth::AccountKind::Human,
        ),
        session,
    }
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let person = |name: &'static str| NewUser::human(name, name, &format!("{name}@example.test"));
    let owner = users::create(&t.db, &ctx("req-owner"), &person("olga"))
        .await
        .expect("owner");
    let dev = users::create(&t.db, &ctx("req-dev"), &person("dave"))
        .await
        .expect("dev");
    let rita = users::create(&t.db, &ctx("req-rita"), &person("rita"))
        .await
        .expect("reporter");
    let outsider = users::create(&t.db, &ctx("req-out"), &person("otto"))
        .await
        .expect("outsider");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
    )
    .await
    .expect("web");
    for (who, role) in [
        (dev.user_id, Role::Developer),
        (rita.user_id, Role::Reporter),
    ] {
        projects::add_member(&t.db, &ctx("req-member"), web.project_id, who, role)
            .await
            .expect("member");
    }
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
    let base = commit_on(&path, None, &[("README", "base\n")]);
    let head = commit_on(
        &path,
        Some(&base),
        &[("README", "base\n"), ("feature.txt", "f\n")],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default());
    let refs = VersionRefs::new(git.clone(), storage.clone());
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
    .with_review(
        Review::new(t.db.clone(), Arc::new(browser.clone()))
            .with_version_refs(refs.clone())
            .with_review_git(forge_review::approvals::ReviewGit::new(
                git.clone(),
                storage.clone(),
            )),
    )
    .with_checks(forge_checks::Checks::new(
        t.db.clone(),
        Arc::new(browser.clone()),
    ));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let merger =
        Merger::new(t.db.clone(), git.clone(), storage.clone()).with_browser(browser.clone());
    let session = forge_db::sessions::issue(
        &t.db,
        &ctx("req-session"),
        &forge_db::sessions::NewSession {
            user: dev.user_id,
            idle: Duration::from_secs(3600),
            absolute: Duration::from_secs(8 * 3600),
            oidc: None,
        },
    )
    .await
    .expect("session")
    .session_id;
    let dev_caller = session_of(dev.user_id, Some(session));
    let p = web.project_id.get();
    let (status, opened) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        dev_caller.clone(),
        Some(json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let mr: i64 = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    let mr = forge_core::MergeRequestId::new(mr);
    let rid = RequestId::parse("req-worker").expect("id");
    let report = forge_review::refresh(
        &t.db,
        &refs,
        forge_review::RefreshRequest {
            merge_request: mr,
            pushed_by: None,
        },
        &rid,
    )
    .await
    .expect("refresh");
    let seq = report.check_requested.expect("a check");
    forge_review::check(
        &t.db,
        &Mergeability::new(git.clone(), storage.clone()),
        forge_db::review::MergeabilityRequest {
            merge_request: mr,
            check_seq: seq,
        },
        &rid,
    )
    .await
    .expect("check");
    World {
        t,
        _home: home,
        _root: root,
        path,
        router,
        merger,
        dev: dev_caller,
        dev_id: dev.user_id,
        reporter: session_of(rita.user_id, None),
        outsider: session_of(outsider.user_id, None),
        project: p,
        head,
    }
}

impl World {
    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/merge-requests/1{tail}", self.project)
    }

    async fn revert(&self, who: Caller) -> (StatusCode, Value) {
        send(self.router.clone(), "POST", &self.uri("/revert"), who, None).await
    }

    async fn can_revert(&self, who: Caller) -> Value {
        let (status, body) =
            send(self.router.clone(), "GET", &self.uri("/status"), who, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["viewer"]["can_revert"].clone()
    }

    /// dave merges through the route; the job lands it.
    async fn merged(&self) {
        let (status, body) = send(
            self.router.clone(),
            "POST",
            &self.uri("/merge"),
            self.dev.clone(),
            Some(json!({"sha": self.head})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let merge_id = body["merge_id"].as_i64().expect("merge id");
        let report = self.merger.execute(merge_id).await.expect("merge");
        assert_eq!(report.outcome, "applied");
    }

    fn revert_branch(&self) -> bool {
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&self.path)
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/revert/mr-1"])
            .output()
            .expect("git")
            .status
            .success()
    }

    async fn merge_requests(&self) -> usize {
        self.t
            .json_rows("SELECT id FROM forge.merge_requests")
            .await
            .len()
    }
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

/// The happy path: an open merge request is not revertible (409
/// FG-REVIEW-0029, `can_revert` false); once merged, dave's revert answers
/// 201 with the revert merge request (`revert/mr-1` → main, not a draft,
/// his own) and the status offered it; a second revert is refused.
#[tokio::test]
async fn a_merged_merge_request_is_reverted_through_the_route() {
    let w = world().await;
    assert_eq!(w.can_revert(w.dev.clone()).await, json!(false));
    let (status, body) = w.revert(w.dev.clone()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-REVIEW-0029"),
        "{body}"
    );
    w.merged().await;
    assert_eq!(w.can_revert(w.dev.clone()).await, json!(true));
    assert_eq!(w.can_revert(w.reporter.clone()).await, json!(false));
    let (status, body) = w.revert(w.dev.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["reverted_iid"], 1);
    assert_eq!(body["merge_request"]["iid"], 2);
    assert_eq!(body["merge_request"]["source_branch"], "revert/mr-1");
    assert_eq!(body["merge_request"]["target_branch"], "main");
    assert_eq!(body["merge_request"]["draft"], false);
    assert_eq!(body["merge_request"]["author_id"], w.dev_id.get());
    assert!(w.revert_branch());
    let (status, again) = w.revert(w.dev.clone()).await;
    assert_eq!(
        (status, code(&again)),
        (StatusCode::CONFLICT, "FG-REVIEW-0029"),
        "{again}"
    );
    w.t.drop_now().await;
}

/// Who may not: a Reporter (403 FG-AUTH-0003), a non-member of the
/// private project (404 FG-REPO-0005), a `read_api` token (403
/// FG-AUTH-0002) and an `api` token without `write_repository` (403
/// FG-AUTH-0002: the branch is a push) — nothing is written for any.
#[tokio::test]
async fn revert_permissions_are_decided_before_anything_is_written() {
    let w = world().await;
    w.merged().await;
    let before = w.merge_requests().await;
    let (status, body) = w.revert(w.reporter.clone()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    let (status, body) = w.revert(w.outsider.clone()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{body}"
    );
    let (status, body) = w.revert(token(w.dev_id, &[Scope::ReadApi], false)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        "{body}"
    );
    let (status, body) = w.revert(token(w.dev_id, &[Scope::Api], false)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        "{body}"
    );
    assert!(!w.revert_branch(), "no branch");
    assert_eq!(w.merge_requests().await, before, "no merge request");
    // With both scopes the token reverts.
    let (status, body) = w
        .revert(token(
            w.dev_id,
            &[Scope::Api, Scope::WriteRepository],
            false,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    w.t.drop_now().await;
}
