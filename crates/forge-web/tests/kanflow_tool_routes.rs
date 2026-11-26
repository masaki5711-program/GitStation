#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Kanflow's read-only tool pack (plan 19 §3.3, L5-9, ADR-0025 Amendment
//! L5-4/L5-9) through the product router, against the development database
//! and a real bare repository: the pack, and each tool called by kanflow's
//! service account acting for the person who asked — answered with the
//! person's own permissions, `as_of` on every answer, what the person cannot
//! see the same 404, the blocking reasons exactly the merge decision's, and
//! a confidential file's CODEOWNERS pattern withheld from the tool. The
//! deployment environments, approvers and pipeline schedules stay refused
//! acting for a person. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::act_as::ActAsKeys;
use forge_core::{ObjectId, RefName, RequestId, Role, Scope, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{self as pdb, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter, Pipelines};
use forge_review::{Review, VersionRefs};
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const KEYS: &str = "1:BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY= active";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct OpenMarks;

impl MarkMeter for OpenMarks {
    fn measure<'a>(&'a self, _: forge_core::RepoId, _: &'a ObjectId) -> MarkFuture<'a> {
        Box::pin(async { CopyMark::Open })
    }
}

struct OnMain;

impl forge_deploy::CommitSource for OnMain {
    fn facts<'a>(
        &'a self,
        _: forge_core::ProjectId,
        _: &'a ObjectId,
    ) -> forge_deploy::FactsFuture<'a> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Clone)]
enum Who {
    Person(Principal),
    Bearer(String),
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

/// A commit of `files` (`dir/name` paths one level deep at most).
fn commit_on(repo: &std::path::Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let mut root = String::new();
    let mut dirs: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    for (name, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        match name.split_once('/') {
            Some((dir, leaf)) => dirs
                .entry(dir)
                .or_default()
                .push_str(&format!("100644 blob {blob}\t{leaf}\n")),
            None => root.push_str(&format!("100644 blob {blob}\t{name}\n")),
        }
    }
    for (dir, listing) in dirs {
        let tree = plumb(repo, &["mktree"], Some(listing.as_bytes()));
        root.push_str(&format!("040000 tree {tree}\t{dir}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(root.as_bytes()));
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

fn session_of(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

struct World {
    t: FreshDb,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
    router: axum::Router,
    olga: UserId,
    /// No role on the project.
    eve: UserId,
    kanflow: String,
    project: i64,
    pipeline: i64,
    job: i64,
}

async fn send(
    router: axum::Router,
    uri: &str,
    who: Who,
    act_as: Option<UserId>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Who::Bearer(token) = &who {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    if let Some(person) = act_as {
        req = req.header("forge-act-as", format!("user:{}", person.get()));
    }
    let mut req = req.body(Body::empty()).expect("request");
    if let Who::Person(p) = who {
        req.extensions_mut().insert(p);
    }
    let res = router.oneshot(req).await.expect("infallible");
    let status = res.status();
    let raw = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn code(body: &Value) -> &str {
    body["error_code"].as_str().unwrap_or("")
}

const CODEOWNERS: &str = "/secret/ @olga\n*.txt @olga\n";
const CONFIDENTIAL: &str = "---\nclassification: confidential\n---\nthe hidden plan\n";

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let mut olga_new = NewUser::human("olga", "Olga", "olga@example.test");
    olga_new.is_admin = true;
    let olga = users::create(&t.db, &ctx("req-owner"), &olga_new)
        .await
        .expect("owner");
    let dave = users::create(
        &t.db,
        &ctx("req-dev"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dev");
    let eve = users::create(
        &t.db,
        &ctx("req-eve"),
        &NewUser::human("eve", "eve", "eve@example.test"),
    )
    .await
    .expect("eve");
    let web = projects::create(
        &t.db,
        &ctx("req-web"),
        &NewProject::new("web", olga.namespace_id, "Web", olga.user_id),
    )
    .await
    .expect("web");
    projects::add_member(
        &t.db,
        &ctx("req-member"),
        web.project_id,
        dave.user_id,
        Role::Developer,
    )
    .await
    .expect("member");
    let _ready = repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
        .await
        .expect("ready");
    let sa = forge_db::service_accounts::create(
        &t.db,
        &ctx("req-sa"),
        &forge_db::service_accounts::NewServiceAccount {
            username: "kanflow-sync".to_owned(),
            display_name: "kanflow".to_owned(),
            owner: olga.user_id,
            purpose: "kanflow answers questions for the people who ask".to_owned(),
        },
        olga.user_id,
    )
    .await
    .expect("sa")
    .user_id;
    let integration = forge_db::integrations::create(
        &t.db,
        &ctx("req-int"),
        &forge_db::integrations::NewIntegration {
            kind: forge_core::IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        olga.user_id,
    )
    .await
    .expect("integration");
    let kanflow = forge_db::tokens::issue_for_service_account(
        &t.db,
        &ctx("req-token"),
        &forge_db::tokens::NewToken {
            user: sa,
            name: "kanflow".to_owned(),
            scopes: vec![Scope::ReadApi, Scope::ActAs],
            expires_in: None,
            is_agent: false,
        },
        olga.user_id,
    )
    .await
    .expect("token")
    .token
    .reveal();
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
        &[("CODEOWNERS", CODEOWNERS), ("README", "base\n")],
    );
    let head = commit_on(
        &path,
        Some(&base),
        &[
            ("CODEOWNERS", CODEOWNERS),
            ("README", "base\n"),
            ("feature.txt", "f\n"),
            ("secret/plan.md", CONFIDENTIAL),
        ],
    );
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &head], None);
    let browser = Browser::new(git.clone(), storage.clone(), BrowseLimits::default());
    let smart = SmartHttp::new(git.clone(), storage.clone(), GitHttpLimits::default());
    let refs = VersionRefs::new(git.clone(), storage.clone());
    let state = AppState::new(
        "test",
        Vec::new(),
        Arc::new(t.db.clone()),
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
    .with_pipelines(Pipelines::new(
        t.db.clone(),
        browser.clone(),
        Arc::new(OpenMarks),
    ))
    .with_deploy(forge_deploy::Deploy::new(
        t.db.clone(),
        Arc::new(OnMain),
        forge_core::deploy::TargetPolicy::default(),
    ))
    .with_act_as(forge_integ::ActAs::new(
        t.db.clone(),
        Some(ActAsKeys::parse(KEYS).expect("keys")),
    ));
    let router = forge_web::app(state, AppConfig::default()).expect("router");
    let p = web.project_id.get();
    let bind = Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/projects/{p}/integration-bindings/{}",
            integration.get()
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"external_project_id": "KF-1"}).to_string(),
        ))
        .map(|mut r| {
            r.extensions_mut().insert(session_of(olga.user_id));
            r
        })
        .expect("bind");
    let bound = router.clone().oneshot(bind).await.expect("infallible");
    assert!(bound.status().is_success(), "{}", bound.status());
    let open = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/projects/{p}/merge-requests"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"title": "Feature PBI-12", "source_branch": "feature", "target_branch": "main"})
                .to_string(),
        ))
        .map(|mut r| {
            r.extensions_mut().insert(session_of(dave.user_id));
            r
        })
        .expect("open");
    let opened = router.clone().oneshot(open).await.expect("infallible");
    assert_eq!(opened.status(), StatusCode::CREATED);
    // The work-item-sync subscriber does not run here: link by hand.
    let link = Request::builder()
        .method("PUT")
        .uri(format!(
            "/api/v1/projects/{p}/merge-requests/1/work-items/PBI-12"
        ))
        .body(Body::empty())
        .map(|mut r| {
            r.extensions_mut().insert(session_of(dave.user_id));
            r
        })
        .expect("link");
    let linked = router.clone().oneshot(link).await.expect("infallible");
    assert_eq!(linked.status(), StatusCode::OK);
    let mr: i64 = t
        .json_rows("SELECT id FROM forge.merge_requests")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("merge request id");
    forge_review::refresh(
        &t.db,
        &refs,
        forge_review::RefreshRequest {
            merge_request: forge_core::MergeRequestId::new(mr),
            pushed_by: None,
        },
        &RequestId::parse("req-worker").expect("id"),
    )
    .await
    .expect("refresh");
    let pipeline = pdb::create(
        &t.db,
        &NewPipeline {
            ai_eval_job: None,
            request: PipelineRequest {
                project: web.project_id,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&base).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: Some(dave.user_id),
            },
            request_id: "req-seed".to_owned(),
            definition_hash: Some(pdb::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![NewCiJob {
                name: "build".to_owned(),
                stage: "build".to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                timeout_s: 600,
                tags: Vec::new(),
                secrets: Vec::new(),
                manual: false,
            }]),
        },
    )
    .await
    .expect("pipeline")
    .pipeline_id;
    let job: i64 = t
        .json_rows("SELECT id FROM forge.ci_jobs")
        .await
        .first()
        .and_then(|r| r["id"].as_i64())
        .expect("job id");
    World {
        t,
        _dirs: (home, root),
        router,
        olga: olga.user_id,
        eve: eve.user_id,
        kanflow,
        project: p,
        pipeline,
        job,
    }
}

impl World {
    fn kanflow(&self) -> Who {
        Who::Bearer(self.kanflow.clone())
    }

    fn uri(&self, tail: &str) -> String {
        format!("/api/v1/projects/{}/{tail}", self.project)
    }

    /// Every tool's route of the world, as kanflow calls it.
    fn tool_uris(&self) -> Vec<String> {
        vec![
            self.uri("merge-requests/1/status"),
            self.uri("merge-requests?state=all"),
            self.uri("merge-requests?work_item=PBI-12&state=all"),
            self.uri("pipelines"),
            self.uri(&format!("pipelines/{}", self.pipeline)),
            self.uri("deployments"),
        ]
    }
}

fn is_rfc3339(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() == 20 && s.ends_with('Z') && s.as_bytes()[10] == b'T')
}

/// The pack lists every tool for kanflow's own token, and is refused
/// acting for a person (it is the integration's, not a person's, read).
#[tokio::test]
async fn the_pack_lists_read_only_tools_for_the_service_account() {
    let w = world().await;
    let (status, pack) = send(w.router.clone(), "/api/v1/ai-tools", w.kanflow(), None).await;
    assert_eq!(status, StatusCode::OK, "{pack}");
    assert_eq!(pack["pack"], "gitstation");
    let tools = pack["tools"].as_array().expect("tools");
    assert!(tools.len() >= 5);
    assert!(tools
        .iter()
        .all(|t| t["read_only"] == true && t["method"] == "GET"));
    assert!(pack["terms"]
        .as_array()
        .expect("terms")
        .iter()
        .any(|t| t["name"] == "approvals_missing" && t["ja"].is_string()));
    let (status, body) = send(
        w.router.clone(),
        "/api/v1/ai-tools",
        w.kanflow(),
        Some(w.olga),
    )
    .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{body}"
    );
    w.t.drop_now().await;
}

