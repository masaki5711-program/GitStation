#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The discussions of M3 R3 (ADR-0024 §6, §12) through the product router,
//! against the development database and a real bare repository:
//! `GET|POST …/{iid}/discussions`, `POST …/discussions/{d}/notes`,
//! `…/resolve`, `…/unresolve`. The permission rows through HTTP (an
//! agent's token comments but never resolves; the merge request's author
//! never resolves another's thread; the starter and a Developer who is not
//! the author do), the 404 of an invisible project or a missing thread,
//! and the inline position check (FG-REVIEW-0004).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::ids::MergeRequestId;
use forge_core::namespace_resolver::LookupFuture;
use forge_core::{ObjectId, RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Diff;
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::{DiffProbe, Review};
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn agent(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::Human,
    )
    .as_agent(true)
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

/// A commit whose `lib.rs` has lines 1..=20, line 10 being `ten`.
fn commit(repo: &std::path::Path, ten: &str, parent: Option<&str>) -> String {
    let content: String = (1..=20)
        .map(|n| {
            if n == 10 {
                format!("{ten}\n")
            } else {
                format!("line {n}\n")
            }
        })
        .collect();
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tlib.rs\n").as_bytes()),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

/// The browse API's diffs, failing (`FG-GIT-0004` busy) once `allowed`
/// calls have run: a write whose position check passed and whose
/// `outdated` view then fails (review round 1).
struct Flaky {
    browser: Arc<Browser>,
    allowed: AtomicUsize,
}

impl DiffProbe for Flaky {
    fn merge_request_diff<'a>(
        &'a self,
        repo: forge_core::RepoId,
        iid: i64,
        from: &'a str,
        to: &'a str,
        caller: &'a str,
    ) -> LookupFuture<'a, Diff> {
        let left = self.allowed.load(Ordering::SeqCst);
        if left == 0 {
            return Box::pin(async { Err(ForgeError::new(ErrorCode::GitBusy)) });
        }
        self.allowed.store(left - 1, Ordering::SeqCst);
        self.browser.merge_request_diff(repo, iid, from, to, caller)
    }
}

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    router: axum::Router,
    flaky: Arc<Flaky>,
    path: std::path::PathBuf,
    base: String,
    mr: MergeRequestId,
    project: i64,
    /// A service account with the Developer role.
    svc: UserId,
    dave_ns: forge_core::NamespaceId,
    /// The project's Owner and the merge request's author.
    dave: UserId,
    bob: UserId,
    rita: UserId,
    gus: UserId,
    outsider: UserId,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let user = |name: &'static str| {
        let db = t.db.clone();
        async move {
            users::create(
                &db,
                &ctx(&format!("req-{name}")),
                &NewUser::human(name, name, &format!("{name}@example.test")),
            )
            .await
            .expect("user")
        }
    };
    let dave = user("dave").await;
    let bob = user("bob").await.user_id;
    let rita = user("rita").await.user_id;
    let gus = user("gus").await.user_id;
    let outsider = user("olga").await.user_id;
    let svc = users::create(
        &t.db,
        &ctx("req-svc"),
        &NewUser {
            kind: forge_core::UserKind::ServiceAccount,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("service account")
    .user_id;
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", dave.namespace_id, "Web", dave.user_id),
    )
    .await
    .expect("web");
    for (who, role) in [
        (bob, Role::Developer),
        (rita, Role::Reporter),
        (gus, Role::Guest),
        (svc, Role::Developer),
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
    let base = commit(&path, "line 10", None);
    let v1 = commit(&path, "ten", Some(&base));
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &v1], None);
    plumb(&path, &["update-ref", "refs/heads/fix", &v1], None);
    // clone: the router owns its handles
    let browser = Arc::new(Browser::new(
        git.clone(),
        storage.clone(),
        BrowseLimits::default(),
    ));
    let smart = SmartHttp::new(git, storage, GitHttpLimits::default());
    let flaky = Arc::new(Flaky {
        browser: browser.clone(), // clone: the probe shares the browse caches
        allowed: AtomicUsize::new(usize::MAX),
    });
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
    .with_review(Review::new(t.db.clone(), browser).with_diffs(flaky.clone()));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let p = web.project_id.get();
    let (status, opened) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        session(dave.user_id),
        Some(json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    // Version 1 as the version job writes it: the row, the bases, the ref.
    let id = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    let mr = MergeRequestId::new(id);
    let oid = |raw: &str| ObjectId::parse(raw).expect("oid");
    forge_db::review::record_mr_version(&t.db, &ctx("req-v1"), mr, &oid(&v1), None)
        .await
        .expect("version");
    forge_db::review::record_version_bases(
        &t.db,
        &ctx("req-v1-b"),
        mr,
        1,
        &oid(&base),
        Some(&oid(&base)),
    )
    .await
    .expect("bases");
    plumb(
        &path,
        &["update-ref", "refs/merge-requests/1/v1", &v1],
        None,
    );
    World {
        t,
        _home: home,
        _root: root,
        router,
        flaky,
        path,
        base,
        mr,
        svc,
        dave_ns: dave.namespace_id,
        project: p,
        dave: dave.user_id,
        bob,
        rita,
        gus,
        outsider,
    }
}

