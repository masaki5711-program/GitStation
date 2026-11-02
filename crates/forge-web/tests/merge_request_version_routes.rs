#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The merge request reads of M3 R2 (ADR-0024 §1–§3) through the product
//! router, against the development database and a real bare repository:
//! `GET …/{iid}/versions`, `…/versions/{n}/diff`, `…/compare?from=&to=`
//! and `…/mergeability`. The versions and the status are written by the
//! worker's own functions (`forge_review::refresh` / `check`). A member
//! reads; someone who cannot see the project gets FG-REPO-0005; an unknown
//! and a purged version answer alike (FG-REPO-0023 ref_not_found); a trial
//! merge's objects never become browsable.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{Credential, Principal};
use forge_core::ids::MergeRequestId;
use forge_core::{RequestId, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{Mergeability, Review, VersionRefs};
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

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session)
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

/// A commit with `files` on top of `parent`.
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

/// The product router over `t`, `git` and `storage` (fresh browse caches).
fn build_router(t: &FreshDb, git: &Git, storage: &StorageRoot, refs: &VersionRefs) -> axum::Router {
    // clone: the router owns its handles
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default());
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
    .with_review(Review::new(t.db.clone(), Arc::new(browser)).with_version_refs(refs.clone()));
    forge_web::app(state, AppConfig::default()).expect("router")
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
    router: axum::Router,
    refs: VersionRefs,
    checker: Mergeability,
    git: Git,
    storage: StorageRoot,
    dev: UserId,
    outsider: UserId,
    admin: UserId,
    project: i64,
    mr: MergeRequestId,
    base: String,
    c1: String,
    c2: String,
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
    let outsider = users::create(
        &t.db,
        &ctx("req-out"),
        &NewUser::human("olga", "olga", "olga@example.test"),
    )
    .await
    .expect("outsider");
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
    let base = commit_on(&path, None, &[("README", "base\n")]);
    let c1 = commit_on(
        &path,
        Some(&base),
        &[("README", "base\n"), ("feature.txt", "one\n")],
    );
    let c2 = commit_on(
        &path,
        Some(&c1),
        &[("README", "base\n"), ("feature.txt", "two\n")],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &c1], None);
    let refs = VersionRefs::new(git.clone(), storage.clone()); // clone: shared handles
    let checker = Mergeability::new(git.clone(), storage.clone()); // clone: as refs
    let router = build_router(&t, &git, &storage, &refs);
    let p = web.project_id.get();
    let (status, opened) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        session(dev.user_id),
        Some(json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let id: i64 = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    World {
        t,
        _home: home,
        _root: root,
        path,
        router,
        refs,
        checker,
        git,
        storage,
        dev: dev.user_id,
        outsider: outsider.user_id,
        admin: admin.user_id,
        project: p,
        mr: MergeRequestId::new(id),
        base,
        c1,
        c2,
    }
}

impl World {
    fn fresh_router(&self) -> axum::Router {
        build_router(&self.t, &self.git, &self.storage, &self.refs)
    }

    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/merge-requests/1{tail}", self.project)
    }

    async fn get(&self, tail: &str, who: Principal) -> (StatusCode, Value) {
        send(self.router.clone(), "GET", &self.uri(tail), who, None).await
    }

    /// The version job, then every mergeability job it requested, as the
    /// worker runs them.
    async fn worker(&self, request: &str) {
        let rid = RequestId::parse(request).expect("id");
        let report = forge_review::refresh(
            &self.t.db,
            &self.refs,
            forge_review::RefreshRequest {
                merge_request: self.mr,
                pushed_by: None,
            },
            &rid,
        )
        .await
        .expect("refresh");
        if let Some(seq) = report.check_requested {
            forge_review::check(
                &self.t.db,
                &self.checker,
                forge_db::review::MergeabilityRequest {
                    merge_request: self.mr,
                    check_seq: seq,
                },
                &rid,
            )
            .await
            .expect("check");
        }
    }
}

