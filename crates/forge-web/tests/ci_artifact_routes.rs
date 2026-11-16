#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! CI artifacts (M4 C6, plan 14 §4.2, ADR-0029 decision 5) through the
//! product router against the development database: a running job uploads
//! its files with its own token (another job's token, a runner and people
//! are refused; an ended job's token is unauthenticated); a later job reads
//! the artifacts of the jobs it needs; people read them like the log, and
//! an AI agent audience is refused a confidential pipeline's
//! (FG-AUTH-0022). Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{self as db, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::artifacts::ArtifactStore;
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

struct Answer {
    status: StatusCode,
    bytes: Vec<u8>,
    json: Value,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

enum Who {
    As(Principal),
    Bearer(String),
}

enum Payload {
    None,
    Json(Value),
    Raw(Vec<u8>),
}

struct World {
    t: FreshDb,
    owner: UserId,
    web: ProjectId,
    storage: StorageRoot,
    git: Git,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let owner = users::create(
            &t.db,
            &ctx("req-user-ada"),
            &NewUser::human("ada", "Synthetic Ada", "ada@example.test"),
        )
        .await
        .expect("user");
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        let home = tempfile::tempdir().expect("home");
        let root = tempfile::tempdir().expect("root");
        let git = Git::new(GitConfig::new("git", home.path()));
        let storage = StorageRoot::open(root.path()).expect("root");
        Self {
            t,
            owner: owner.user_id,
            web: web.project_id,
            storage,
            git,
            _dirs: (home, root),
        }
    }

    async fn seed(&self, mark: CopyMark) -> i64 {
        let job = |name: &str, needs: &[&str], wave: i32| NewCiJob {
            name: name.to_owned(),
            stage: "build".to_owned(),
            wave,
            needs: needs.iter().map(|n| (*n).to_owned()).collect(),
            image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
            timeout_s: 600,
            tags: Vec::new(),
            secrets: Vec::new(),
        };
        db::create(
            &self.t.db,
            &NewPipeline {
                ai_eval_job: None,
                request: PipelineRequest {
                    project: self.web,
                    git_ref: RefName::parse("refs/heads/main").expect("ref"),
                    sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
                    source: Source::Push,
                    merge_request: None,
                    triggered_by: None,
                },
                request_id: "req-seed".to_owned(),
                definition_hash: Some(db::definition_hash(b"version: 1")),
                protected: false,
                copy_mark: mark,
                planned: Planned::Jobs(vec![job("build", &[], 0), job("test", &["build"], 1)]),
            },
        )
        .await
        .expect("seed")
        .pipeline_id
    }

