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
    // M3 R6: the protected-ref routes (merge rules).
    .with_hosting(forge_hosting::Hosting::new(t.db.clone()))
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

// ── M3 R5: approvals and approval rules (ADR-0024 §8, §12) ──

impl World {
    /// olga (the project's Owner) in a web session of her own.
    async fn owner(&self) -> Caller {
        let id = self
            .t
            .json_rows("SELECT id FROM forge.users WHERE username = 'olga'")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .map(UserId::new)
            .expect("olga");
        let session = forge_db::sessions::issue(
            &self.t.db,
            &ctx("req-session-olga"),
            &forge_db::sessions::NewSession {
                user: id,
                idle: Duration::from_secs(3600),
                absolute: Duration::from_secs(8 * 3600),
                oidc: None,
            },
        )
        .await
        .expect("session")
        .session_id;
        Caller {
            principal: Principal::authenticated(
                id,
                false,
                true,
                Credential::Session,
                forge_auth::AccountKind::Human,
            ),
            session: Some(session),
        }
    }

    fn rules_uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/approval-rules{tail}", self.project)
    }
}

/// Approve, read the evaluation, withdraw — and the refusals: the author
/// (FG-REVIEW-0013), a stale head (FG-REVIEW-0008), an agent's token and an
/// `api`-only token, an outsider (404).
#[tokio::test]
async fn the_approval_routes_approve_evaluate_and_withdraw() {
    let w = world().await;
    let olga = w.owner().await;
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/approve"),
        w.dev.clone(),
        Some(json!({"sha": w.head})),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-REVIEW-0013"),
        "{body}"
    );
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/approve"),
        olga.clone(),
        Some(json!({"sha": "a".repeat(40)})),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-REVIEW-0008"),
        "{body}"
    );
    for (who, expected) in [
        (
            token(w.dev_id, &[Scope::MergeApprove], true),
            "FG-AUTH-0003",
        ),
        (token(w.dev_id, &[Scope::Api], false), "FG-AUTH-0002"),
    ] {
        let (status, body) = send(
            w.router.clone(),
            "POST",
            &w.uri("/approve"),
            who,
            Some(json!({"sha": w.head})),
        )
        .await;
        assert_eq!(code(&body), expected, "{status} {body}");
    }
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/approve"),
        w.outsider.clone(),
        Some(json!({"sha": w.head})),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{body}"
    );
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/approve"),
        olga.clone(),
        Some(json!({"sha": w.head})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "approved");
    assert_eq!(body["approval"]["head_sha"], w.head);
    let (status, state) = send(
        w.router.clone(),
        "GET",
        &w.uri("/approvals"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["approvals"][0]["username"], "olga");
    assert_eq!(state["blocked_by"], json!([]));
    let (status, body) = send(
        w.router.clone(),
        "POST",
        &w.uri("/unapprove"),
        olga.clone(),
        None,
    )
    .await;
    assert_eq!(
        (status, &body["outcome"]),
        (StatusCode::OK, &json!("withdrawn")),
        "{body}"
    );
    w.t.drop_now().await;
}

