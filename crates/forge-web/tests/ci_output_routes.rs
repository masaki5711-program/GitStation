#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The CI output routes (M4 C5, ADR-0029 decision 5, plan 14 §5 C5) through
//! the product router against the development database (ADR-0011), with
//! the real loaders, `forge-pipelines`, Smart HTTP's tip measurement and
//! real bare repositories: a job's log and a pipeline's diagnosis are read
//! by the project's readers, masked; an AI agent audience — an agent's
//! token, a service account, a person's token through `gstn mcp`
//! (`Forge-Context-Audience: agent`) — is refused with `FG-AUTH-0022` when
//! the pipeline's commit or the default branch's tip is marked
//! confidential (or undecided), and never sees the text. Principals are
//! inserted as request extensions. Synthetic data only.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal, Scope};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, Role, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_runners::lease::{self, JobResult};
use forge_db::ci_runners::{register, NewRunner};
use forge_db::pipelines::{self as db, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter, Pipelines};
use forge_web::{AppConfig, AppState, GitHttpLimits, Lookups, SmartHttp};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const JOB_TOKEN: &str =
    "fgj_0123456789abcdef_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const SECRET_LINE: &str = "the hidden build output";

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

/// `main` with these files at the root; the commit.
fn commit(repo: &Path, files: &[(&str, &str)]) -> String {
    let mut tree = String::new();
    for (name, body) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(body.as_bytes()),
        );
        tree.push_str(&format!("100644 blob {blob}\t{name}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(tree.as_bytes()));
    let commit = plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None);
    plumb(repo, &["update-ref", "refs/heads/main", &commit], None);
    commit
}

const CI_FILE: &str = "version: 1\n";
const CONFIDENTIAL: &str = "---\ntitle: Pricing\nclassification: confidential\n---\nsecret\n";

struct Answer {
    status: StatusCode,
    json: Value,
    raw: String,
}

impl Answer {
    fn code(&self) -> &str {
        self.json["error_code"].as_str().unwrap_or("")
    }
}

/// One failed job of a pipeline, its log appended by a runner.
#[derive(Debug, Clone, Copy)]
struct Ran {
    pipeline: i64,
    job: i64,
}