    async fn call(&self, method: &str, uri: &str, who: Who, payload: Payload) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Who::Bearer(token) = &who {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut req = match payload {
            Payload::Json(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            Payload::Raw(b) => builder
                .header("content-type", "application/octet-stream")
                .body(Body::from(b)),
            Payload::None => builder.body(Body::empty()),
        }
        .expect("request");
        if let Who::As(p) = who {
            req.extensions_mut().insert(p);
        }
        let pipelines = Pipelines::new(
            self.t.db.clone(),
            Browser::new(
                self.git.clone(),
                self.storage.clone(),
                BrowseLimits::default(),
            ),
            Arc::new(OpenMarks),
        )
        .with_artifacts(ArtifactStore::new(self.storage.path()));
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(self.t.db.clone()),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_pipelines(pipelines);
        let res = forge_web::app(state, AppConfig::default())
            .expect("router")
            .oneshot(req)
            .await
            .expect("infallible");
        let status = res.status();
        let bytes = res
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Answer {
            status,
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            bytes,
        }
    }

    fn admin_session(&self) -> Principal {
        Principal::authenticated(
            self.owner,
            true,
            true,
            Credential::Session,
            AccountKind::Human,
        )
    }

    fn reader(&self) -> Principal {
        Principal::authenticated(
            self.owner,
            false,
            true,
            Credential::Token(vec![Scope::ReadApi]),
            AccountKind::Human,
        )
    }

    async fn runner(&self) -> String {
        let a = self
            .call(
                "POST",
                "/api/v1/admin/runners",
                Who::As(self.admin_session()),
                Payload::Json(json!({"name": "linux-1"})),
            )
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
        a.json["token"].as_str().expect("token").to_owned()
    }

    /// Claim the next job: `(job_id, attempt, job token)`.
    async fn claim(&self, runner: &str) -> (i64, i64, String) {
        let a = self
            .call(
                "POST",
                "/api/v1/runner/jobs/request",
                Who::Bearer(runner.to_owned()),
                Payload::Json(json!({"wait_s": 0})),
            )
            .await;
        let job = &a.json["job"];
        (
            job["job_id"].as_i64().expect("a job"),
            job["attempt"].as_i64().expect("attempt"),
            job["job_token"].as_str().expect("token").to_owned(),
        )
    }

    async fn upload(&self, job: i64, path: &str, who: Who, bytes: &[u8]) -> Answer {
        self.call(
            "PUT",
            &format!("/api/v1/runner/jobs/{job}/artifacts?path={path}"),
            who,
            Payload::Raw(bytes.to_vec()),
        )
        .await
    }
}

#[tokio::test]
async fn a_running_job_uploads_and_a_later_job_reads_what_it_needs() {
    let w = World::new().await;
    let runner = w.runner().await;
    let pipeline = w.seed(CopyMark::Open).await;
    let (build, attempt, build_token) = w.claim(&runner).await;
    let stored = w
        .upload(
            build,
            "out%2Fapp.bin",
            Who::Bearer(build_token.clone()),
            b"synthetic bytes",
        )
        .await;
    assert_eq!(stored.status, StatusCode::CREATED, "{}", stored.json);
    assert_eq!(stored.json["path"], "out/app.bin");
    assert_eq!(stored.json["size_bytes"], 15);
    let artifact = stored.json["id"].as_i64().expect("id");
    // The same path again, a bad path, another job's token, the runner's
    // token, a person: refused.
    let again = w
        .upload(
            build,
            "out%2Fapp.bin",
            Who::Bearer(build_token.clone()),
            b"x",
        )
        .await;
    assert_eq!(again.code(), "FG-WEB-0006", "{}", again.json);
    let escape = w
        .upload(build, "..%2Fx", Who::Bearer(build_token.clone()), b"x")
        .await;
    assert_eq!(escape.code(), "FG-WEB-0006", "{}", escape.json);
    let by_runner = w
        .upload(build, "y", Who::Bearer(runner.clone()), b"x")
        .await;
    assert_eq!(by_runner.code(), "FG-AUTH-0003", "{}", by_runner.json);
    let by_person = w.upload(build, "y", Who::As(w.admin_session()), b"x").await;
    assert_eq!(by_person.code(), "FG-AUTH-0003", "{}", by_person.json);
    let done = w
        .call(
            "POST",
            &format!("/api/v1/runner/jobs/{build}/complete"),
            Who::Bearer(runner.clone()),
            Payload::Json(json!({"attempt": attempt, "result": "succeeded", "exit_code": 0})),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.json);
    // The ended job's token is revoked.
    let late = w.upload(build, "z", Who::Bearer(build_token), b"x").await;
    assert_eq!(late.code(), "FG-AUTH-0001", "{}", late.json);
    // The next job needs `build`: it lists and reads its artifact.
    let (test, _, test_token) = w.claim(&runner).await;
    let needs = w
        .call(
            "GET",
            &format!("/api/v1/runner/jobs/{test}/needs/artifacts"),
            Who::Bearer(test_token.clone()),
            Payload::None,
        )
        .await;
    assert_eq!(needs.status, StatusCode::OK, "{}", needs.json);
    assert_eq!(needs.json["artifacts"][0]["path"], "out/app.bin");
    let one = w
        .call(
            "GET",
            &format!("/api/v1/runner/jobs/{test}/needs/artifacts/{artifact}"),
            Who::Bearer(test_token.clone()),
            Payload::None,
        )
        .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.bytes, b"synthetic bytes");
    // Its token on another job's path: the invisible project's 404.
    let other = w
        .call(
            "GET",
            &format!("/api/v1/runner/jobs/{build}/needs/artifacts"),
            Who::Bearer(test_token),
            Payload::None,
        )
        .await;
    assert_eq!(other.code(), "FG-REPO-0005", "{}", other.json);
    // A person reads them like the log.
    let base = format!(
        "/api/v1/projects/{}/pipelines/{pipeline}/jobs/{build}/artifacts",
        w.web.get()
    );
    let list = w
        .call("GET", &base, Who::As(w.reader()), Payload::None)
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    assert_eq!(list.json["artifacts"][0]["id"], artifact);
    let file = w
        .call(
            "GET",
            &format!("{base}/{artifact}"),
            Who::As(w.reader()),
            Payload::None,
        )
        .await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(file.bytes, b"synthetic bytes");
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_agent_is_refused_the_artifacts_of_a_confidential_pipeline() {
    let w = World::new().await;
    let runner = w.runner().await;
    let pipeline = w.seed(CopyMark::Confidential).await;
    let (build, _, token) = w.claim(&runner).await;
    let stored = w
        .upload(build, "report.txt", Who::Bearer(token), b"x")
        .await;
    assert_eq!(stored.status, StatusCode::CREATED, "{}", stored.json);
    let base = format!(
        "/api/v1/projects/{}/pipelines/{pipeline}/jobs/{build}/artifacts",
        w.web.get()
    );
    let agent = w.reader().as_agent(true);
    let refused = w.call("GET", &base, Who::As(agent), Payload::None).await;
    assert_eq!(refused.code(), "FG-AUTH-0022", "{}", refused.json);
    // A person still reads it.
    let person = w
        .call("GET", &base, Who::As(w.reader()), Payload::None)
        .await;
    assert_eq!(person.status, StatusCode::OK, "{}", person.json);
    w.t.drop_now().await;
}