#[tokio::test]
async fn versions_diffs_compare_and_mergeability_are_read_by_members_only() {
    let w = world().await;
    let me = || session(w.dev);

    // Before the worker ran: no version, never checked.
    let (status, body) = w.get("/versions", me()).await;
    assert_eq!(
        (status, body["versions"].clone()),
        (StatusCode::OK, json!([])),
        "{body}"
    );
    let (status, body) = w.get("/mergeability", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "unchecked");
    assert_eq!(body["head_sha"], Value::Null);

    // Version 1 (c1), then version 2 (c2), each checked.
    w.worker("req-v1").await;
    plumb(&w.path, &["update-ref", "refs/heads/feature", &w.c2], None);
    w.worker("req-v2").await;

    let (status, body) = w.get("/versions", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let versions = body["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version"], 1);
    assert_eq!(versions[0]["head_sha"], w.c1.as_str());
    assert_eq!(versions[0]["base_sha"], w.base.as_str());
    assert_eq!(versions[0]["merge_base_sha"], w.base.as_str());
    assert_eq!(versions[1]["head_sha"], w.c2.as_str());
    assert_eq!(versions[1]["refs_purged"], false);

    // Version 2's diff: merge base → head (both commits, one file).
    let (status, body) = w.get("/versions/2/diff", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["from"], w.base.as_str());
    assert_eq!(body["to"], w.c2.as_str());
    assert_eq!(body["commits"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["diff"]["files"][0]["new_path"], "feature.txt");
    assert_eq!(body["diff"]["files"][0]["status"], "added");

    // Version 1 → 2: head to head, one commit, a modification.
    let (status, body) = w.get("/compare?from=1&to=2", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["from_sha"], w.c1.as_str());
    assert_eq!(body["to_sha"], w.c2.as_str());
    assert_eq!(body["commits"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["diff"]["files"][0]["status"], "modified");

    let (status, body) = w.get("/mergeability", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "can_be_merged");
    assert_eq!(body["head_sha"], w.c2.as_str());
    assert_eq!(body["checked_head_sha"], w.c2.as_str());
    assert_eq!(body["checked_target_sha"], w.base.as_str());

    // Unknown versions and malformed parameters.
    let (status, body) = w.get("/versions/9/diff", me()).await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::NOT_FOUND, Some("FG-REPO-0023")),
        "{body}"
    );
    let unknown = body["user_message"].clone();
    let (status, body) = w.get("/compare?from=1&to=9", me()).await;
    assert_eq!(body["error_code"], "FG-REPO-0023", "{status} {body}");
    for bad in [
        "/compare?from=1",
        "/compare?from=0&to=2",
        "/compare?from=1&to=2&x=1",
    ] {
        let (status, body) = w.get(bad, me()).await;
        assert_eq!(
            (status, body["error_code"].as_str()),
            (StatusCode::BAD_REQUEST, Some("FG-WEB-0006")),
            "{bad}: {body}"
        );
    }
    let (status, body) = w.get("/versions/x/diff", me()).await;
    assert_eq!(body["error_code"], "FG-WEB-0006", "{status} {body}");

    // Someone who cannot see the private project: every read is 404.
    for tail in [
        "/versions",
        "/versions/1/diff",
        "/compare?from=1&to=2",
        "/mergeability",
    ] {
        let (status, body) = w.get(tail, session(w.outsider)).await;
        assert_eq!(
            (status, body["error_code"].as_str()),
            (StatusCode::NOT_FOUND, Some("FG-REPO-0005")),
            "{tail}: {body}"
        );
    }
    // A token needs a read scope like the browse API.
    let read =
        Principal::authenticated(w.dev, false, true, Credential::Token(vec![Scope::ReadApi]));
    let (status, body) = w.get("/versions", read).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Purge version 1 (the administrator closes the merge request first,
    // as the runbook says): it answers like an unknown version.
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/close"),
        session(w.dev),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/version-refs/purge"),
        Principal::authenticated(w.admin, true, true, Credential::Session),
        Some(json!({"versions": [1]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = w.get("/versions/1/diff", me()).await;
    assert_eq!(
        (
            status,
            body["error_code"].as_str(),
            body["user_message"].clone()
        ),
        (StatusCode::NOT_FOUND, Some("FG-REPO-0023"), unknown),
        "a purged version answers like an unknown one: {body}"
    );
    let (_, body) = w.get("/compare?from=1&to=2", me()).await;
    assert_eq!(body["error_code"], "FG-REPO-0023", "{body}");
    let (status, body) = w.get("/versions", me()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["versions"][0]["refs_purged"], true);
    w.t.drop_now().await;
}

const READS: [&str; 4] = [
    "/versions",
    "/versions/1/diff",
    "/compare?from=1&to=2",
    "/mergeability",
];

/// Review (pr-test): nobody signed in is FG-AUTH-0001 (401) on every read;
/// a token without a REST read scope (git-only) is FG-AUTH-0002 (403).
#[tokio::test]
async fn reads_refuse_anonymous_callers_and_tokens_without_a_read_scope() {
    let w = world().await;
    for tail in READS {
        let (status, body) = w.get(tail, Principal::anonymous()).await;
        assert_eq!(
            (status, body["error_code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("FG-AUTH-0001")),
            "{tail}: {body}"
        );
        for scopes in [vec![Scope::WriteRepository], vec![]] {
            let token =
                Principal::authenticated(w.dev, false, true, Credential::Token(scopes.clone()));
            let (status, body) = w.get(tail, token).await;
            assert_eq!(
                (status, body["error_code"].as_str()),
                (StatusCode::FORBIDDEN, Some("FG-AUTH-0002")),
                "{tail} {scopes:?}: {body}"
            );
        }
    }
    w.t.drop_now().await;
}

/// Review (pr-test): a version whose bases the version job has not
/// recorded yet has no diff: FG-REPO-0023 (`version_base=pending` in the
/// log only), like an unknown version.
#[tokio::test]
async fn diff_of_a_version_without_bases_is_not_found_pending() {
    let w = world().await;
    forge_db::review::record_mr_version(
        &w.t.db,
        &ctx("req-v1"),
        w.mr,
        &forge_core::ObjectId::parse(&w.c1).expect("oid"),
        None,
    )
    .await
    .expect("v1 without bases");
    let (status, body) = w.get("/versions/1/diff", session(w.dev)).await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (StatusCode::NOT_FOUND, Some("FG-REPO-0023")),
        "{body}"
    );
    w.t.drop_now().await;
}

/// Review (stale decisions): when the target moves after a check, the API
/// reports `unchecked` with `stale: true` — never the old decision — and
/// the stored pair stays visible.
#[tokio::test]
async fn a_decision_for_an_old_target_tip_is_reported_unchecked_and_stale() {
    let w = world().await;
    w.worker("req-v1").await;
    let (_, body) = w.get("/mergeability", session(w.dev)).await;
    assert_eq!(body["status"], "can_be_merged", "{body}");
    assert_eq!(body["stale"], false);
    let moved = commit_on(&w.path, Some(&w.base), &[("README", "moved\n")]);
    plumb(&w.path, &["update-ref", "refs/heads/main", &moved], None);
    // The browse ref list is cached for 5 s: a fresh router reads it anew.
    let (status, body) = send(
        w.fresh_router(),
        "GET",
        &w.uri("/mergeability"),
        session(w.dev),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "unchecked", "{body}");
    assert_eq!(body["stale"], true);
    assert_eq!(body["reason"], Value::Null);
    assert_eq!(body["checked_target_sha"], w.base.as_str());
    w.t.drop_now().await;
}
