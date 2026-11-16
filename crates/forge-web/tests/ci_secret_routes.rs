#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! CI variables and their release (M4 C6, plan 14 §4.2, ADR-0029 decision
//! 8) through the product router against the development database: a
//! Maintainer sets, lists and deletes variables and never reads a value
//! back; an agent's token is refused; a runner's claim of a job of a
//! protected push gets the values; a job of an unprotected pipeline, a job
//! naming an undefined variable, and a job whose values cannot be opened
//! are failed in the claim's place and never handed out; a log chunk is
//! stored with the job's masked values replaced. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::ci_secret::CiSecretKeys;
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

/// Synthetic key material (test only).
const KEYS: &str = "1 00000000000000000000000000000000000000000000000000000000000000aa";
const VALUE: &str = "synthetic-deploy-value-0001";

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

enum Who {
    As(Principal),
    Bearer(String),
}

struct World {
    t: FreshDb,
    owner: UserId,
    web: ProjectId,
    storage: StorageRoot,
    git: Git,
    keys: bool,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl World {
    async fn new(keys: bool) -> Self {
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
            keys,
            _dirs: (home, root),
        }
    }

    async fn seed(&self, sha: char, protected: bool, secrets: &[&str]) -> i64 {
        db::create(
            &self.t.db,
            &NewPipeline {
                ai_eval_job: None,
                request: PipelineRequest {
                    project: self.web,
                    git_ref: RefName::parse("refs/heads/main").expect("ref"),
                    sha: ObjectId::parse(&sha.to_string().repeat(40)).expect("sha"),
                    source: Source::Push,
                    merge_request: None,
                    triggered_by: None,
                },
                request_id: "req-seed".to_owned(),
                definition_hash: Some(db::definition_hash(b"version: 1")),
                protected,
                copy_mark: CopyMark::Open,
                planned: Planned::Jobs(vec![NewCiJob {
                    name: "deploy".to_owned(),
                    stage: "deploy".to_owned(),
                    wave: 0,
                    needs: Vec::new(),
                    image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                    timeout_s: 600,
                    tags: Vec::new(),
                    secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
                }]),
            },
        )
        .await
        .expect("seed")
        .pipeline_id
    }

