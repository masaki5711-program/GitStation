#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The CI pipeline routes (M4 C2, plan 14 §4.2) through the product router
//! against the development database (ADR-0011), with the real loaders,
//! `forge-pipelines` and a real bare repository: readers list and read
//! (an agent's token too, metadata only), a Guest is refused, a person
//! outside a private project gets the invisible project's 404; Developers
//! (an agent's token and a service account too) cancel, retry and run by
//! hand, a Reporter is refused; a finished pipeline is `409 FG-CI-0003`.
//! Principals are inserted as request extensions. Synthetic data only.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{self as db, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter, Pipelines};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct OpenMarks;

impl MarkMeter for OpenMarks {
    fn measure<'a>(&'a self, _: RepoId, _: &'a ObjectId) -> MarkFuture<'a> {
        Box::pin(async { CopyMark::Open })
    }
}

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

async fn person(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(
            name,
            &format!("Synthetic {name}"),
            &format!("{name}@example.test"),
        ),
    )
    .await
    .expect("user")
}

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
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

fn definition() -> String {
    format!(
        "version: 1\nstages: [build]\njobs:\n  build:\n    stage: build\n    image: registry.internal/rust@sha256:{}\n    timeout: 10m\n    script: [cargo build]\n",
        "a".repeat(64)
    )
}

struct Answer {
    status: StatusCode,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

struct World {
    t: FreshDb,
    owner: UserId,
    dev: UserId,
    reporter: UserId,
    guest: UserId,
    outsider: UserId,
    sa: UserId,
    web: ProjectId,
    storage: StorageRoot,
    git: Git,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let owner = person(&t, "olive").await;
        let dev = person(&t, "dave").await.user_id;
        let reporter = person(&t, "rita").await.user_id;
        let guest = person(&t, "gus").await.user_id;
        let outsider = person(&t, "otto").await.user_id;
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        let sa = service_accounts::create(
            &t.db,
            &ctx("req-sa"),
            &NewServiceAccount {
                username: "kanflow-bot".to_owned(),
                display_name: "Kanflow".to_owned(),
                owner: owner.user_id,
                purpose: "synthetic pipeline runner".to_owned(),
            },
            owner.user_id,
        )
        .await
        .expect("sa")
        .user_id;
        for (user, role) in [
            (dev, Role::Developer),
            (reporter, Role::Reporter),
            (guest, Role::Guest),
            (sa, Role::Developer),
        ] {
            projects::add_member(&t.db, &ctx("req-member"), web.project_id, user, role)
                .await
                .expect("member");
        }
        let _ = forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), web.repository_id)
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
        let blob = plumb(
            &path,
            &["hash-object", "-w", "--stdin"],
            Some(definition().as_bytes()),
        );
        let tree = plumb(
            &path,
            &["mktree"],
            Some(format!("100644 blob {blob}\t.forge-ci.yml\n").as_bytes()),
        );
        let commit = plumb(&path, &["commit-tree", &tree, "-m", "synthetic"], None);
        plumb(&path, &["update-ref", "refs/heads/main", &commit], None);
        Self {
            t,
            owner: owner.user_id,
            dev,
            reporter,
            guest,
            outsider,
            sa,
            web: web.project_id,
            storage,
            git,
            _dirs: (home, root),
        }
    }

    async fn seed(&self, branch: &str) -> i64 {
        db::create(
            &self.t.db,
            &NewPipeline {
                request: PipelineRequest {
                    project: self.web,
                    git_ref: RefName::parse(&format!("refs/heads/{branch}")).expect("ref"),
                    sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
                    source: Source::Push,
                    merge_request: None,
                    triggered_by: Some(self.dev),
                },
                request_id: "req-seed".to_owned(),
                definition_hash: Some(db::definition_hash(b"version: 1")),
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
                    secrets: vec!["DEPLOY_KEY".to_owned()],
                }]),
            },
        )
        .await
        .expect("seed")
        .pipeline_id
    }

    /// The succeeded pipeline audit rows: action, actor kind, actor.
    async fn audit(&self) -> Vec<(String, String, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, actor_kind, actor_user_id FROM forge.audit_log              WHERE action LIKE 'pipeline.%' AND outcome = 'succeeded' ORDER BY id",
        )
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }

    async fn call(&self, method: &str, path: &str, who: Principal, body: Option<Value>) -> Answer {
        let uri = format!("/api/v1/projects/{}/pipelines{path}", self.web.get());
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        req.extensions_mut().insert(who);
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(self.t.db.clone()),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_pipelines(Pipelines::new(
            self.t.db.clone(),
            Browser::new(
                self.git.clone(),
                self.storage.clone(),
                BrowseLimits::default(),
            ),
            Arc::new(OpenMarks),
        ));
        let res = forge_web::app(state, AppConfig::default())
            .expect("router")
            .oneshot(req)
            .await
            .expect("infallible");
        let status = res.status();
        let raw = res
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Answer {
            status,
            json: serde_json::from_slice(&raw).unwrap_or(Value::Null),
        }
    }
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, scopes: &[Scope], kind: AccountKind) -> Principal {
    Principal::authenticated(user, false, true, Credential::Token(scopes.to_vec()), kind)
}

fn agent(user: UserId, scopes: &[Scope]) -> Principal {
    token(user, scopes, AccountKind::Human).as_agent(true)
}

