#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The context API's database-backed routes of M3 X5 (plan 03 §6, ADR-0023
//! Amendment X5) through the product router, against the development
//! database and a real bare repository: `GET …/context/work-item?ref=` and
//! `GET …/merge-requests/{iid}/context`. A member reads with provenance on
//! every item; someone who cannot see the project gets the same 404 as a
//! missing one; an unbound project is FG-INT-0018; a work item kanflow
//! marked confidential is refused to an AI agent (FG-AUTH-0021) and left
//! out of a merge request's context for it, while a person sees it (plan 06
//! D-5). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::ids::MergeRequestId;
use forge_core::{IntegrationKind, RequestId, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::integrations::{self, NewIntegration};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_hosting::Hosting;
use forge_review::{Review, VersionRefs};
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
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

/// The same person's token issued for an AI agent.
fn agent(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::Human,
    )
    .as_agent(true)
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

/// A commit of `files` (paths with at most one directory) on `parent`.
fn commit_on(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    use std::collections::BTreeMap;
    let mut dirs: BTreeMap<&str, String> = BTreeMap::new();
    let mut top = String::new();
    for (path, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        match path.split_once('/') {
            Some((dir, name)) => dirs
                .entry(dir)
                .or_default()
                .push_str(&format!("100644 blob {blob}\t{name}\n")),
            None => top.push_str(&format!("100644 blob {blob}\t{path}\n")),
        }
    }
    for (dir, listing) in dirs {
        let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
        top.push_str(&format!("040000 tree {tree}\t{dir}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(top.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

const OPEN: &str = "---\nspec: web/core\ntitle: Core\nstatus: approved\n---\n";
const SECRET: &str =
    "---\nspec: web/secret\ntitle: Secret\nstatus: approved\nconfidential: true\n---\n";

struct World {
    t: FreshDb,
    _home: tempfile::TempDir,
    _root: tempfile::TempDir,
    router: axum::Router,
    dev: UserId,
    outsider: UserId,
    project: i64,
    unbound: i64,
    base: String,
    head: String,
}

/// dave owns `web` (bound to kanflow `kf-1`) and `api` (not bound); olga
/// is no member. `main` has the spec and a test of PBI-43/AC-1; merge
/// request !1 from `feature` adds WEB-CORE-2, the confidential WEB-SEC-1
/// and a file, and is linked to PBI-43 (detected) and PBI-9 (by hand).
async fn world() -> World {
    let t = FreshDb::migrated().await;
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
    let api = projects::create(
        &t.db,
        &ctx("req-api"),
        &NewProject::new("api", dev.namespace_id, "Api", dev.user_id),
    )
    .await
    .expect("api");
    for p in [&web, &api] {
        let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), p.repository_id)
            .await
            .expect("ready");
    }
    let home = tempfile::tempdir().expect("home");
    let root = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let storage = StorageRoot::open(root.path()).expect("root");
    let path = storage
        .init_bare(&git, web.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init");
    let _api_path = storage
        .init_bare(&git, api.repository_id, uuid::Uuid::new_v4(), None)
        .await
        .expect("init api");
    let spec_v1 = format!("{OPEN}::: req WEB-CORE-1\nlog in\n:::\n");
    let base = commit_on(
        &path,
        None,
        &[
            ("spec/core.md", &spec_v1),
            (
                "tests/login.rs",
                "// @req PBI-43/AC-1\n// @req WEB-CORE-1\n",
            ),
        ],
    );
    let spec_v2 = format!("{spec_v1}\n::: req WEB-CORE-2\nlog out\n:::\n");
    let secret = format!("{SECRET}::: req WEB-SEC-1\nthe secret rule\n:::\n");
    let head = commit_on(
        &path,
        Some(&base),
        &[
            ("spec/core.md", &spec_v2),
            ("spec/secret.md", &secret),
            ("src/logout.rs", "fn logout() {}\n"),
            (
                "tests/login.rs",
                "// @req PBI-43/AC-1\n// @req WEB-CORE-1\n",
            ),
        ],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    plumb(&path, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
    // clone: the router owns its handles
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let refs = VersionRefs::new(git.clone(), storage.clone()); // clone: as above
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
    .with_hosting(Hosting::new(t.db.clone()))
    .with_checks(forge_checks::Checks::new(
        t.db.clone(),
        Arc::new(browser.clone()), // clone: a shared handle
    ))
    .with_review(Review::new(t.db.clone(), Arc::new(browser)).with_version_refs(refs.clone()));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let sa = service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: dev.user_id,
            purpose: "Kanflow reads the event feed".to_owned(),
        },
        dev.user_id,
    )
    .await
    .expect("service account")
    .user_id;
    let integration = integrations::create(
        &t.db,
        &ctx("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        dev.user_id,
    )
    .await
    .expect("integration");
    let p = web.project_id.get();
    let (status, bound) = send(
        router.clone(),
        "PUT",
        &format!(
            "/api/v1/projects/{p}/integration-bindings/{}",
            integration.get()
        ),
        session(dev.user_id),
        Some(json!({"external_project_id": "kf-1"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bound}");
    let (status, opened) = send(
        router.clone(),
        "POST",
        &format!("/api/v1/projects/{p}/merge-requests"),
        session(dev.user_id),
        Some(json!({
            "title": "Log out (PBI-43)",
            "description": "Adds logging out.",
            "source_branch": "feature",
            "target_branch": "main"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{opened}");
    let (status, linked) = send(
        router.clone(),
        "PUT",
        &format!("/api/v1/projects/{p}/merge-requests/1/work-items/PBI-9"),
        session(dev.user_id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    let id: i64 = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    // The version job, as the worker runs it.
    forge_review::refresh(
        &t.db,
        &refs,
        forge_review::RefreshRequest {
            merge_request: MergeRequestId::new(id),
            pushed_by: None,
        },
        &RequestId::parse("req-v1").expect("id"),
    )
    .await
    .expect("refresh");
    // The work-item-sync subscriber links PBI-43 from the title.
    drain(&t).await;
    // A check run on the head (a fixture: no reporter command here).
    t.commit_audited(
        ("project", p),
        &[format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
             summary, reported_by, request_id, completed_at) VALUES ({p}, '{head}', 'ci/test', \
             'completed', 'success', 'all green', {}, 'req-check', now())",
            dev.user_id.get()
        )],
    )
    .await;
    World {
        t,
        _home: home,
        _root: root,
        router,
        dev: dev.user_id,
        outsider: outsider.user_id,
        project: p,
        unbound: api.project_id.get(),
        base,
        head,
    }
}

/// Number the events, then run work-item-sync from the first event until
/// it caught up.
async fn drain(t: &FreshDb) {
    use forge_db::sequencer::{self, AuditSequencing, BatchSize};
    use forge_db::subscribers::{self, Progress, Start};
    use forge_db::work_items::WorkItemSync;
    subscribers::register(&t.db, &WorkItemSync, Start::Beginning)
        .await
        .expect("register");
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
    loop {
        match subscribers::run_once(&t.db, &WorkItemSync)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
        sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
            .await
            .expect("sequencer");
    }
}

impl World {
    async fn get(&self, uri: &str, who: Principal) -> (StatusCode, Value) {
        send(self.router.clone(), "GET", uri, who, None).await
    }

    fn work_item(&self, reference: &str) -> String {
        format!(
            "/api/v1/projects/{}/context/work-item?ref={reference}",
            self.project
        )
    }

    fn mr(&self) -> String {
        format!("/api/v1/projects/{}/merge-requests/1/context", self.project)
    }

    async fn mark_confidential(&self, reference: &str) {
        self.t
            .exec_superuser(&format!(
                "UPDATE forge.work_items SET confidential = true WHERE ref = '{reference}'"
            ))
            .await;
    }
}

#[tokio::test]
async fn work_item_context_lists_linked_merge_requests_and_criterion_tests() {
    let w = world().await;
    let (status, body) = w.get(&w.work_item("PBI-43"), session(w.dev)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["work_item"]["ref"], "PBI-43");
    assert_eq!(body["work_item"]["kanflow_project_id"], "kf-1");
    assert_eq!(body["work_item"]["known"], true);
    let mrs = body["merge_requests"].as_array().expect("merge requests");
    assert_eq!(mrs.len(), 1, "{body}");
    assert_eq!(mrs[0]["iid"], 1);
    assert_eq!(mrs[0]["link_source"], "auto");
    assert_eq!(mrs[0]["source_branch"], "feature");
    assert_eq!(mrs[0]["source"]["project_id"], w.project);
    assert_eq!(mrs[0]["source"]["project"], "dave/web");
    assert_eq!(mrs[0]["source"]["commit"], w.head.as_str());
    assert_eq!(body["ref"]["name"], "main");
    assert_eq!(body["test_count"], 1);
    let test = &body["tests"][0];
    assert_eq!(test["target"], "PBI-43/AC-1");
    assert_eq!(test["source"]["path"], "tests/login.rs");
    assert_eq!(test["source"]["line"], 1);
    assert_eq!(test["source"]["commit"], w.base.as_str());
    // A reference forge never saw: known false, nothing linked.
    let (status, unseen) = w.get(&w.work_item("PBI-77"), session(w.dev)).await;
    assert_eq!(status, StatusCode::OK, "{unseen}");
    assert_eq!(unseen["work_item"]["known"], false);
    assert_eq!(unseen["merge_requests"], json!([]));
    // Not canonical: refused before anything is read.
    let (status, bad) = w.get(&w.work_item("PBI-043"), session(w.dev)).await;
    assert_eq!(
        (status, bad["error_code"].clone()),
        (StatusCode::BAD_REQUEST, json!("FG-WEB-0006"))
    );
    // An unbound project.
    let (status, unbound) = w
        .get(
            &format!(
                "/api/v1/projects/{}/context/work-item?ref=PBI-43",
                w.unbound
            ),
            session(w.dev),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{unbound}");
    assert_eq!(unbound["error_code"], "FG-INT-0018");
    // Someone who cannot see the project: the same 404 as a missing one.
    let (status, hidden) = w.get(&w.work_item("PBI-43"), session(w.outsider)).await;
    let (missing_status, missing) = w
        .get(
            "/api/v1/projects/999999/context/work-item?ref=PBI-43",
            session(w.outsider),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");
    assert_eq!(
        (status, &hidden["error_code"]),
        (missing_status, &missing["error_code"])
    );
}

#[tokio::test]
async fn merge_request_context_carries_files_requirements_work_items_and_checks() {
    let w = world().await;
    let (status, body) = w.get(&w.mr(), session(w.dev)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mr = &body["merge_request"];
    assert_eq!(mr["iid"], 1);
    assert_eq!(mr["title"], "Log out (PBI-43)");
    assert_eq!(mr["description"], "Adds logging out.");
    assert_eq!(mr["merge_base_sha"], w.base.as_str());
    assert_eq!(mr["source"]["commit"], w.head.as_str());
    let files: Vec<&str> = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["source"]["path"].as_str().expect("path"))
        .collect();
    assert_eq!(files, ["spec/core.md", "spec/secret.md", "src/logout.rs"]);
    let changes: Vec<(&str, &str)> = body["requirements"]
        .as_array()
        .expect("requirements")
        .iter()
        .map(|c| {
            (
                c["id"].as_str().expect("id"),
                c["kind"].as_str().expect("kind"),
            )
        })
        .collect();
    assert_eq!(changes, [("WEB-CORE-2", "added"), ("WEB-SEC-1", "added")]);
    assert_eq!(body["requirements"][0]["body"], "log out");
    assert_eq!(body["requirements"][0]["source"]["path"], "spec/core.md");
    assert_eq!(body["requirements"][0]["source"]["commit"], w.head.as_str());
    let items: Vec<&str> = body["work_items"]
        .as_array()
        .expect("work items")
        .iter()
        .map(|i| i["ref"].as_str().expect("ref"))
        .collect();
    assert_eq!(items, ["PBI-9", "PBI-43"]);
    assert_eq!(body["checks"][0]["name"], "ci/test");
    assert_eq!(body["checks"][0]["conclusion"], "success");
    assert_eq!(body["checks"][0]["summary"], "all green");
    assert_eq!(body["checks"][0]["source"]["commit"], w.head.as_str());
    assert_eq!(body["withheld"], Value::Null);
    // Someone who cannot see the project.
    let (status, hidden) = w.get(&w.mr(), session(w.outsider)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden}");
}

/// Plan 06 D-5: a work item kanflow marked confidential is refused to an
/// AI agent, and left out of a merge request's context for it (with the
/// confidential requirement), counted in `withheld`; a person sees both.
#[tokio::test]
async fn a_confidential_work_item_is_withheld_from_an_agent() {
    let w = world().await;
    w.mark_confidential("PBI-43").await;
    let (status, refused) = w.get(&w.work_item("PBI-43"), agent(w.dev)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error_code"], "FG-AUTH-0021");
    let (status, read) = w.get(&w.work_item("PBI-43"), session(w.dev)).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    // The agent's merge request context.
    let (status, body) = w.get(&w.mr(), agent(w.dev)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items: Vec<&str> = body["work_items"]
        .as_array()
        .expect("work items")
        .iter()
        .map(|i| i["ref"].as_str().expect("ref"))
        .collect();
    assert_eq!(items, ["PBI-9"]);
    let changes: Vec<&str> = body["requirements"]
        .as_array()
        .expect("requirements")
        .iter()
        .map(|c| c["id"].as_str().expect("id"))
        .collect();
    assert_eq!(changes, ["WEB-CORE-2"]);
    assert_eq!(body["withheld"]["error_code"], "FG-AUTH-0021");
    assert_eq!(body["withheld"]["work_items"], 1);
    assert_eq!(body["withheld"]["requirements"], 1);
    assert_eq!(body["withheld"]["documents"], 1);
    // Review X5 (MEDIUM b): the confidential spec file is no changed file
    // for an agent either.
    let files: Vec<&str> = body["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["source"]["path"].as_str().expect("path"))
        .collect();
    assert_eq!(files, ["spec/core.md", "src/logout.rs"]);
    assert_eq!(body["withheld"]["files"], 1);
    assert!(!body.to_string().contains("the secret rule"));
    // The merge request's version diff gives an agent no patch of it.
    let (status, diff) = w
        .get(
            &format!(
                "/api/v1/projects/{}/merge-requests/1/versions/1/diff",
                w.project
            ),
            agent(w.dev),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{diff}");
    assert!(!diff.to_string().contains("the secret rule"));
    assert_eq!(diff["diff"]["withheld"]["files"], 1);
    // A person sees the mark.
    let (_, person) = w.get(&w.mr(), session(w.dev)).await;
    let marked = person["work_items"]
        .as_array()
        .expect("work items")
        .iter()
        .find(|i| i["ref"] == "PBI-43")
        .expect("listed for a person")
        .clone();
    assert_eq!(marked["confidential"], true);
}