struct World {
    t: FreshDb,
    reporter: UserId,
    guest: UserId,
    outsider: UserId,
    sa: UserId,
    /// Its default branch's tip is open.
    web: ProjectId,
    /// Its default branch's tip holds a confidential document.
    vault: ProjectId,
    runner: i64,
    storage: StorageRoot,
    git: Git,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let owner = person(&t, "olive").await;
        let reporter = person(&t, "rita").await.user_id;
        let guest = person(&t, "gus").await.user_id;
        let outsider = person(&t, "otto").await.user_id;
        let sa = forge_db::service_accounts::create(
            &t.db,
            &ctx("req-sa"),
            &forge_db::service_accounts::NewServiceAccount {
                username: "ci-bot".to_owned(),
                display_name: "CI".to_owned(),
                owner: owner.user_id,
                purpose: "synthetic reader".to_owned(),
            },
            owner.user_id,
        )
        .await
        .expect("sa")
        .user_id;
        let home = tempfile::tempdir().expect("home");
        let root = tempfile::tempdir().expect("root");
        let git = Git::new(GitConfig::new("git", home.path()));
        let storage = StorageRoot::open(root.path()).expect("root");
        let mut ids = Vec::new();
        for (name, files) in [
            ("web", vec![(".forge-ci.yml", CI_FILE)]),
            (
                "vault",
                vec![(".forge-ci.yml", CI_FILE), ("pricing.md", CONFIDENTIAL)],
            ),
        ] {
            let p = projects::create(
                &t.db,
                &ctx(&format!("req-{name}")),
                &NewProject::new(name, owner.namespace_id, name, owner.user_id),
            )
            .await
            .expect("project");
            for (user, role) in [
                (reporter, Role::Reporter),
                (guest, Role::Guest),
                (sa, Role::Developer),
            ] {
                projects::add_member(&t.db, &ctx("req-member"), p.project_id, user, role)
                    .await
                    .expect("member");
            }
            let _ = forge_db::repositories::mark_ready(&t.db, &ctx("req-ready"), p.repository_id)
                .await
                .expect("ready");
            let path = storage
                .init_bare(&git, p.repository_id, uuid::Uuid::new_v4(), None)
                .await
                .expect("init");
            commit(&path, &files);
            ids.push(p.project_id);
        }
        let runner = register(
            &t.db,
            &ChangeContext::new(
                Actor::User(owner.user_id),
                RequestId::parse("req-runner").expect("id"),
            ),
            &NewRunner {
                name: "linux-1".to_owned(),
                description: String::new(),
                tags: Vec::new(),
            },
        )
        .await
        .expect("runner")
        .runner
        .id;
        Self {
            t,
            reporter,
            guest,
            outsider,
            sa,
            web: ids[0],
            vault: ids[1],
            runner,
            storage,
            git,
            _dirs: (home, root),
        }
    }

    /// A pipeline of `project` measured `mark`, whose one job a runner ran
    /// and failed after appending `chunks`.
    async fn failed(&self, project: ProjectId, mark: CopyMark, chunks: &[&str]) -> Ran {
        let pipeline = db::create(
            &self.t.db,
            &NewPipeline {
                request: PipelineRequest {
                    project,
                    git_ref: RefName::parse("refs/heads/main").expect("ref"),
                    sha: ObjectId::parse(&"b".repeat(40)).expect("sha"),
                    source: Source::Manual,
                    merge_request: None,
                    triggered_by: None,
                },
                request_id: "req-seed".to_owned(),
                definition_hash: Some(db::definition_hash(b"version: 1")),
                protected: false,
                copy_mark: mark,
                planned: Planned::Jobs(vec![NewCiJob {
                    name: "test".to_owned(),
                    stage: "test".to_owned(),
                    wave: 0,
                    needs: Vec::new(),
                    image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                    timeout_s: 600,
                    tags: Vec::new(),
                    secrets: Vec::new(),
                }]),
            },
        )
        .await
        .expect("pipeline")
        .pipeline_id;
        let claimed = lease::claim(&self.t.db, self.runner)
            .await
            .expect("claim")
            .expect("a job");
        assert_eq!(claimed.pipeline.id, pipeline);
        for (seq, body) in chunks.iter().enumerate() {
            lease::append_log(
                &self.t.db,
                self.runner,
                claimed.job_id,
                claimed.attempt,
                i64::try_from(seq).expect("seq"),
                body,
            )
            .await
            .expect("append");
        }
        lease::complete(
            &self.t.db,
            self.runner,
            claimed.job_id,
            claimed.attempt,
            JobResult::Failed,
        )
        .await
        .expect("complete");
        Ran {
            pipeline,
            job: claimed.job_id,
        }
    }

    fn state(&self) -> AppState {
        AppState::new(
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
        ))
        .with_smart_http(SmartHttp::new(
            self.git.clone(),
            self.storage.clone(),
            GitHttpLimits::default(),
        ))
    }

    async fn get(&self, uri: &str, who: Principal, agent_header: bool) -> Answer {
        let mut builder = Request::builder().method("GET").uri(uri);
        if agent_header {
            builder = builder.header("forge-context-audience", "agent");
        }
        let mut req = builder.body(Body::empty()).expect("request");
        req.extensions_mut().insert(who);
        let res = forge_web::app(self.state(), AppConfig::default())
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
        let raw = String::from_utf8(raw).expect("utf8");
        Answer {
            status,
            json: serde_json::from_str(&raw).unwrap_or(Value::Null),
            raw,
        }
    }

    async fn log(&self, project: ProjectId, ran: Ran, query: &str, who: Principal) -> Answer {
        self.get(&log_uri(project, ran, query), who, false).await
    }
}

fn log_uri(project: ProjectId, ran: Ran, query: &str) -> String {
    format!(
        "/api/v1/projects/{}/pipelines/{}/jobs/{}/log{query}",
        project.get(),
        ran.pipeline,
        ran.job
    )
}

fn diagnosis_uri(project: ProjectId, ran: Ran) -> String {
    format!(
        "/api/v1/projects/{}/pipelines/{}/diagnosis",
        project.get(),
        ran.pipeline
    )
}

fn session(user: UserId) -> Principal {
    Principal::authenticated(user, false, true, Credential::Session, AccountKind::Human)
}

fn token(user: UserId, kind: AccountKind) -> Principal {
    Principal::authenticated(
        user,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        kind,
    )
}

fn agent(user: UserId) -> Principal {
    token(user, AccountKind::Human).as_agent(true)
}