    async fn call(&self, method: &str, uri: &str, who: Who, body: Option<Value>) -> Answer {
        let raw = body.map(|b| (b.to_string().into_bytes(), "application/json"));
        self.send(method, uri, who, raw).await
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        who: Who,
        body: Option<(Vec<u8>, &str)>,
    ) -> Answer {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Who::Bearer(token) = &who {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut req = match body {
            Some((b, kind)) => builder.header("content-type", kind).body(Body::from(b)),
            None => builder.body(Body::empty()),
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
        );
        let pipelines = if self.keys {
            pipelines.with_secret_keys(Arc::new(CiSecretKeys::parse(KEYS).expect("keys")))
        } else {
            pipelines
        };
        let state = AppState::new(
            "test",
            Vec::new(),
            Arc::new(self.t.db.clone()),
            Lookups {
                namespaces: Arc::new(self.t.db.clone()),
                projects: Arc::new(self.t.db.clone()),
            },
        )
        .with_pipelines(pipelines.with_artifacts(ArtifactStore::new(self.storage.path())));
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

    fn owner_token(&self) -> Principal {
        Principal::authenticated(
            self.owner,
            false,
            true,
            Credential::Token(vec![Scope::Api]),
            AccountKind::Human,
        )
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

    fn var_uri(&self, name: &str) -> String {
        format!("/api/v1/projects/{}/ci/variables/{name}", self.web.get())
    }

    async fn set(&self, name: &str, value: &str) -> Answer {
        self.call(
            "PUT",
            &self.var_uri(name),
            Who::As(self.owner_token()),
            Some(json!({"value": value})),
        )
        .await
    }

    async fn runner(&self) -> String {
        let a = self
            .call(
                "POST",
                "/api/v1/admin/runners",
                Who::As(self.admin_session()),
                Some(json!({"name": "linux-1"})),
            )
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
        a.json["token"].as_str().expect("token").to_owned()
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

    async fn job_error(&self, pipeline: i64) -> (String, Option<String>) {
        let mut su = self.t.superuser().await;
        sqlx::query_as("SELECT status, error_code FROM forge.ci_jobs WHERE pipeline_id = $1")
            .bind(pipeline)
            .fetch_one(&mut su)
            .await
            .expect("job")
    }
}

#[tokio::test]
async fn a_maintainer_sets_lists_and_deletes_variables_and_never_reads_a_value() {
    let w = World::new(true).await;
    let set = w.set("DEPLOY_TOKEN", VALUE).await;
    assert_eq!(set.status, StatusCode::CREATED, "{}", set.json);
    assert_eq!(set.json["name"], "DEPLOY_TOKEN");
    assert_eq!(set.json["masked"], true);
    assert_eq!(set.json["protected_only"], true);
    assert!(!set.json.to_string().contains(VALUE));
    let again = w.set("DEPLOY_TOKEN", "synthetic-deploy-value-0002").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    let list = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}/ci/variables", w.web.get()),
            Who::As(w.owner_token()),
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.json);
    assert_eq!(list.json["variables"][0]["name"], "DEPLOY_TOKEN");
    assert!(!list.json.to_string().contains("synthetic-deploy"));
    // A bad value is refused without echoing it.
    for value in ["short", "two\nlines-value"] {
        let bad = w.set("OTHER_TOKEN", value).await;
        assert_eq!(bad.code(), "FG-WEB-0006", "{}", bad.json);
        assert!(!bad.json.to_string().contains(value), "{}", bad.json);
    }
    let reserved = w.set("FORGE_TOKEN", VALUE).await;
    assert_eq!(reserved.code(), "FG-WEB-0006");
    // An agent's token of the same person: people only.
    let agent = w
        .call(
            "PUT",
            &w.var_uri("AGENT_TOKEN"),
            Who::As(w.owner_token().as_agent(true)),
            Some(json!({"value": VALUE})),
        )
        .await;
    assert_eq!(agent.code(), "FG-AUTH-0003", "{}", agent.json);
    let gone = w
        .call(
            "DELETE",
            &w.var_uri("DEPLOY_TOKEN"),
            Who::As(w.owner_token()),
            None,
        )
        .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let missing = w
        .call(
            "DELETE",
            &w.var_uri("DEPLOY_TOKEN"),
            Who::As(w.owner_token()),
            None,
        )
        .await;
    assert_eq!(missing.code(), "FG-REPO-0005");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_protected_push_job_gets_its_values_and_its_log_is_masked() {
    let w = World::new(true).await;
    assert_eq!(
        w.set("DEPLOY_TOKEN", VALUE).await.status,
        StatusCode::CREATED
    );
    let token = w.runner().await;
    w.seed('b', true, &["DEPLOY_TOKEN"]).await;
    let claimed = w.claim(&token).await;
    assert_eq!(claimed.status, StatusCode::OK, "{}", claimed.json);
    let job = &claimed.json["job"];
    assert_eq!(job["secrets_allowed"], true);
    assert_eq!(
        job["secrets"],
        json!([{"name": "DEPLOY_TOKEN", "value": VALUE, "masked": true}])
    );
    let job_id = job["job_id"].as_i64().expect("job");
    let attempt = job["attempt"].as_i64().expect("attempt");
    // The variable is replaced, then deleted, while the job runs: the
    // released value stays masked (the claim's snapshot).
    assert_eq!(
        w.set("DEPLOY_TOKEN", "synthetic-other-value-02")
            .await
            .status,
        StatusCode::OK
    );
    let gone = w
        .call(
            "DELETE",
            &w.var_uri("DEPLOY_TOKEN"),
            Who::As(w.owner_token()),
            None,
        )
        .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let log = w
        .call(
            "POST",
            &format!("/api/v1/runner/jobs/{job_id}/log"),
            Who::Bearer(token.clone()),
            Some(json!({"attempt": attempt, "seq": 0, "text": format!("using {VALUE}\n")})),
        )
        .await;
    assert_eq!(log.json, json!({"stored": true}), "{}", log.json);
    let mut su = w.t.superuser().await;
    let body: String = sqlx::query_scalar("SELECT body FROM forge.ci_log_chunks WHERE job_id = $1")
        .bind(job_id)
        .fetch_one(&mut su)
        .await
        .expect("chunk");
    assert_eq!(body, "using [masked]\n");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_asking_for_secrets_on_an_unprotected_pipeline_fails_without_values() {
    let w = World::new(true).await;
    assert_eq!(
        w.set("DEPLOY_TOKEN", VALUE).await.status,
        StatusCode::CREATED
    );
    let token = w.runner().await;
    let pipeline = w.seed('b', false, &["DEPLOY_TOKEN"]).await;
    let claimed = w.claim(&token).await;
    assert_eq!(claimed.json, json!({"job": null}), "never handed out");
    assert_eq!(
        w.job_error(pipeline).await,
        ("failed".to_owned(), Some("FG-CI-0017".to_owned()))
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_asking_for_an_undefined_secret_fails() {
    let w = World::new(true).await;
    let token = w.runner().await;
    let pipeline = w.seed('b', true, &["NEVER_SET"]).await;
    assert_eq!(w.claim(&token).await.json, json!({"job": null}));
    assert_eq!(
        w.job_error(pipeline).await,
        ("failed".to_owned(), Some("FG-CI-0018".to_owned()))
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_whose_secrets_cannot_be_opened_fails_with_secret_key_unavailable() {
    // The value is set with keys; forge then runs without them.
    let keyed = World::new(true).await;
    assert_eq!(
        keyed.set("DEPLOY_TOKEN", VALUE).await.status,
        StatusCode::CREATED
    );
    let w = World {
        keys: false,
        ..keyed
    };
    let refused = w.set("OTHER_TOKEN", VALUE).await;
    assert_eq!(refused.code(), "FG-CI-0016", "{}", refused.json);
    let token = w.runner().await;
    let pipeline = w.seed('b', true, &["DEPLOY_TOKEN"]).await;
    assert_eq!(w.claim(&token).await.json, json!({"job": null}));
    assert_eq!(
        w.job_error(pipeline).await,
        ("failed".to_owned(), Some("FG-CI-0016".to_owned()))
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn ci_variables_refuse_developers_and_service_accounts() {
    let w = World::new(true).await;
    let sa = Principal::authenticated(
        w.owner,
        false,
        true,
        Credential::Token(vec![Scope::Api]),
        AccountKind::ServiceAccount,
    );
    let refused = w
        .call(
            "GET",
            &format!("/api/v1/projects/{}/ci/variables", w.web.get()),
            Who::As(sa),
            None,
        )
        .await;
    assert_eq!(refused.code(), "FG-AUTH-0003", "{}", refused.json);
    w.t.drop_now().await;
}

/// M4 C6 review (HIGH): an artifact of a job that was given secrets is
/// scanned for the released values (also base64 behind a prefix) and
/// refused; a clean one is stored but readable only by a Maintainer who is
/// a person — an agent's token is refused whatever the marks.
#[tokio::test]
async fn an_artifact_holding_a_released_secret_is_refused() {
    let w = World::new(true).await;
    assert_eq!(
        w.set("DEPLOY_TOKEN", VALUE).await.status,
        StatusCode::CREATED
    );
    let token = w.runner().await;
    let pipeline = w.seed('b', true, &["DEPLOY_TOKEN"]).await;
    let claimed = w.claim(&token).await;
    let job_id = claimed.json["job"]["job_id"].as_i64().expect("job");
    let job_token = claimed.json["job"]["job_token"]
        .as_str()
        .expect("token")
        .to_owned();
    let upload = |path: &str, bytes: Vec<u8>| {
        let uri = format!("/api/v1/runner/jobs/{job_id}/artifacts?path={path}");
        let who = Who::Bearer(job_token.clone());
        let w = &w;
        async move {
            w.send("PUT", &uri, who, Some((bytes, "application/octet-stream")))
                .await
        }
    };
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(format!("user:{VALUE}"));
    for (path, bytes) in [
        (
            "env.txt",
            format!("PATH=/bin\nDEPLOY_TOKEN={VALUE}\n").into_bytes(),
        ),
        (
            "auth.txt",
            format!("Authorization: Basic {b64}").into_bytes(),
        ),
    ] {
        let refused = upload(path, bytes).await;
        assert_eq!(
            (refused.status, refused.code()),
            (StatusCode::UNPROCESSABLE_ENTITY, "FG-CI-0021"),
            "{path}: {}",
            refused.json
        );
        assert!(!refused.json.to_string().contains(VALUE));
    }
    let clean = upload("report.txt", b"all tests passed".to_vec()).await;
    assert_eq!(clean.status, StatusCode::CREATED, "{}", clean.json);
    let base = format!(
        "/api/v1/projects/{}/pipelines/{pipeline}/jobs/{job_id}/artifacts",
        w.web.get()
    );
    let owner = w.call("GET", &base, Who::As(w.owner_token()), None).await;
    assert_eq!(owner.status, StatusCode::OK, "{}", owner.json);
    assert_eq!(owner.json["artifacts"].as_array().map(Vec::len), Some(1));
    let agent = w
        .call("GET", &base, Who::As(w.owner_token().as_agent(true)), None)
        .await;
    assert_eq!(agent.code(), "FG-AUTH-0022", "{}", agent.json);
    w.t.drop_now().await;
}
