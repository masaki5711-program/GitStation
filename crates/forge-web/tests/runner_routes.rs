#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The runner API and the runner administration (M4 C3, plan 14 §4.2,
//! ADR-0029) through the product router against the development database,
//! authenticating real `fgr_` / `fgj_` tokens with forge-db: an
//! administrator's web session registers a runner (a token, even an
//! `admin` one, is refused); the runner claims, heartbeats, logs and
//! completes; another runner, a job's token and people are refused; a
//! lost lease is `409 FG-CI-0004`; a job's token reaches no API route
//! (`FG-AUTH-0003`) and is revoked when the job ends (`FG-AUTH-0001`); a
//! disabled runner's token is refused. Synthetic data only.

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

/// Who calls: a principal set as an extension, or a bearer token.
enum Who {
    As(Principal),
    Bearer(String),
}

struct World {
    t: FreshDb,
    admin: UserId,
    web: ProjectId,
    storage: StorageRoot,
    git: Git,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let admin = users::create(
            &t.db,
            &ctx("req-user-ada"),
            &NewUser::human("ada", "Synthetic Ada", "ada@example.test"),
        )
        .await
        .expect("user");
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", admin.namespace_id, "Web", admin.user_id),
        )
        .await
        .expect("web");
        let home = tempfile::tempdir().expect("home");
        let root = tempfile::tempdir().expect("root");
        let git = Git::new(GitConfig::new("git", home.path()));
        let storage = StorageRoot::open(root.path()).expect("root");
        Self {
            t,
            admin: admin.user_id,
            web: web.project_id,
            storage,
            git,
            _dirs: (home, root),
        }
    }

    async fn seed(&self) -> i64 {
        db::create(
            &self.t.db,
            &NewPipeline {
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
                copy_mark: CopyMark::Open,
                planned: Planned::Jobs(vec![NewCiJob {
                    name: "build".to_owned(),
                    stage: "build".to_owned(),
                    wave: 0,
                    needs: Vec::new(),
                    image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                    timeout_s: 600,
                    tags: Vec::new(),
                    // M4 C6: a job naming secrets on an unprotected push is failed
                    // at the claim (ci_secret_routes.rs); this one names none.
                    secrets: Vec::new(),
                }]),
            },
        )
        .await
        .expect("seed")
        .pipeline_id
    }

    async fn call(&self, method: &str, uri: &str, who: Who, body: Option<Value>) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Who::Bearer(token) = &who {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut req = match body {
            Some(b) => builder
                .header("content-type", "application/json")
                .body(Body::from(b.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("request");
        if let Who::As(p) = who {
            req.extensions_mut().insert(p);
        }
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

    fn admin_session(&self) -> Principal {
        Principal::authenticated(
            self.admin,
            true,
            true,
            Credential::Session,
            AccountKind::Human,
        )
    }

    async fn register(&self, name: &str) -> (i64, String) {
        let a = self
            .call(
                "POST",
                "/api/v1/admin/runners",
                Who::As(self.admin_session()),
                Some(json!({"name": name, "tags": ["linux"]})),
            )
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
        (
            a.json["runner"]["id"].as_i64().expect("id"),
            a.json["token"].as_str().expect("token").to_owned(),
        )
    }

    async fn claim(&self, token: &str) -> Answer {
        self.call(
            "POST",
            "/api/v1/runner/jobs/request",
            Who::Bearer(token.to_owned()),
            Some(json!({"wait_s": 0})),
        )
        .await
    }
}

fn job_uri(job: i64, tail: &str) -> String {
    format!("/api/v1/runner/jobs/{job}/{tail}")
}

#[tokio::test]
async fn an_administrators_session_registers_runners_and_tokens_cannot() {
    let w = World::new().await;
    let (id, token) = w.register("linux-1").await;
    assert!(token.starts_with("fgr_"));
    let listed = w
        .call(
            "GET",
            "/api/v1/admin/runners",
            Who::As(w.admin_session()),
            None,
        )
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.json["runners"][0]["name"], "linux-1");
    assert_eq!(listed.json["runners"][0]["tags"], json!(["linux"]));
    assert!(!listed.json.to_string().contains(&token[4..]));
    // An administrator's token, even with `admin`: session only.
    let admin_token = Principal::authenticated(
        w.admin,
        true,
        true,
        Credential::Token(vec![Scope::Api, Scope::Admin]),
        AccountKind::Human,
    );
    let refused = w
        .call(
            "POST",
            "/api/v1/admin/runners",
            Who::As(admin_token),
            Some(json!({"name": "linux-2"})),
        )
        .await;
    assert_eq!(refused.code(), "FG-AUTH-0002");
    // A runner's token administers nothing.
    let refused = w
        .call(
            "GET",
            "/api/v1/admin/runners",
            Who::Bearer(token.clone()),
            None,
        )
        .await;
    assert_eq!(refused.code(), "FG-AUTH-0003");
    // A bad name, a bad tag.
    for body in [
        json!({"name": "Linux"}),
        json!({"name": "x", "tags": ["A b"]}),
    ] {
        let bad = w
            .call(
                "POST",
                "/api/v1/admin/runners",
                Who::As(w.admin_session()),
                Some(body),
            )
            .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.json);
    }
    // Disabling it refuses its token at once.
    let off = w
        .call(
            "PATCH",
            &format!("/api/v1/admin/runners/{id}"),
            Who::As(w.admin_session()),
            Some(json!({"enabled": false})),
        )
        .await;
    assert_eq!(off.status, StatusCode::OK, "{}", off.json);
    assert_eq!(off.json["enabled"], false);
    let refused = w.claim(&token).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::UNAUTHORIZED, "FG-AUTH-0001")
    );
    let missing = w
        .call(
            "PATCH",
            &format!("/api/v1/admin/runners/{}", id + 100),
            Who::As(w.admin_session()),
            Some(json!({"enabled": true})),
        )
        .await;
    assert_eq!(missing.code(), "FG-REPO-0005");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_runner_claims_heartbeats_logs_and_completes_its_job() {
    let w = World::new().await;
    let (_, token) = w.register("linux-1").await;
    let (_, other) = w.register("linux-2").await;
    let pipeline = w.seed().await;
    let claimed = w.claim(&token).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.json);
    let job = &claimed.json["job"];
    let job_id = job["job_id"].as_i64().expect("job");
    let attempt = job["attempt"].as_i64().expect("attempt");
    let job_token = job["job_token"].as_str().expect("job token").to_owned();
    assert!(job_token.starts_with("fgj_"));
    assert_eq!(job["pipeline_id"].as_i64(), Some(pipeline));
    assert_eq!(job["sha"], "b".repeat(40));
    // An unprotected push: never secret-eligible, no names handed out.
    assert_eq!(job["secrets_allowed"], false);
    assert_eq!(job["secret_names"], json!([]));
    // Nothing else waits.
    assert_eq!(w.claim(&other).await.json, json!({"job": null}));
    let beat = w
        .call(
            "POST",
            &job_uri(job_id, "heartbeat"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt})),
        )
        .await;
    assert_eq!(beat.status, StatusCode::OK, "{}", beat.json);
    assert_eq!(beat.json["state"], "running");
    // Another runner, the job's own token and a person are refused.
    for who in [
        Who::Bearer(other.clone()),
        Who::Bearer(job_token.clone()),
        Who::As(w.admin_session()),
    ] {
        let refused = w
            .call(
                "POST",
                &job_uri(job_id, "heartbeat"),
                who,
                Some(json!({"attempt": attempt})),
            )
            .await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
            "{}",
            refused.json
        );
    }
    let log = w
        .call(
            "POST",
            &job_uri(job_id, "log"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt, "seq": 0, "text": "compiling\n"})),
        )
        .await;
    assert_eq!(log.json, json!({"stored": true}));
    // A job's token reads no API route (`ci_not_allowed`; its one action,
    // a copy of its pinned SHA, refuses every other resource with the
    // invisible project's 404: matrix rows `matrix_rows_ci.rs`).
    let read = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}", w.web.get()),
            Who::Bearer(job_token.clone()),
            None,
        )
        .await;
    assert_eq!(
        (read.status, read.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{}",
        read.json
    );
    // A runner's token reads nothing either.
    let read = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}", w.web.get()),
            Who::Bearer(token.clone()),
            None,
        )
        .await;
    assert_eq!(read.code(), "FG-AUTH-0003");
    let done = w
        .call(
            "POST",
            &job_uri(job_id, "complete"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt, "result": "succeeded", "exit_code": 0})),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.json);
    assert_eq!(
        done.json,
        json!({"status": "succeeded", "pipeline_status": "succeeded"})
    );
    // The job ended: its token is revoked (FG-AUTH-0001), and the runner
    // no longer acts on the job.
    let read = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}", w.web.get()),
            Who::Bearer(job_token),
            None,
        )
        .await;
    assert_eq!(
        (read.status, read.code()),
        (StatusCode::UNAUTHORIZED, "FG-AUTH-0001")
    );
    let late = w
        .call(
            "POST",
            &job_uri(job_id, "heartbeat"),
            Who::Bearer(token),
            Some(json!({"attempt": attempt})),
        )
        .await;
    assert_eq!(late.code(), "FG-AUTH-0003");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_lost_lease_is_a_conflict_and_bad_requests_are_refused() {
    let w = World::new().await;
    let (_, token) = w.register("linux-1").await;
    w.seed().await;
    let bad = w
        .call(
            "POST",
            "/api/v1/runner/jobs/request",
            Who::Bearer(token.clone()),
            Some(json!({"wait_s": 21})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let claimed = w.claim(&token).await;
    let job_id = claimed.json["job"]["job_id"].as_i64().expect("job");
    let attempt = claimed.json["job"]["attempt"].as_i64().expect("attempt");
    let unknown = w
        .call(
            "POST",
            &job_uri(job_id, "complete"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt, "result": "maybe"})),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST);
    // A NUL cannot be stored as text: a bad request, not a database error.
    let nul = w
        .call(
            "POST",
            &job_uri(job_id, "log"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt, "seq": 0, "text": "a\u{0}b"})),
        )
        .await;
    assert_eq!(
        (nul.status, nul.code()),
        (StatusCode::BAD_REQUEST, "FG-WEB-0006"),
        "{}",
        nul.json
    );
    w.t.exec_superuser(&format!(
        "UPDATE forge.ci_jobs SET lease_until = now() - interval '1 second' WHERE id = {job_id}"
    ))
    .await;
    let lost = w
        .call(
            "POST",
            &job_uri(job_id, "heartbeat"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt})),
        )
        .await;
    assert_eq!(
        (lost.status, lost.code()),
        (StatusCode::CONFLICT, "FG-CI-0004")
    );
    let absent = w
        .call(
            "POST",
            &job_uri(job_id + 100, "heartbeat"),
            Who::Bearer(token),
            Some(json!({"attempt": 1})),
        )
        .await;
    assert_eq!(absent.code(), "FG-REPO-0005");
    w.t.drop_now().await;
}