/// Rules: a Maintainer+ person puts and deletes; a Developer is refused; a
/// rule blocks the merge route with `approvals_missing` until an approval.
#[tokio::test]
async fn the_approval_rule_routes_manage_rules_that_block_the_merge() {
    let w = world().await;
    let olga = w.owner().await;
    let rule = json!({"target_pattern": "refs/heads/main", "required_approvals": 1});
    let (status, body) = send(
        w.router.clone(),
        "PUT",
        &w.rules_uri("/review"),
        w.dev.clone(),
        Some(rule.clone()),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    let (status, body) = send(
        w.router.clone(),
        "PUT",
        &w.rules_uri("/review"),
        olga.clone(),
        Some(rule.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "review");
    let (status, body) = send(
        w.router.clone(),
        "PUT",
        &w.rules_uri("/Bad%20Name"),
        olga.clone(),
        Some(rule),
    )
    .await;
    assert_eq!(code(&body), "FG-WEB-0006", "{status} {body}");
    let (status, list) = send(
        w.router.clone(),
        "GET",
        &w.rules_uri(""),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["rules"][0]["required_approvals"], 1);
    let (status, body) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-REVIEW-0007"),
        "{body}"
    );
    // Without a push record nobody can be shown not to be a committer
    // (committers_unknown, fail closed): record dave's push of the branch.
    let repo =
        w.t.json_rows("SELECT id FROM forge.repositories")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .map(forge_core::RepoId::new)
            .expect("repository");
    let head = forge_core::ObjectId::parse(&w.head).expect("oid");
    let recorded = repositories::record_push(
        &w.t.db,
        &ChangeContext::new(
            Actor::User(w.dev_id),
            RequestId::parse("req-push-dave").expect("id"),
        ),
        &forge_core::PushRecord {
            repository_id: repo,
            updates: vec![forge_core::PushedRef {
                name: forge_core::RefName::parse("refs/heads/feature").expect("ref"),
                old: forge_core::ObjectId::parse(&"0".repeat(40)).expect("oid"),
                new: head.clone(),
                kind: forge_core::PushUpdateKind::Create,
                commits: vec![head],
                total_commits: 1,
                total_commits_partial: false,
            }],
        },
    )
    .await
    .expect("push record");
    assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    let (status, _) = send(
        w.router.clone(),
        "POST",
        &w.uri("/approve"),
        olga.clone(),
        Some(json!({"sha": w.head})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, _) = send(
        w.router.clone(),
        "DELETE",
        &w.rules_uri("/review"),
        olga.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    w.t.drop_now().await;
}

// ── M3 R6: merge settings, the CODEOWNERS switch, merge rules, the status ──

impl World {
    fn project_uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}{tail}", self.project)
    }
}

/// ADR-0024 §4.1, §12: anyone who reads the project reads the settings
/// (the defaults first); only a Maintainer (people only) replaces them;
/// a default the methods do not allow is FG-WEB-0006; the merge route then
/// uses the project's default method.
#[tokio::test]
async fn the_merge_settings_routes_read_and_write_the_settings() {
    let w = world().await;
    let olga = w.owner().await;
    let uri = w.project_uri("/merge-settings");
    let (status, body) = send(w.router.clone(), "GET", &uri, w.dev.clone(), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["allowed_methods"], json!(["merge_commit", "squash"]));
    assert_eq!(body["codeowners"]["enforced"], true);
    assert_eq!(body["stored"], false);
    let ff_only = json!({
        "allowed_methods": ["fast_forward"],
        "default_method": "fast_forward",
        "delete_source_default": false,
        "reset_approvals_on_push": true,
        "committer_approvals_excluded": true
    });
    let (status, body) = send(
        w.router.clone(),
        "PUT",
        &uri,
        w.dev.clone(),
        Some(ff_only.clone()),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    let mut bad = ff_only.clone();
    bad["default_method"] = json!("squash");
    let (status, body) = send(w.router.clone(), "PUT", &uri, olga.clone(), Some(bad)).await;
    assert_eq!(code(&body), "FG-WEB-0006", "{status} {body}");
    let (status, body) = send(w.router.clone(), "PUT", &uri, olga.clone(), Some(ff_only)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["default_method"], "fast_forward");
    assert_eq!(body["stored"], true);
    let (status, body) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["method"], "fast_forward");
    w.t.drop_now().await;
}

/// ADR-0024 §9: the CODEOWNERS switch needs a Maintainer and an expiry of
/// 1..=1440 minutes (FG-AUTH-0003 otherwise); a suspension shows its end
/// and is an event; enforcing again restores it.
#[tokio::test]
async fn the_codeowners_route_suspends_for_a_bounded_time_only() {
    let w = world().await;
    let olga = w.owner().await;
    let uri = w.project_uri("/merge-settings/codeowners-enforcement");
    let put = |who: Caller, body: Value| send(w.router.clone(), "PUT", &uri, who, Some(body));
    let (status, body) = put(w.dev.clone(), json!({"enforced": false, "for_minutes": 60})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    for refused in [
        json!({"enforced": false}),
        json!({"enforced": false, "for_minutes": 1441}),
        json!({"enforced": false, "for_minutes": 0}),
    ] {
        let (status, body) = put(olga.clone(), refused.clone()).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{refused} {body}"
        );
    }
    let (status, body) = put(olga.clone(), json!({"enforced": false, "for_minutes": 60})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["codeowners"]["enforced"], false);
    assert!(body["codeowners"]["suspended_until"].is_string(), "{body}");
    let events =
        w.t.json_rows(
            "SELECT payload FROM forge.events \
             WHERE event_type = 'merge_settings.codeowners_enforcement_changed'",
        )
        .await;
    assert_eq!(events.len(), 1);
    let (status, body) = put(olga.clone(), json!({"enforced": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["codeowners"]["enforced"], true);
    w.t.drop_now().await;
}

/// ADR-0024 §7, §10, §11, §12: a merge rule refuses `ext/` checks
/// (FG-REVIEW-0016) and the work item link (FG-WEB-0006); set, it is
/// listed with its rule and the status (the stable contract) shows every
/// reason; the merge is refused with them; removed, nothing blocks.
#[tokio::test]
async fn the_merge_rule_routes_and_the_status_contract() {
    let w = world().await;
    let olga = w.owner().await;
    let (status, created) = send(
        w.router.clone(),
        "POST",
        &w.project_uri("/protected-refs"),
        olga.clone(),
        Some(json!({"pattern": "refs/heads/main", "push_min_role": "developer"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created["merge_rule"].is_null());
    let rule = created["id"].as_i64().expect("rule id");
    let uri = w.project_uri(&format!("/protected-refs/{rule}/merge-rule"));
    let put = |who: Caller, body: Value| send(w.router.clone(), "PUT", &uri, who, Some(body));
    let (status, body) = put(olga.clone(), json!({"required_checks": ["ext/sonar"]})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "FG-REVIEW-0016"),
        "{body}"
    );
    let (status, body) = put(olga.clone(), json!({"require_work_item_link": true})).await;
    assert_eq!(code(&body), "FG-WEB-0006", "{status} {body}");
    let (status, body) = put(w.dev.clone(), json!({"required_approvals": 1})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    let wanted = json!({
        "required_approvals": 1,
        "required_checks": ["ci/gates"],
        "require_resolved_discussions": true
    });
    let (status, body) = put(olga.clone(), wanted).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["merge_rule"]["required_checks"], json!(["ci/gates"]));
    let (_, list) = send(
        w.router.clone(),
        "GET",
        &w.project_uri("/protected-refs"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(list["rules"][0]["merge_rule"]["required_approvals"], 1);
    let (status, s) = send(
        w.router.clone(),
        "GET",
        &w.uri("/status"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{s}");
    assert_eq!(s["schema_version"], 1);
    assert_eq!(s["mergeable"], false);
    let reasons: Vec<&str> = s["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .filter_map(|r| r["reason"].as_str())
        .collect();
    assert_eq!(
        reasons,
        ["approvals_missing", "required_check_missing"],
        "{s}"
    );
    assert_eq!(s["checks"][0]["name"], "ci/gates");
    assert!(s["checks"][0]["status"].is_null(), "no run reported");
    assert_eq!(s["discussions"]["unresolved"], 0);
    assert_eq!(s["method"], "merge_commit");
    let (status, body) = w.post_merge(w.dev.clone(), json!({"sha": w.head})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-REVIEW-0007"),
        "{body}"
    );
    let (status, _) = send(w.router.clone(), "DELETE", &uri, olga.clone(), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, s) = send(
        w.router.clone(),
        "GET",
        &w.uri("/status"),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(s["mergeable"], true, "{s}");
    assert_eq!(s["reasons"], json!([]));
    // An outsider gets the not-found answer, as for the merge request.
    let (status, body) = send(
        w.router.clone(),
        "GET",
        &w.uri("/status"),
        w.outsider.clone(),
        None,
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{body}"
    );
    w.t.drop_now().await;
}
