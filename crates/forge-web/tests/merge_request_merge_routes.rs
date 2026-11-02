#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge routes of M3 R4 (ADR-0024 §4) through the product router,
//! against the development database and a real bare repository:
//! `POST …/{iid}/merge` queues the merge (202) and `GET …/{iid}/merge`
//! shows the latest attempt; the job (`forge_review::Merger`, run by
//! `forge serve`) lands it. Agents' tokens and `api`-only tokens are
//! refused before anything is read; refusals of the request are 4xx with
//! their own codes.

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
    outsider: Caller,
    project: i64,
    head: String,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let owner = users::create(
        &t.db,
        &ctx("req-owner"),
        &NewUser::human("olga", "olga", "olga@example.test"),
    )
    .await
    .expect("owner");
    let dev = users::create(
        &t.db,
        &ctx("req-dev"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dev");
    let outsider = users::create(
        &t.db,
        &ctx("req-out"),
        &NewUser::human("otto", "otto", "otto@example.test"),
    )
    .await
    .expect("outsider");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        web.project_id,
        dev.user_id,
        Role::Developer,
    )
    .await
    .expect("member");
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
    // One browse cache for the routes and the merge job, as in `forge serve`.
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
    .with_review(
        Review::new(t.db.clone(), Arc::new(browser.clone()))
            .with_version_refs(refs.clone())
            .with_review_git(forge_review::approvals::ReviewGit::new(
                git.clone(),
                storage.clone(),
            )),
    );
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
    let dev_caller = Caller {
        principal: Principal::authenticated(
            dev.user_id,
            false,
            true,
            Credential::Session,
            forge_auth::AccountKind::Human,
        ),
        session: Some(session),
    };
    let outsider_caller = Caller {
        principal: Principal::authenticated(
            outsider.user_id,
            false,
            true,
            Credential::Session,
            forge_auth::AccountKind::Human,
        ),
        session: None,
    };
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
    // The worker's version job and mergeability check.
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
        outsider: outsider_caller,
        project: p,
        head,
    }
}

impl World {
    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/merge-requests/1{tail}", self.project)
    }

    async fn post_merge(&self, who: Caller, body: Value) -> (StatusCode, Value) {
        send(
            self.router.clone(),
            "POST",
            &self.uri("/merge"),
            who,
            Some(body),
        )
        .await
    }
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

/// The happy path: 202 with the queued attempt, the job lands it, the
/// status route shows `applied` and the merge request is `merged`.
#[tokio::test]
async fn merging_through_the_route_queues_the_attempt_and_the_job_lands_it() {
    let w = world().await;
    let (status, body) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "queued");
    assert_eq!(body["method"], "merge_commit");
    assert_eq!(body["target_branch"], "main");
    let merge_id = body["merge_id"].as_i64().expect("merge id");
    let (status, seen) = send(
        w.router.clone(),
        "GET",
        &w.uri("/merge"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{seen}");
    assert_eq!(seen["merge"]["state"], "queued");
    // A second merge, a close: the merge is in progress.
    let (status, again) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(
        (status, code(&again)),
        (StatusCode::CONFLICT, "FG-REVIEW-0011"),
        "{again}"
    );
    let (status, closed) = send(
        w.router.clone(),
        "POST",
        &w.uri("/close"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(
        (status, code(&closed)),
        (StatusCode::CONFLICT, "FG-REVIEW-0011"),
        "{closed}"
    );
    // The job (forge serve's merge runner).
    let report = w.merger.execute(merge_id).await.expect("merge");
    assert_eq!(report.outcome, "applied");
    let (_, seen) = send(
        w.router.clone(),
        "GET",
        &w.uri("/merge"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(seen["merge"]["state"], "applied", "{seen}");
    let tip = plumb(&w.path, &["rev-parse", "refs/heads/main"], None);
    assert_eq!(seen["merge"]["new_sha"], tip);
    let (_, mr) = send(w.router.clone(), "GET", &w.uri(""), w.dev.clone(), None).await;
    assert_eq!(mr["state"], "merged", "{mr}");
    // The browse API sees the merge at once (the same cache, invalidated).
    let (status, refs) = send(
        w.router.clone(),
        "GET",
        &format!("/api/v1/projects/{}/repository/refs", w.project),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{refs}");
    assert!(refs.to_string().contains(&tip), "{refs}");
    w.t.drop_now().await;
}

/// The request's refusals, each with its own code, nothing written: a bad
/// body (FG-WEB-0006), a method the project does not allow
/// (FG-REVIEW-0010), a head the requester did not see (FG-REVIEW-0008);
/// an outsider (FG-REPO-0005, as for a missing merge request); an `api`
/// token (FG-AUTH-0002: merges need `merge_execute`) and an agent's token
/// (FG-AUTH-0003: people only).
#[tokio::test]
async fn the_merge_route_refuses_with_its_codes_and_writes_nothing() {
    let w = world().await;
    for (body, status, expected) in [
        (
            json!({"sha": "nope"}),
            StatusCode::BAD_REQUEST,
            "FG-WEB-0006",
        ),
        (
            json!({"sha": w.head, "method": "rebase"}),
            StatusCode::BAD_REQUEST,
            "FG-WEB-0006",
        ),
        (
            json!({"sha": w.head, "method": "fast_forward"}),
            StatusCode::CONFLICT,
            "FG-REVIEW-0010",
        ),
        (
            json!({"sha": "a".repeat(40)}),
            StatusCode::CONFLICT,
            "FG-REVIEW-0008",
        ),
        (
            json!({"sha": w.head, "extra": 1}),
            StatusCode::BAD_REQUEST,
            "FG-WEB-0006",
        ),
    ] {
        let (got, answer) = w.post_merge(w.dev.clone(), body.clone()).await;
        assert_eq!(
            (got, code(&answer)),
            (status, expected),
            "{body} -> {answer}"
        );
    }
    let (got, answer) = w
        .post_merge(w.outsider.clone(), json!({"sha": w.head}))
        .await;
    assert_eq!(
        (got, code(&answer)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{answer}"
    );
    let (got, answer) = w
        .post_merge(
            token(w.dev_id, &[Scope::Api], false),
            json!({"sha": w.head}),
        )
        .await;
    assert_eq!(
        (got, code(&answer)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        "{answer}"
    );
    let (got, answer) = w
        .post_merge(
            token(w.dev_id, &[Scope::MergeExecute], true),
            json!({"sha": w.head}),
        )
        .await;
    assert_eq!(
        (got, code(&answer)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{answer}"
    );
    let rows =
        w.t.json_rows("SELECT count(*) AS n FROM forge.merge_request_merges")
            .await;
    assert_eq!(rows[0]["n"], 0);
    let (status, seen) = send(
        w.router.clone(),
        "GET",
        &w.uri("/merge"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(
        (status, seen["merge"].clone()),
        (StatusCode::OK, Value::Null),
        "{seen}"
    );
    w.t.drop_now().await;
}