/// Every tool answers acting for a person who sees the project, with
/// `as_of`.
#[tokio::test]
async fn every_tool_answers_acting_for_the_person_with_as_of() {
    let w = world().await;
    for uri in w.tool_uris() {
        let (status, body) = send(w.router.clone(), &uri, w.kanflow(), Some(w.olga)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(is_rfc3339(&body["as_of"]), "{uri}: {body}");
    }
    // The PBI's merge requests are the one linked to it (by its title).
    let (_, linked) = send(
        w.router.clone(),
        &w.uri("merge-requests?work_item=PBI-12&state=all"),
        w.kanflow(),
        Some(w.olga),
    )
    .await;
    let iids: Vec<i64> = linked["merge_requests"]
        .as_array()
        .expect("list")
        .iter()
        .filter_map(|m| m["iid"].as_i64())
        .collect();
    assert_eq!(iids, vec![1], "{linked}");
    let (_, none) = send(
        w.router.clone(),
        &w.uri("merge-requests?work_item=PBI-99&state=all"),
        w.kanflow(),
        Some(w.olga),
    )
    .await;
    assert_eq!(none["merge_requests"], json!([]), "{none}");
    w.t.drop_now().await;
}

/// Plan 19 §3.3: what the asking person cannot see is the invisible
/// project's 404 in every tool, never the service account's answer.
#[tokio::test]
async fn tool_answer_hides_invisible_project() {
    let w = world().await;
    for uri in w.tool_uris() {
        let (status, body) = send(w.router.clone(), &uri, w.kanflow(), Some(w.eve)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{uri}: {body}"
        );
        assert!(body.get("as_of").is_none(), "{uri}: {body}");
    }
    w.t.drop_now().await;
}

/// Plan 19 §3.3: "why is it stopped" is `evaluate_merge()`'s list as it
/// is — the same reasons, in the same order, as the person's own read.
#[tokio::test]
async fn blocked_reasons_equal_evaluate_merge() {
    let w = world().await;
    let uri = w.uri("merge-requests/1/status");
    let (status, own) = send(
        w.router.clone(),
        &uri,
        Who::Person(session_of(w.olga)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{own}");
    let (status, tool) = send(w.router.clone(), &uri, w.kanflow(), Some(w.olga)).await;
    assert_eq!(status, StatusCode::OK, "{tool}");
    let names = |v: &Value| -> Vec<Value> {
        v["reasons"]
            .as_array()
            .expect("reasons")
            .iter()
            .map(|r| r["reason"].clone())
            .collect()
    };
    assert!(!names(&own).is_empty(), "{own}");
    assert_eq!(names(&tool), names(&own));
    assert_eq!(tool["mergeable"], own["mergeable"]);
    w.t.drop_now().await;
}

/// Plan 19 §5 (ADR-0039 decision 6): the tool acting for a person is an
/// agent audience — a CODEOWNERS pattern naming a confidential file is
/// withheld (approvals and reason detail), the owners and other patterns
/// stay; the person's own read shows it.
#[tokio::test]
async fn confidential_path_names_not_in_tool_answer_for_agent() {
    let w = world().await;
    let uri = w.uri("merge-requests/1/status");
    let (_, own) = send(
        w.router.clone(),
        &uri,
        Who::Person(session_of(w.olga)),
        None,
    )
    .await;
    assert!(own.to_string().contains("/secret/"), "{own}");
    let (status, tool) = send(w.router.clone(), &uri, w.kanflow(), Some(w.olga)).await;
    assert_eq!(status, StatusCode::OK, "{tool}");
    let text = tool.to_string();
    assert!(!text.contains("secret/"), "{tool}");
    assert!(text.contains("*.txt"), "{tool}");
    let codeowners = tool["approvals"]["codeowners"]
        .as_array()
        .expect("codeowners");
    assert!(
        codeowners.iter().any(|c| c["withheld"] == true
            && c["pattern"] == ""
            && c["owners"] == json!(["@olga"])),
        "{tool}"
    );
    w.t.drop_now().await;
}

/// ADR-0025 Amendment L5-4 (L5-9): only the pipeline list / one pipeline
/// and the deployment list load the act-as facts; the environments (host
/// keys), the approvers (people), the pipeline schedules, the CI output
/// (log, artifacts) and the diagnosis stay `act_as_facts_missing`.
#[tokio::test]
async fn act_as_reaches_no_other_pipeline_or_deployment_route() {
    let w = world().await;
    for tail in [
        "deploy/environments".to_owned(),
        "deploy/approvers".to_owned(),
        "pipeline-schedules".to_owned(),
        format!("pipelines/{}/diagnosis", w.pipeline),
        format!("pipelines/{}/jobs/{}/log", w.pipeline, w.job),
        format!("pipelines/{}/jobs/{}/artifacts", w.pipeline, w.job),
    ] {
        let uri = w.uri(&tail);
        let (status, body) = send(w.router.clone(), &uri, w.kanflow(), Some(w.olga)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{uri}: {body}"
        );
    }
    w.t.drop_now().await;
}

/// Plan 06 D-5 (review L5-9): a work item kanflow marked confidential is
/// answered to the tool like one forge never saw — its merge requests are
/// not listed, and nothing tells it apart; the person's own read lists them.
#[tokio::test]
async fn a_confidential_work_item_is_an_empty_list_for_the_tool() {
    let w = world().await;
    sqlx::query("UPDATE forge.work_items SET confidential = true WHERE ref = 'PBI-12'")
        .execute(&mut w.t.superuser().await)
        .await
        .expect("mark");
    let uri = w.uri("merge-requests?work_item=PBI-12&state=all");
    let (status, tool) = send(w.router.clone(), &uri, w.kanflow(), Some(w.olga)).await;
    assert_eq!(status, StatusCode::OK, "{tool}");
    assert_eq!(tool["merge_requests"], json!([]), "{tool}");
    let (_, own) = send(
        w.router.clone(),
        &uri,
        Who::Person(session_of(w.olga)),
        None,
    )
    .await;
    assert_eq!(
        own["merge_requests"].as_array().map(Vec::len),
        Some(1),
        "{own}"
    );
    w.t.drop_now().await;
}