/// A reader gets the log page, masked; the cursor and `complete` say what
/// is left; a secret cut by a chunk's edge is masked on both sides.
#[tokio::test]
async fn a_reader_gets_the_log_masked_with_a_cursor() {
    let w = World::new().await;
    let cut = JOB_TOKEN.len() / 2;
    let ran = w
        .failed(
            w.web,
            CopyMark::Open,
            &[
                "running 4 tests\n",
                &format!("env: FORGE_JOB_TOKEN={}", &JOB_TOKEN[..cut]),
                &format!("{}\n", &JOB_TOKEN[cut..]),
                "test db::reload ... FAILED\n",
            ],
        )
        .await;
    let a = w.log(w.web, ran, "", session(w.reporter)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
    assert_eq!(a.json["job"], "test");
    assert_eq!(a.json["job_status"], "failed");
    assert_eq!(
        (a.json["from"].clone(), a.json["to"].clone()),
        (0.into(), 3.into())
    );
    assert_eq!(a.json["next_from"], Value::Null);
    assert_eq!(a.json["complete"], true);
    let text = a.json["text"].as_str().expect("text");
    assert!(text.contains("env: FORGE_JOB_TOKEN=[MASKED]"), "{text}");
    assert!(!a.raw.contains(&JOB_TOKEN[..20]), "{}", a.raw);
    assert!(!a.raw.contains(&JOB_TOKEN[cut..cut + 20]), "{}", a.raw);
    // A page starting at the token's second half masks it too.
    let a = w.log(w.web, ran, "?from=2", session(w.reporter)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
    assert!(
        a.json["text"]
            .as_str()
            .expect("text")
            .starts_with("[MASKED]\n"),
        "{}",
        a.raw
    );
    assert!(!a.raw.contains(&JOB_TOKEN[cut..cut + 20]), "{}", a.raw);
    // An attempt that never ran, a job of another pipeline: not found.
    let a = w.log(w.web, ran, "?attempt=2", session(w.reporter)).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{}",
        a.raw
    );
    let other = Ran {
        pipeline: ran.pipeline + 100,
        job: ran.job,
    };
    let a = w.log(w.web, other, "", session(w.reporter)).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND, "{}", a.raw);
    let a = w.log(w.web, ran, "?from=x", session(w.reporter)).await;
    assert_eq!(a.status, StatusCode::BAD_REQUEST, "{}", a.raw);
    w.t.drop_now().await;
}

/// ADR-0029 decision 5: an agent audience reads a CI output only when the
/// pipeline's commit and the default branch's tip are both open; a marked
/// one is FG-AUTH-0022 for an agent's token, a service account and a
/// person's token through `gstn mcp` — and the answer carries no log text.
/// A person reads it all.
#[tokio::test]
async fn an_agent_is_refused_the_logs_of_a_marked_pipeline_or_tip() {
    let w = World::new().await;
    let line = format!("{SECRET_LINE}\nerror: boom\n");
    let open = w.failed(w.web, CopyMark::Open, &[&line]).await;
    let marked = w.failed(w.web, CopyMark::Confidential, &[&line]).await;
    let undecided = w.failed(w.web, CopyMark::Undecided, &[&line]).await;
    let tip = w.failed(w.vault, CopyMark::Open, &[&line]).await;
    // Open pipeline, open tip: an agent's token reads it.
    let a = w.log(w.web, open, "", agent(w.reporter)).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
    assert!(a.raw.contains(SECRET_LINE));
    let refusals = [
        (
            w.web,
            marked,
            agent(w.reporter),
            false,
            "pipeline_confidential",
        ),
        (
            w.web,
            undecided,
            agent(w.reporter),
            false,
            "pipeline_undecided",
        ),
        (w.vault, tip, agent(w.reporter), false, "tip_confidential"),
        (
            w.vault,
            tip,
            token(w.sa, AccountKind::ServiceAccount),
            false,
            "tip_confidential",
        ),
        (
            w.web,
            marked,
            token(w.sa, AccountKind::ServiceAccount),
            false,
            "pipeline_confidential",
        ),
        // A person's own token through `gstn mcp`.
        (
            w.vault,
            tip,
            token(w.reporter, AccountKind::Human),
            true,
            "tip_confidential",
        ),
    ];
    for (project, ran, who, header, why) in refusals {
        for uri in [log_uri(project, ran, ""), diagnosis_uri(project, ran)] {
            let a = w.get(&uri, who.clone(), header).await;
            assert_eq!(
                (a.status, a.code()),
                (StatusCode::FORBIDDEN, "FG-AUTH-0022"),
                "{uri} {why}: {}",
                a.raw
            );
            assert!(!a.raw.contains(SECRET_LINE), "{}", a.raw);
            assert!(a.json["request_id"].as_str().is_some(), "{}", a.raw);
        }
    }
    // People read every one of them (a token without the header, a session).
    for (project, ran) in [(w.web, marked), (w.web, undecided), (w.vault, tip)] {
        for who in [session(w.reporter), token(w.reporter, AccountKind::Human)] {
            let a = w.log(project, ran, "", who).await;
            assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
            assert!(a.raw.contains(SECRET_LINE));
        }
    }
    // The project read comes first: a Guest is refused, an outsider gets
    // the invisible project's 404 — the marks are never an oracle.
    let a = w.log(w.vault, tip, "", session(w.guest)).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::FORBIDDEN, "FG-AUTH-0003"),
        "{}",
        a.raw
    );
    let a = w.log(w.vault, tip, "", agent(w.outsider)).await;
    assert_eq!(
        (a.status, a.code()),
        (StatusCode::NOT_FOUND, "FG-REPO-0005"),
        "{}",
        a.raw
    );
    w.t.drop_now().await;
}

