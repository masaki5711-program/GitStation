#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Pipeline schedules and manual jobs (M4 C12, plan 14 §5 C12) through the
//! product router against the development database: a Maintainer creates,
//! lists, changes and deletes a schedule (and owns it); a Developer and an
//! agent's token are refused; a bad rule names its parameter; a Developer
//! starts a manual job once, a Reporter may not. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::pipelines::{self as db, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
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
    web: ProjectId,
    storage: StorageRoot,
    git: Git,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn token(user: UserId) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::Human,
    )
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
        let member = |name: &'static str| {
            let db = t.db.clone();
            async move {
                users::create(
                    &db,
                    &ctx(&format!("req-user-{name}")),
                    &NewUser::human(name, name, &format!("{name}@example.test")),
                )
                .await
                .expect("user")
                .user_id
            }
        };
        let dev = member("dev").await;
        let reporter = member("rita").await;
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", owner.namespace_id, "Web", owner.user_id),
        )
        .await
        .expect("web");
        for (user, role) in [(dev, Role::Developer), (reporter, Role::Reporter)] {
            projects::add_member(&t.db, &ctx("req-member"), web.project_id, user, role)
                .await
                .expect("member");
        }
        let home = tempfile::tempdir().expect("home");
        let root = tempfile::tempdir().expect("root");
        let git = Git::new(GitConfig::new("git", home.path()));
        let storage = StorageRoot::open(root.path()).expect("root");
        Self {
            t,
            owner: owner.user_id,
            dev,
            reporter,
            web: web.project_id,
            storage,
            git,
            _dirs: (home, root),
        }
    }

    async fn call(&self, method: &str, uri: &str, who: Principal, body: Option<Value>) -> Answer {
        let builder = Request::builder().method(method).uri(uri);
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        req.extensions_mut().insert(who);
        let pipelines = Pipelines::new(
            self.t.db.clone(),
            Browser::new(
                self.git.clone(),
                self.storage.clone(),
                BrowseLimits::default(),
            ),
            Arc::new(OpenMarks),
        );
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

    fn list_uri(&self) -> String {
        format!("/api/v1/projects/{}/pipeline-schedules", self.web.get())
    }
}

fn nightly() -> Value {
    json!({"description": "Nightly build", "ref": "main", "cron": "0 2 * * *",
           "utc_offset_minutes": 540})
}

#[tokio::test]
async fn a_maintainer_manages_schedules_and_owns_what_they_save() {
    let w = World::new().await;
    let created = w
        .call("POST", &w.list_uri(), token(w.owner), Some(nightly()))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    assert_eq!(created.json["ref"], "refs/heads/main");
    assert_eq!(created.json["owner_id"], w.owner.get());
    assert_eq!(created.json["active"], true);
    assert!(created.json["next_run_at"].is_string());
    assert_eq!(created.json["last_outcome"], Value::Null);
    let id = created.json["id"].as_i64().expect("id");

    // A reader lists them (PipelineRead).
    let list = w.call("GET", &w.list_uri(), token(w.reporter), None).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    assert_eq!(list.json["schedules"][0]["id"], id);

    // A bad rule names its parameter and reason.
    let mut bad = nightly();
    bad["cron"] = json!("* * * * *");
    let refused = w
        .call("POST", &w.list_uri(), token(w.owner), Some(bad))
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.json);
    assert_eq!(refused.code(), "FG-WEB-0006");

    // A Developer and an agent's token of the owner are refused.
    let dev = w
        .call("POST", &w.list_uri(), token(w.dev), Some(nightly()))
        .await;
    assert_eq!(dev.code(), "FG-AUTH-0003", "{}", dev.json);
    let agent = w
        .call(
            "POST",
            &w.list_uri(),
            token(w.owner).as_agent(true),
            Some(nightly()),
        )
        .await;
    assert_eq!(agent.code(), "FG-AUTH-0003", "{}", agent.json);

    let one = format!("{}/{id}", w.list_uri());
    let mut off = nightly();
    off["active"] = json!(false);
    let updated = w.call("PUT", &one, token(w.owner), Some(off)).await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.json);
    assert_eq!(updated.json["active"], false);
    assert_eq!(updated.json["next_run_at"], Value::Null);

    let gone = w.call("DELETE", &one, token(w.owner), None).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let again = w.call("DELETE", &one, token(w.owner), None).await;
    assert_eq!(again.code(), "FG-REPO-0005", "{}", again.json);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_developer_starts_a_manual_job_once() {
    let w = World::new().await;
    let pipeline = db::create(
        &w.t.db,
        &NewPipeline {
            request: PipelineRequest {
                project: w.web,
                git_ref: RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&"a".repeat(40)).expect("sha"),
                source: Source::Push,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-seed".to_owned(),
            definition_hash: Some(db::definition_hash(b"version: 1")),
            protected: false,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![NewCiJob {
                name: "deploy".to_owned(),
                stage: "deploy".to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                timeout_s: 600,
                tags: Vec::new(),
                secrets: Vec::new(),
                manual: true,
            }]),
        },
    )
    .await
    .expect("seed")
    .pipeline_id;
    let show = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}/pipelines/{pipeline}", w.web.get()),
            token(w.reporter),
            None,
        )
        .await;
    assert_eq!(show.json["status"], "skipped", "{}", show.json);
    assert_eq!(show.json["jobs"][0]["status"], "manual");
    assert_eq!(show.json["jobs"][0]["manual"], true);
    let job = show.json["jobs"][0]["id"].as_i64().expect("job");
    let play = format!(
        "/api/v1/projects/{}/pipelines/{pipeline}/jobs/{job}/play",
        w.web.get()
    );
    let reporter = w.call("POST", &play, token(w.reporter), None).await;
    assert_eq!(reporter.code(), "FG-AUTH-0003", "{}", reporter.json);
    let started = w.call("POST", &play, token(w.dev), None).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.json);
    assert_eq!(started.json["status"], "running");
    assert_eq!(started.json["jobs"][0]["status"], "pending");
    let twice = w.call("POST", &play, token(w.dev), None).await;
    assert_eq!(twice.status, StatusCode::CONFLICT, "{}", twice.json);
    assert_eq!(twice.code(), "FG-CI-0003");
    w.t.drop_now().await;
}