#[tokio::test]
async fn readers_list_and_read_pipelines_and_others_are_refused() {
    let w = World::new().await;
    let id = w.seed("main").await;
    let _other = w.seed("feature").await;
    let a = w.call("GET", "", session(w.reporter), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["pipelines"].as_array().expect("list").len(), 2);
    assert!(
        a.json["pipelines"][0].get("jobs").is_none(),
        "the list has no jobs"
    );
    let a = w
        .call("GET", "?ref=main&limit=1", session(w.reporter), None)
        .await;
    assert_eq!(a.json["pipelines"][0]["id"], id);
    assert_eq!(a.json["pipelines"][0]["ref"], "refs/heads/main");
    assert_eq!(a.json["next_before"], id);
    // One pipeline with its jobs; an agent's read token reads metadata too.
    let a = w
        .call(
            "GET",
            &format!("/{id}"),
            agent(w.dev, &[Scope::ReadApi]),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["jobs"][0]["name"], "build");
    assert_eq!(a.json["jobs"][0]["secrets"], json!(["DEPLOY_KEY"]));
    assert_eq!(a.json["copy_mark"], "open");
    assert_eq!(a.json["protected"], false);
    // No such pipeline in the project.
    let a = w.call("GET", "/999999", session(w.reporter), None).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // A Guest: 403; a person outside the private project: the invisible 404.
    let a = w.call("GET", "", session(w.guest), None).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let a = w
        .call("GET", &format!("/{id}"), session(w.outsider), None)
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // A bad query is refused.
    let a = w.call("GET", "?limit=0", session(w.reporter), None).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    w.t.drop_now().await;
}

#[tokio::test]
async fn developers_cancel_and_retry_and_a_finished_pipeline_conflicts() {
    let w = World::new().await;
    let id = w.seed("main").await;
    let a = w
        .call("POST", &format!("/{id}/cancel"), session(w.reporter), None)
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let a = w
        .call("POST", &format!("/{id}/cancel"), session(w.outsider), None)
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // An agent's api token (Developer) cancels.
    let a = w
        .call(
            "POST",
            &format!("/{id}/cancel"),
            agent(w.dev, &[Scope::Api]),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["status"], "canceled");
    assert_eq!(a.json["jobs"][0]["status"], "canceled");
    let a = w
        .call("POST", &format!("/{id}/cancel"), session(w.dev), None)
        .await;
    assert_eq!((a.status, a.code()), (StatusCode::CONFLICT, "FG-CI-0003"));
    // A read-only token is refused before anything is changed.
    let a = w
        .call(
            "POST",
            &format!("/{id}/retry"),
            agent(w.dev, &[Scope::ReadApi]),
            None,
        )
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0002")
    );
    // A service account (Developer) retries: a new attempt.
    let a = w
        .call(
            "POST",
            &format!("/{id}/retry"),
            token(w.sa, &[Scope::Api], AccountKind::ServiceAccount),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.json["status"], "pending");
    assert_eq!(a.json["jobs"][0]["attempt"], 2);
    let a = w
        .call("POST", &format!("/{id}/retry"), session(w.owner), None)
        .await;
    assert_eq!((a.status, a.code()), (StatusCode::CONFLICT, "FG-CI-0003"));
    // No such pipeline.
    let a = w.call("POST", "/999999/cancel", session(w.dev), None).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    // One audit row per change, naming who: the agent's person, then the
    // service account (refusals and conflicts write no succeeded row).
    assert_eq!(
        w.audit().await,
        [
            (
                "pipeline.canceled".to_owned(),
                "user".to_owned(),
                Some(w.dev.get())
            ),
            (
                "pipeline.retried".to_owned(),
                "service_account".to_owned(),
                Some(w.sa.get())
            ),
        ]
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_developer_runs_a_pipeline_by_hand_and_a_reporter_cannot() {
    let w = World::new().await;
    let a = w
        .call(
            "POST",
            "",
            session(w.reporter),
            Some(json!({"ref": "main"})),
        )
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003")
    );
    let a = w
        .call("POST", "", session(w.dev), Some(json!({"ref": "main"})))
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
    assert_eq!(a.json["source"], "manual");
    assert_eq!(a.json["status"], "pending");
    assert_eq!(a.json["triggered_by"], w.dev.get());
    assert_eq!(a.json["jobs"][0]["name"], "build");
    // Each manual run is a run of its own.
    let b = w
        .call(
            "POST",
            "",
            agent(w.dev, &[Scope::Api]),
            Some(json!({"ref": "refs/heads/main"})),
        )
        .await;
    assert_eq!(b.status, StatusCode::CREATED, "{}", b.json);
    assert_ne!(a.json["id"], b.json["id"]);
    // No such branch; a tag is no branch.
    let a = w
        .call("POST", "", session(w.dev), Some(json!({"ref": "nope"})))
        .await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0023")
    );
    let a = w
        .call(
            "POST",
            "",
            session(w.dev),
            Some(json!({"ref": "refs/tags/v1"})),
        )
        .await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST);
    // Two manual runs, two audit rows (a manual run has no idempotency key).
    assert_eq!(
        w.audit().await,
        [
            (
                "pipeline.run_manual".to_owned(),
                "user".to_owned(),
                Some(w.dev.get())
            ),
            (
                "pipeline.run_manual".to_owned(),
                "user".to_owned(),
                Some(w.dev.get())
            ),
        ]
    );
    w.t.drop_now().await;
}