/// Plan 04 P-9: the diagnosis names each failed job's fingerprint, error
/// codes and masked excerpt (pending until the worker fingerprinted it).
#[tokio::test]
async fn the_diagnosis_shows_the_fingerprinted_failure_masked() {
    let w = World::new().await;
    let ran = w
        .failed(
            w.web,
            CopyMark::Open,
            &[&format!(
                "build ok\nerror: pool timed out FG-DB-0003 token {JOB_TOKEN}\n"
            )],
        )
        .await;
    let a = w
        .get(&diagnosis_uri(w.web, ran), session(w.reporter), false)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
    assert_eq!(a.json["status"], "failed");
    assert_eq!(a.json["failures"][0]["pending"], true, "{}", a.raw);
    let done = forge_pipelines::outputs::fingerprint_failures(
        &w.t.db,
        &RequestId::parse("req-sweep").expect("id"),
    )
    .await
    .expect("sweep");
    assert_eq!(done.len(), 1);
    let a = w
        .get(&diagnosis_uri(w.web, ran), agent(w.reporter), false)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.raw);
    let f = &a.json["failures"][0];
    assert_eq!(f["pending"], false);
    assert_eq!(f["job"], "test");
    assert_eq!(f["fingerprint"], Value::String(done[0].fingerprint.clone()));
    assert_eq!(f["error_codes"], serde_json::json!(["FG-DB-0003"]));
    assert_eq!(f["occurrences"], 1);
    assert_eq!(f["source"]["lines"], serde_json::json!([2, 2]));
    assert!(
        f["source"]["log_path"]
            .as_str()
            .expect("path")
            .ends_with(&format!("/jobs/{}/log?attempt=1", ran.job)),
        "{f}"
    );
    let excerpt = f["excerpt"].as_str().expect("excerpt");
    assert!(
        excerpt.contains("pool timed out FG-DB-0003 token [MASKED]"),
        "{excerpt}"
    );
    assert!(!a.raw.contains(&JOB_TOKEN[..20]), "{}", a.raw);
    w.t.drop_now().await;
}

/// Review C5 (LOW): a pipeline or job of another project is the 404 of the
/// path's project — for a person and an agent alike, the log and the
/// diagnosis — and never another project's output.
#[tokio::test]
async fn another_projects_pipeline_or_job_is_not_found() {
    let w = World::new().await;
    let line = format!("{SECRET_LINE}\nerror: boom\n");
    let theirs = w.failed(w.vault, CopyMark::Open, &[&line]).await;
    let ours = w.failed(w.web, CopyMark::Open, &[&line]).await;
    let crossed = [
        theirs,
        Ran {
            pipeline: ours.pipeline,
            job: theirs.job,
        },
    ];
    for who in [|u| session(u), |u| agent(u)] {
        for ran in crossed {
            let a = w.log(w.web, ran, "", who(w.reporter)).await;
            assert_eq!(
                (a.status, a.code()),
                (StatusCode::NOT_FOUND, "FG-REPO-0005"),
                "{ran:?}: {}",
                a.raw
            );
            assert!(!a.raw.contains(SECRET_LINE), "{}", a.raw);
        }
        let a = w
            .get(&diagnosis_uri(w.web, theirs), who(w.reporter), false)
            .await;
        assert_eq!(
            (a.status, a.code()),
            (StatusCode::NOT_FOUND, "FG-REPO-0005"),
            "{}",
            a.raw
        );
    }
    w.t.drop_now().await;
}
