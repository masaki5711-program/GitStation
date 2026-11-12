#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The simple editor's commit (E1, 2026-11-12) through the product router,
//! against the development database and a real bare repository:
//! `POST /api/v1/projects/{id}/repository/commits/single-file` commits one
//! file like a push — the project-level `RepoPush`, the ref decided by the
//! push's protection rules, the secret scan, the compare-and-swap, the
//! push record (audit row and event) — on a new branch with a merge request
//! or directly on the branch. Synthetic data only.

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
use forge_git::{Git, GitConfig, StorageRoot};
use forge_review::Review;
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

#[derive(Clone)]
struct Caller {
    principal: Principal,
    session: Option<i64>,
}

fn token(user: UserId, scopes: &[Scope]) -> Caller {
    Caller {
        principal: Principal::authenticated(
            user,
            false,
            true,
            Credential::Token(scopes.to_vec()),
            forge_auth::AccountKind::Human,
        ),
        session: None,
    }
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

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
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

/// A commit of `files` (nested paths allowed: built in a temporary index).
fn commit_on(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut listing = String::new();
    for (name, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        listing.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let index = tempfile::tempdir().expect("index dir");
    let index = index.path().join("index");
    let with_index = |args: &[&str], input: Option<&[u8]>| {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(repo)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
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
    };
    with_index(
        &["update-index", "--add", "--index-info"],
        Some(listing.as_bytes()),
    );
    let tree = with_index(&["write-tree"], None);
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
    dev: Caller,
    reporter: Caller,
    outsider: Caller,
    read_only: Caller,
    project: i64,
    base: String,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut ids = Vec::new();
    for name in ["olga", "dave", "rita", "otto"] {
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
    for (who, role) in [
        (ids[1].user_id, Role::Developer),
        (ids[2].user_id, Role::Reporter),
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
    let base = commit_on(
        &path,
        None,
        &[("README.md", "# Web\n"), ("docs/spec.md", "# Spec\n\nv1\n")],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
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
    .with_review(Review::new(t.db.clone(), Arc::new(browser.clone())))
    // The merge-rule route reads the check reporters (as `forge serve`).
    .with_checks(forge_checks::Checks::new(t.db.clone(), Arc::new(browser)));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    World {
        dev: token(ids[1].user_id, &[Scope::WriteRepository, Scope::Api]),
        reporter: token(ids[2].user_id, &[Scope::WriteRepository, Scope::Api]),
        outsider: token(ids[3].user_id, &[Scope::WriteRepository, Scope::Api]),
        read_only: token(ids[1].user_id, &[Scope::ReadRepository, Scope::Api]),
        t,
        _home: home,
        _root: root,
        path,
        router,
        project: web.project_id.get(),
        base,
    }
}

impl World {
    fn uri(&self) -> String {
        format!(
            "/api/v1/projects/{}/repository/commits/single-file",
            self.project
        )
    }

    async fn commit(&self, who: &Caller, body: Value) -> (StatusCode, Value) {
        send(
            self.router.clone(),
            "POST",
            &self.uri(),
            who.clone(),
            Some(body),
        )
        .await
    }

    fn tip(&self, branch: &str) -> String {
        plumb(
            &self.path,
            &["rev-parse", &format!("refs/heads/{branch}")],
            None,
        )
    }

    fn show(&self, rev: &str, path: &str) -> String {
        plumb(&self.path, &["show", &format!("{rev}:{path}")], None)
    }

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

    /// Protect main (`push_min_role`), optionally banning direct pushes.
    async fn protect_main(&self, role: &str, ban: bool) {
        let olga = self.owner().await;
        let (status, created) = send(
            self.router.clone(),
            "POST",
            &format!("/api/v1/projects/{}/protected-refs", self.project),
            olga.clone(),
            Some(json!({"pattern": "refs/heads/main", "push_min_role": role})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        if ban {
            let rule = created["id"].as_i64().expect("rule id");
            let (status, body) = send(
                self.router.clone(),
                "PUT",
                &format!(
                    "/api/v1/projects/{}/protected-refs/{rule}/merge-rule",
                    self.project
                ),
                olga,
                Some(json!({
                    "direct_push_forbidden": true,
                    "require_work_item_link": false,
                    "expected_updated_at": null
                })),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
    }

    async fn pushed_rows(&self) -> Vec<Value> {
        self.t
            .json_rows(
                "SELECT request_id, details FROM forge.audit_log \
                 WHERE action = 'repository.pushed' ORDER BY id",
            )
            .await
    }
}

fn edit(base: &str, content: &str) -> Value {
    json!({
        "branch": "main",
        "start_sha": base,
        "path": "docs/spec.md",
        "content": content,
        "message": "Update the spec\n\nfrom the editor",
    })
}

/// A direct commit on an unprotected branch moves it by one commit on the
/// head, records the push (audit row, event, introduced commit) and is
/// what the browse API shows next.
#[tokio::test]
async fn a_direct_commit_moves_the_branch_and_is_recorded_like_a_push() {
    let w = world().await;
    let (status, body) = w.commit(&w.dev, edit(&w.base, "# Spec\n\nv2\n")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sha = body["commit_sha"].as_str().expect("sha");
    assert_eq!(body["parent_sha"], w.base.as_str());
    assert_eq!(body["branch"], "main");
    assert_eq!(body["branch_created"], false);
    assert!(body["merge_request"].is_null(), "{body}");
    assert_eq!(w.tip("main"), sha);
    assert_eq!(w.show(sha, "docs/spec.md"), "# Spec\n\nv2");
    assert_eq!(w.show(sha, "README.md"), "# Web");
    let raw = plumb(&w.path, &["cat-file", "commit", sha], None);
    assert!(raw.contains("author dave <dave@example.test>"), "{raw}");
    let rows = w.pushed_rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let refs = &rows[0]["details"]["refs"];
    assert_eq!(refs[0]["ref"], "refs/heads/main", "{rows:?}");
    assert_eq!(refs[0]["kind"], "fast_forward", "{rows:?}");
    let events = w
        .t
        .json_rows("SELECT count(*) AS n FROM forge.events WHERE event_type = 'repository.pushed'")
        .await;
    assert_eq!(events[0]["n"], 1);
    let introduced =
        w.t.json_rows("SELECT count(*) AS n FROM forge.push_commits")
            .await;
    assert_eq!(introduced[0]["n"], 1);
    // The browse cache was dropped: the blob API reads the new content.
    let (status, blob) = send(
        w.router.clone(),
        "GET",
        &format!(
            "/api/v1/projects/{}/repository/blob?ref=main&path=docs/spec.md",
            w.project
        ),
        w.dev.clone(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{blob}");
    assert_eq!(blob["content"], "# Spec\n\nv2\n");
    w.t.drop_now().await;
}

/// The default flow: a new branch from the head and a merge request into
/// the branch it started from; main does not move.
#[tokio::test]
async fn a_new_branch_gets_the_commit_and_a_merge_request() {
    let w = world().await;
    let mut body = edit(&w.base, "# Spec\n\nv2\n");
    body["create_branch"] = json!("edit/spec");
    body["path"] = json!("docs/new/guide.md");
    let (status, answer) = w.commit(&w.dev, body).await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(answer["branch"], "edit/spec");
    assert_eq!(answer["branch_created"], true);
    assert!(answer["merge_request_error"].is_null(), "{answer}");
    assert_eq!(answer["merge_request"]["source_branch"], "edit/spec");
    assert_eq!(answer["merge_request"]["target_branch"], "main");
    assert_eq!(answer["merge_request"]["title"], "Update the spec");
    assert_eq!(w.tip("main"), w.base);
    let sha = answer["commit_sha"].as_str().expect("sha");
    assert_eq!(w.tip("edit/spec"), sha);
    assert_eq!(w.show(sha, "docs/new/guide.md"), "# Spec\n\nv2");
    let rows = w.pushed_rows().await;
    assert_eq!(rows[0]["details"]["refs"][0]["kind"], "create", "{rows:?}");
    // The same name again: the existing branch is never moved.
    let mut again = edit(&w.base, "# Spec\n\nv3\n");
    again["create_branch"] = json!("edit/spec");
    let (status, refused) = w.commit(&w.dev, again).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::CONFLICT, "FG-GIT-0027"),
        "{refused}"
    );
    assert_eq!(w.tip("edit/spec"), sha);
    w.t.drop_now().await;
}

/// No silent overwrite: the file changed since start_sha is a conflict;
/// another file changed is not (the commit is built on the new head).
#[tokio::test]
async fn a_file_changed_since_the_start_is_a_conflict() {
    let w = world().await;
    let theirs = commit_on(
        &w.path,
        Some(&w.base),
        &[
            ("README.md", "# Web\n"),
            ("docs/spec.md", "# Spec\n\ntheirs\n"),
        ],
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &theirs], None);
    let (status, body) = w.commit(&w.dev, edit(&w.base, "# Spec\n\nmine\n")).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-GIT-0027"),
        "{body}"
    );
    assert_eq!(w.tip("main"), theirs);
    // Another file moved the branch: fine, built on the new head.
    let other = commit_on(
        &w.path,
        Some(&theirs),
        &[
            ("README.md", "# Web 2\n"),
            ("docs/spec.md", "# Spec\n\ntheirs\n"),
        ],
    );
    plumb(&w.path, &["update-ref", "refs/heads/main", &other], None);
    let (status, body) = w.commit(&w.dev, edit(&theirs, "# Spec\n\nmine\n")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["parent_sha"], other.as_str());
    let sha = body["commit_sha"].as_str().expect("sha");
    assert_eq!(w.show(sha, "README.md"), "# Web 2");
    // An unknown start commit.
    let (status, body) = w
        .commit(&w.dev, edit(&"e".repeat(40), "# Spec\n\nx\n"))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "FG-GIT-0027"),
        "{body}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_same_content_is_unchanged() {
    let w = world().await;
    let (status, body) = w.commit(&w.dev, edit(&w.base, "# Spec\n\nv1\n")).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "FG-GIT-0031"),
        "{body}"
    );
    assert_eq!(w.tip("main"), w.base);
    assert!(w.pushed_rows().await.is_empty());
    w.t.drop_now().await;
}

/// The push rules decide the ref: a direct-push ban (FG-GIT-0017) and a
/// Maintainer-level protection (FG-GIT-0008) refuse a direct commit — the
/// new-branch route still works; a secret is FG-GIT-0022; bad paths and
/// contents have their codes. Nothing moves on a refusal.
#[tokio::test]
async fn the_push_rules_refuse_like_a_push() {
    let w = world().await;
    let (status, body) = w
        .commit(
            &w.dev,
            edit(&w.base, "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n"),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "FG-GIT-0022"),
        "{body}"
    );
    let mut bad = edit(&w.base, "x\n");
    bad["path"] = json!("../etc/passwd");
    let (status, body) = w.commit(&w.dev, bad).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "FG-GIT-0028"),
        "{body}"
    );
    let mut dir = edit(&w.base, "x\n");
    dir["path"] = json!("docs");
    let (status, body) = w.commit(&w.dev, dir).await;
    assert_eq!(code(&body), "FG-GIT-0028", "{status} {body}");
    let (status, body) = w.commit(&w.dev, edit(&w.base, "a\u{0}b")).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "FG-GIT-0030"),
        "{body}"
    );
    let big = "x".repeat(600 * 1024);
    let (status, body) = w.commit(&w.dev, edit(&w.base, &big)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::PAYLOAD_TOO_LARGE, "FG-GIT-0029"),
        "{body}"
    );
    let mut blank = edit(&w.base, "x\n");
    blank["message"] = json!("  ");
    let (status, body) = w.commit(&w.dev, blank).await;
    assert_eq!(code(&body), "FG-WEB-0006", "{status} {body}");

    w.protect_main("maintainer", false).await;
    let (status, body) = w.commit(&w.dev, edit(&w.base, "# Spec\n\nv2\n")).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-GIT-0008"),
        "{body}"
    );
    assert_eq!(w.tip("main"), w.base);
    w.t.drop_now().await;

    let w = world().await;
    w.protect_main("developer", true).await;
    let (status, body) = w.commit(&w.dev, edit(&w.base, "# Spec\n\nv2\n")).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-GIT-0017"),
        "{body}"
    );
    assert_eq!(w.tip("main"), w.base);
    let mut branch = edit(&w.base, "# Spec\n\nv2\n");
    branch["create_branch"] = json!("edit/ban");
    let (status, body) = w.commit(&w.dev, branch).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["merge_request"]["iid"].is_i64(), "{body}");
    assert!(w.pushed_rows().await.len() == 1);
    w.t.drop_now().await;
}

/// The project-level decision (RepoPush): a Reporter is refused, an
/// outsider sees nothing, a read-only token lacks the scope.
#[tokio::test]
async fn only_a_developer_with_write_repository_commits() {
    let w = world().await;
    for (who, status, expected) in [
        (&w.reporter, StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        (&w.outsider, StatusCode::NOT_FOUND, "FG-REPO-0005"),
        (&w.read_only, StatusCode::FORBIDDEN, "FG-AUTH-0002"),
    ] {
        let (got, body) = w.commit(who, edit(&w.base, "# Spec\n\nv2\n")).await;
        assert_eq!((got, code(&body)), (status, expected), "{body}");
    }
    assert_eq!(w.tip("main"), w.base);
    assert!(w.pushed_rows().await.is_empty());
    w.t.drop_now().await;
}