impl World {
    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/merge-requests/1{tail}", self.project)
    }

    async fn post(&self, tail: &str, who: Principal, body: Option<Value>) -> (StatusCode, Value) {
        send(self.router.clone(), "POST", &self.uri(tail), who, body).await
    }
}

fn code(v: &Value) -> &str {
    v["error_code"].as_str().unwrap_or("")
}

/// ADR-0024 §12 through HTTP: who comments, who resolves.
#[tokio::test]
async fn agents_comment_but_only_people_who_may_resolve_resolve() {
    let w = world().await;
    let general = json!({"body": "Please **rename** this <script>kept as text</script>"});
    let (status, thread) = w
        .post("/discussions", session(w.rita), Some(general.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    assert_eq!(thread["kind"], json!("general"));
    assert_eq!(thread["outdated"], json!(false));
    assert_eq!(thread["started_by"], json!(w.rita.get()));
    // Stored and returned as written: the server renders nothing.
    assert_eq!(thread["notes"][0]["body"], general["body"]);
    let d = thread["id"].as_i64().expect("id");
    let (status, body) = w
        .post("/discussions", session(w.gus), Some(general.clone()))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let (status, by_agent) = w
        .post("/discussions", agent(w.bob), Some(general.clone()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "an agent comments: {by_agent}");
    let resolve = format!("/discussions/{d}/resolve");
    let (status, body) = w.post(&resolve, agent(w.bob), None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let (status, body) = w.post(&resolve, session(w.dave), None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "the merge request's author never resolves another's thread"
    );
    let (status, resolved) = w.post(&resolve, session(w.rita), None).await;
    assert_eq!(status, StatusCode::OK, "{resolved}");
    assert_eq!(resolved["resolved"], json!(true));
    assert_eq!(resolved["resolved_by"], json!(w.rita.get()));
    let (status, reopened) = w
        .post(&format!("/discussions/{d}/unresolve"), session(w.bob), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{reopened}");
    assert_eq!(reopened["resolved"], json!(false));
    let (status, replied) = w
        .post(
            &format!("/discussions/{d}/notes"),
            session(w.bob),
            Some(json!({"body": "done"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{replied}");
    assert_eq!(replied["notes"].as_array().map(Vec::len), Some(2));
    let (status, list) = send(
        w.router.clone(),
        "GET",
        &w.uri("/discussions"),
        session(w.gus),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a Guest of a private project reads nothing: {list}"
    );
    let (status, list) = send(
        w.router.clone(),
        "GET",
        &w.uri("/discussions"),
        session(w.rita),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["discussions"].as_array().map(Vec::len), Some(2));
    assert_eq!(list["truncated"], json!(false));
    let events =
        w.t.json_rows(
            "SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.commented'",
        )
        .await;
    assert_eq!(
        events[0]["n"],
        json!(5),
        "2 starts, 1 reply, 1 resolve, 1 unresolve"
    );
    w.t.drop_now().await;
}

/// An invisible project's thread, a missing merge request and a missing
/// thread are the same 404 on every discussion route.
#[tokio::test]
async fn an_invisible_or_missing_discussion_is_the_same_404() {
    let w = world().await;
    let (_, thread) = w
        .post("/discussions", session(w.rita), Some(json!({"body": "x"})))
        .await;
    let d = thread["id"].as_i64().expect("id");
    let note = json!({"body": "y"});
    let mut answers = Vec::new();
    for (who, tail, body) in [
        (w.outsider, "/discussions".to_owned(), Some(note.clone())),
        (
            w.outsider,
            format!("/discussions/{d}/notes"),
            Some(note.clone()),
        ),
        (w.outsider, format!("/discussions/{d}/resolve"), None),
        (w.rita, format!("/discussions/{}/resolve", d + 100), None),
        (
            w.rita,
            format!("/discussions/{}/notes", d + 100),
            Some(note.clone()),
        ),
    ] {
        let (status, mut answer) = w.post(&tail, session(who), body).await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{tail}"
        );
        answer.as_object_mut().expect("error").remove("request_id");
        answers.push(answer);
    }
    let (status, listed) = send(
        w.router.clone(),
        "GET",
        &w.uri("/discussions"),
        session(w.outsider),
        None,
    )
    .await;
    assert_eq!(
        (status, code(&listed)),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    assert!(answers.windows(2).all(|p| p[0] == p[1]), "{answers:?}");
    w.t.drop_now().await;
}

/// An inline thread on a line of version 1's diff, and the positions
/// FG-REVIEW-0004 refuses (422).
#[tokio::test]
async fn inline_positions_are_checked_against_the_version_diff() {
    let w = world().await;
    let at = |line: i64, side: &str, path: &str| json!({"body": "here", "position": {"version": 1, "path": path, "side": side, "line": line}});
    let (status, inline) = w
        .post(
            "/discussions",
            session(w.bob),
            Some(at(10, "new", "lib.rs")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{inline}");
    assert_eq!(inline["kind"], json!("inline"));
    assert_eq!(
        inline["position"],
        json!({"version": 1, "path": "lib.rs", "side": "new", "line": 10})
    );
    for body in [at(1, "new", "lib.rs"), at(10, "new", "other.rs")] {
        let (status, answer) = w.post("/discussions", session(w.bob), Some(body)).await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::UNPROCESSABLE_ENTITY, "FG-REVIEW-0004"),
            "{answer}"
        );
        assert!(
            !answer.to_string().contains("other.rs"),
            "no path in the answer"
        );
    }
    let (status, answer) = w
        .post(
            "/discussions",
            session(w.bob),
            Some(at(10, "right", "lib.rs")),
        )
        .await;
    assert_eq!(code(&answer), "FG-WEB-0006", "{status} {answer}");
    w.t.drop_now().await;
}

impl World {
    /// Version `n` whose head rewrites line 10 as `ten`, recorded as the
    /// version job does (row, bases, ref).
    async fn version(&self, n: i64, ten: &str) {
        let head = commit(&self.path, ten, Some(&self.base));
        plumb(
            &self.path,
            &["update-ref", "refs/heads/feature", &head],
            None,
        );
        let oid = |raw: &str| ObjectId::parse(raw).expect("oid");
        forge_db::review::record_mr_version(
            &self.t.db,
            &ctx(&format!("req-v{n}")),
            self.mr,
            &oid(&head),
            None,
        )
        .await
        .expect("version");
        forge_db::review::record_version_bases(
            &self.t.db,
            &ctx(&format!("req-v{n}-b")),
            self.mr,
            n,
            &oid(&self.base),
            Some(&oid(&self.base)),
        )
        .await
        .expect("bases");
        plumb(
            &self.path,
            &["update-ref", &format!("refs/merge-requests/1/v{n}"), &head],
            None,
        );
    }

    async fn commented(&self) -> i64 {
        self.t
            .json_rows("SELECT count(*) AS n FROM forge.events WHERE event_type = 'merge_request.commented'")
            .await[0]["n"]
            .as_i64()
            .expect("count")
    }

    /// Set the merge request's state behind the API (no merge route yet).
    async fn set_state(&self, state: &str) {
        let closed = if state == "closed" {
            format!("closed_at = now(), closed_by = {}", self.dave.get())
        } else {
            "closed_at = NULL, closed_by = NULL".to_owned()
        };
        self.t
            .commit_audited(
                ("project", self.project),
                &[format!(
                    "UPDATE forge.merge_requests SET state = '{state}', {closed}, \
                     revision = revision + 1 WHERE id = {}",
                    self.mr.get()
                )],
            )
            .await;
    }
}

fn service_account(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::ServiceAccount,
    )
}

/// Review round 1 (HIGH): a committed write answers with what it stored
/// even when the `outdated` view's diff fails — 201 with
/// `outdated_unknown: true`, one event, nothing to retry.
#[tokio::test]
async fn a_failed_outdated_view_never_fails_a_committed_write() {
    let w = world().await;
    w.version(2, "ten v2").await;
    // The position check's diff may run; the view's diff fails.
    w.flaky.allowed.store(1, Ordering::SeqCst);
    let body = json!({"body": "here", "position": {"version": 1, "path": "lib.rs", "side": "new", "line": 10}});
    let (status, created) = w.post("/discussions", session(w.bob), Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["outdated"], json!(true));
    assert_eq!(created["outdated_unknown"], json!(true));
    assert_eq!(w.commented().await, 1, "stored once");
    // The list answers too, marking the thread unknown.
    let (status, list) = send(
        w.router.clone(),
        "GET",
        &w.uri("/discussions"),
        session(w.rita),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["discussions"][0]["outdated_unknown"], json!(true));
    w.flaky.allowed.store(usize::MAX, Ordering::SeqCst);
    let (_, list) = send(
        w.router.clone(),
        "GET",
        &w.uri("/discussions"),
        session(w.rita),
        None,
    )
    .await;
    assert_eq!(list["discussions"][0]["outdated"], json!(true));
    assert_eq!(list["discussions"][0]["outdated_unknown"], json!(false));
    w.t.drop_now().await;
}

/// Review round 1: note bodies over HTTP for both write routes — empty,
/// blank, NUL and unknown fields are FG-WEB-0006; 60 KiB is the cap (a
/// note the rule accepts always fits the JSON body cap).
#[tokio::test]
async fn note_bodies_are_checked_over_http() {
    let w = world().await;
    let (_, thread) = w
        .post("/discussions", session(w.rita), Some(json!({"body": "x"})))
        .await;
    let d = thread["id"].as_i64().expect("id");
    let reply = format!("/discussions/{d}/notes");
    let cap = forge_review::MAX_NOTE_BYTES;
    for tail in ["/discussions", reply.as_str()] {
        for (body, ok) in [
            (json!({"body": ""}), false),
            (json!({"body": " \n\t "}), false),
            (json!({"body": "a\u{0}b"}), false),
            (json!({"body": "x", "extra": 1}), false),
            (json!({"body": "x".repeat(cap)}), true),
            (json!({"body": "x".repeat(cap + 1)}), false),
        ] {
            let (status, answer) = w.post(tail, session(w.bob), Some(body)).await;
            if ok {
                assert_eq!(status, StatusCode::CREATED, "{tail} {answer}");
            } else {
                assert_eq!(
                    (status, code(&answer)),
                    (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
                    "{tail}"
                );
            }
        }
    }
    w.t.drop_now().await;
}

/// Review round 1 (IDOR): a real thread of another merge request of the
/// same project, or of another project, named under this merge request is
/// the same 404 on reply, resolve and unresolve — nothing is written.
#[tokio::test]
async fn a_thread_of_another_merge_request_is_not_found_here() {
    let w = world().await;
    let (status, other_mr) = send(
        w.router.clone(),
        "POST",
        &format!("/api/v1/projects/{}/merge-requests", w.project),
        session(w.dave),
        Some(json!({"title": "Fix", "source_branch": "fix", "target_branch": "main"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{other_mr}");
    let (status, foreign) = send(
        w.router.clone(),
        "POST",
        &format!(
            "/api/v1/projects/{}/merge-requests/2/discussions",
            w.project
        ),
        session(w.rita),
        Some(json!({"body": "on !2"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{foreign}");
    let same_project = foreign["id"].as_i64().expect("id");
    // A thread of another project (rows written behind the API).
    let elsewhere = projects::create(
        &w.t.db,
        &ctx("req-elsewhere"),
        &NewProject::new("elsewhere", w.dave_ns, "Elsewhere", w.dave),
    )
    .await
    .expect("project");
    let e = elsewhere.project_id.get();
    w.t.commit_audited(
        ("project", e),
        &[
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch) VALUES ({e}, 1, 'x', {}, 'refs/heads/a', \
                 'refs/heads/main')",
                w.dave.get()
            ),
            format!(
                "INSERT INTO forge.merge_request_discussions (merge_request_id, project_id, \
                 kind, started_by) SELECT id, project_id, 'general', {} \
                 FROM forge.merge_requests WHERE project_id = {e}",
                w.dave.get()
            ),
        ],
    )
    .await;
    let other_project =
        w.t.json_rows(&format!(
            "SELECT id FROM forge.merge_request_discussions WHERE project_id = {e}"
        ))
        .await[0]["id"]
            .as_i64()
            .expect("id");
    let before = w.commented().await;
    for d in [same_project, other_project] {
        for (tail, body) in [
            (
                format!("/discussions/{d}/notes"),
                Some(json!({"body": "x"})),
            ),
            (format!("/discussions/{d}/resolve"), None),
            (format!("/discussions/{d}/unresolve"), None),
        ] {
            let (status, answer) = w.post(&tail, session(w.dave), body).await;
            assert_eq!(
                (status, code(&answer)),
                (StatusCode::NOT_FOUND, "FG-REPO-0005"),
                "{tail}"
            );
        }
    }
    assert_eq!(w.commented().await, before, "nothing written");
    w.t.drop_now().await;
}

/// Review round 1: a service account's token comments but is refused the
/// resolution (403); resolving twice answers 200 with one event; a closed
/// or merged merge request still takes comments and resolutions
/// (resolution is bookkeeping; what blocks a merge is evaluate_merge()).
#[tokio::test]
async fn service_accounts_repeats_and_finished_merge_requests() {
    let w = world().await;
    let (status, thread) = w
        .post(
            "/discussions",
            service_account(w.svc),
            Some(json!({"body": "ci says"})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{thread}");
    let d = thread["id"].as_i64().expect("id");
    let resolve = format!("/discussions/{d}/resolve");
    let (status, answer) = w.post(&resolve, service_account(w.svc), None).await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let (status, _) = w.post(&resolve, session(w.bob), None).await;
    assert_eq!(status, StatusCode::OK);
    let after_first = w.commented().await;
    let (status, again) = w.post(&resolve, session(w.bob), None).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["resolved"], json!(true));
    assert_eq!(w.commented().await, after_first, "a repeat writes nothing");
    for state in ["closed", "merged"] {
        w.set_state(state).await;
        let (status, answer) = w
            .post(
                "/discussions",
                session(w.rita),
                Some(json!({"body": state})),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{state}: {answer}");
        let id = answer["id"].as_i64().expect("id");
        let (status, answer) = w
            .post(&format!("/discussions/{id}/resolve"), session(w.rita), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{state}: {answer}");
        if state == "closed" {
            w.set_state("open").await;
        }
    }
    w.t.drop_now().await;
}

/// Review round 1: hunk boundaries over HTTP (version 1 rewrites line 10
/// of 20, so its hunk is lines 7..=13 on both sides): the last line is in,
/// the next is FG-REVIEW-0004; the old side's removed line is in.
#[tokio::test]
async fn hunk_boundaries_over_http() {
    let w = world().await;
    let at = |line: i64, side: &str| json!({"body": "b", "position": {"version": 1, "path": "lib.rs", "side": side, "line": line}});
    for (body, expect) in [
        (at(7, "new"), StatusCode::CREATED),
        (at(13, "new"), StatusCode::CREATED),
        (at(14, "new"), StatusCode::UNPROCESSABLE_ENTITY),
        (at(6, "new"), StatusCode::UNPROCESSABLE_ENTITY),
        (at(10, "old"), StatusCode::CREATED),
        (at(14, "old"), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let (status, answer) = w
            .post("/discussions", session(w.bob), Some(body.clone()))
            .await;
        assert_eq!(status, expect, "{body} {answer}");
    }
    w.t.drop_now().await;
}
