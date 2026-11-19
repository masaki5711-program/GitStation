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
        // Plan 18 S5: a branch one commit ahead, for a merge request.
        let feature = plumb(
            &path,
            &[
                "commit-tree",
                &tree,
                "-p",
                &commit,
                "-m",
                "synthetic feature",
            ],
            None,
        );
        plumb(&path, &["update-ref", "refs/heads/feature", &feature], None);
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
                ai_eval_job: None,
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
                    manual: false,
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
        self.call_at(method, &uri, who, body).await
    }

    /// [`Self::call`] on any path.
    async fn call_at(
        &self,
        method: &str,
        uri: &str,
        who: Principal,
        body: Option<Value>,
    ) -> Answer {
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
        // Plan 18 S5: the merge request routes (a merge request is opened
        // through its route: only forge-review calls the review commands).
        .with_review(
            forge_review::Review::new(
                self.t.db.clone(),
                Arc::new(Browser::new(
                    self.git.clone(),
                    self.storage.clone(),
                    BrowseLimits::default(),
                )),
            )
            .with_version_refs(forge_review::VersionRefs::new(
                self.git.clone(),
                self.storage.clone(),
            ))
            .with_review_git(forge_review::approvals::ReviewGit::new(
                self.git.clone(),
                self.storage.clone(),
            )),
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

// ── Plan 18 S5: the merge request pipeline on the current target ──

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

impl World {
    /// dave's merge request `feature` → `main` with its head `e…`
    /// recorded (no mergeability check yet); its iid and id.
    async fn merge_request(&self) -> (i64, forge_core::MergeRequestId) {
        let uri = format!("/api/v1/projects/{}/merge-requests", self.web.get());
        let opened = self
            .call_at(
                "POST",
                &uri,
                session(self.dev),
                Some(json!({"title": "Feature", "source_branch": "feature", "target_branch": "main"})),
            )
            .await;
        assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.json);
        let iid = opened.json["iid"].as_i64().expect("iid");
        let id = self
            .t
            .json_rows(&format!(
                "SELECT id FROM forge.merge_requests WHERE project_id = {} AND iid = {iid}",
                self.web.get()
            ))
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("merge request id");
        let mr = forge_core::MergeRequestId::new(id);
        // The version job's record (the test pins the head to `e…`).
        forge_db::review::record_mr_version(
            &self.t.db,
            &ctx("req-version"),
            mr,
            &oid('e'),
            Some(self.dev),
        )
        .await
        .expect("version");
        (iid, mr)
    }

    async fn ask_pipeline(&self, iid: i64, who: Principal) -> Answer {
        let uri = format!(
            "/api/v1/projects/{}/merge-requests/{iid}/pipeline",
            self.web.get()
        );
        self.call_at("POST", &uri, who, None).await
    }

    /// The payloads of the jobs of `kind`, oldest first.
    async fn jobs(&self, kind: &str) -> Vec<Value> {
        self.t
            .json_rows(&format!(
                "SELECT payload FROM forge.jobs WHERE kind = '{kind}' ORDER BY id"
            ))
            .await
            .into_iter()
            .map(|r| r["payload"].clone())
            .collect()
    }
}

/// Plan 18 S5: `POST …/merge-requests/{iid}/pipeline` is decided as a
/// pipeline run (`PipelineCreate`, the permission matrix's rows): a
/// Reporter, a Guest, a read-only token and an agent's read token are
/// refused and write nothing; a Developer is answered `202`.
#[tokio::test]
async fn the_merge_request_pipeline_route_is_decided_as_a_pipeline_run() {
    let w = World::new().await;
    let (iid, _) = w.merge_request().await;
    for (who, want) in [
        (session(w.reporter), (StatusCode::FORBIDDEN, "FG-AUTH-0003")),
        (session(w.guest), (StatusCode::FORBIDDEN, "FG-AUTH-0003")),
        (
            token(w.dev, &[Scope::ReadApi], AccountKind::Human),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
        (
            agent(w.dev, &[Scope::ReadApi]),
            (StatusCode::FORBIDDEN, "FG-AUTH-0002"),
        ),
    ] {
        let a = w.ask_pipeline(iid, who).await;
        assert_eq!((a.status, a.code()), want, "{}", a.json);
    }
    assert_eq!(w.audit().await, [], "a refusal writes nothing");
    assert_eq!(
        w.jobs("merge_request.mergeability").await,
        Vec::<Value>::new()
    );
    let a = w.ask_pipeline(iid, session(w.dev)).await;
    assert_eq!(a.status, StatusCode::ACCEPTED, "{}", a.json);
    assert_eq!(a.json["outcome"], "computing");
    // No such merge request.
    let a = w.ask_pipeline(99, session(w.dev)).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005")
    );
    w.t.drop_now().await;
}

/// Plan 18 S5: without a merge result for the current target the ask is a
/// check whose job carries it (`computing`); a second press asks nothing
/// more. With the merge result of the current target written (a target
/// move: no pipeline of its own), the ask enqueues exactly one pipeline on
/// it (`queued`), and a double click finds it (`exists`) — one audit row
/// per thing asked.
#[tokio::test]
async fn the_merge_request_pipeline_runs_once_on_the_current_target() {
    let w = World::new().await;
    let (iid, mr) = w.merge_request().await;
    let first = w.ask_pipeline(iid, session(w.dev)).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.json);
    assert_eq!(first.json["outcome"], "computing");
    let seq = first.json["check_seq"].as_i64().expect("seq");
    let again = w.ask_pipeline(iid, session(w.dev)).await;
    assert_eq!(again.json["outcome"], "computing");
    assert_eq!(again.json["check_seq"], seq, "a double click asks once");
    let checks = w.jobs("merge_request.mergeability").await;
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert_eq!(checks[0]["pipeline"], true);
    assert_eq!(checks[0]["check_seq"], seq);

    // The head's first merge result (target 1) got its pipeline; the target
    // moved to 2: a new merge result and none.
    let origin = RequestId::parse("req-mr").expect("id");
    for (target, result) in [('1', 'a'), ('2', 'c')] {
        forge_db::review::record_merge_result(
            &w.t.db,
            &origin,
            mr,
            (&oid('e'), &oid(target)),
            &oid(result),
            forge_db::review::MergeRequestPipeline::OnNewHead,
        )
        .await
        .expect("record")
        .expect("open");
    }
    let shas = |jobs: Vec<Value>| -> Vec<String> {
        jobs.iter()
            .map(|p| p["sha"].as_str().expect("sha").to_owned())
            .collect()
    };
    assert_eq!(shas(w.jobs("ci.pipeline_create").await), ["a".repeat(40)]);
    let seq = forge_db::review::request_check(&w.t.db, mr, RequestId::parse("req-q").expect("id"))
        .await
        .expect("request")
        .expect("open");
    // A later request (as a landing makes) replaces the asked job and
    // keeps the ask.
    let checks = w.jobs("merge_request.mergeability").await;
    let last = checks.last().expect("a job");
    assert_eq!(
        (last["check_seq"].as_i64(), last["pipeline"].clone()),
        (Some(seq), json!(true)),
        "{checks:?}"
    );
    let stored = forge_db::review::record_check(
        &w.t.db,
        &ctx("req-check"),
        mr,
        seq,
        &forge_db::review::CheckResult {
            status: forge_core::MergeabilityStatus::CanBeMerged,
            reason: None,
            head: Some(oid('e')),
            target: Some(oid('2')),
            conflicts: Vec::new(),
            conflicts_truncated: false,
            paths: None,
        },
    )
    .await
    .expect("check");
    assert!(matches!(stored, forge_db::review::Recorded::Stored { .. }));

    let queued = w.ask_pipeline(iid, session(w.dev)).await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{}", queued.json);
    assert_eq!(queued.json["outcome"], "queued");
    assert_eq!(queued.json["merge_result_sha"], "c".repeat(40));
    assert_eq!(queued.json["target_sha"], "2".repeat(40));
    // A double click, and a service account's press: the same pipeline.
    let twice = w.ask_pipeline(iid, session(w.dev)).await;
    assert_eq!(twice.json["outcome"], "exists", "{}", twice.json);
    let sa = w
        .ask_pipeline(iid, token(w.sa, &[Scope::Api], AccountKind::ServiceAccount))
        .await;
    assert_eq!(sa.json["outcome"], "exists", "{}", sa.json);
    assert_eq!(
        shas(w.jobs("ci.pipeline_create").await),
        ["a".repeat(40), "c".repeat(40)],
        "exactly one pipeline for the current target"
    );
    let asked: Vec<_> = w
        .audit()
        .await
        .into_iter()
        .filter(|(action, _, _)| action == "pipeline.run_merge_request")
        .collect();
    assert_eq!(asked.len(), 2, "computing once, queued once: {asked:?}");
    w.t.drop_now().await;
}
