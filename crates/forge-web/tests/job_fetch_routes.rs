#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! A CI job's fetch of its pinned commit (M4 C4, ADR-0029 §3) through the
//! product router against the development database and a real bare
//! repository, with real `fgj_` tokens: the advertisement holds only
//! `refs/pipelines/<id>/head`; the pinned commit is served; a want outside
//! the pinned history is refused in-band; another project's or another
//! commit's job is `FG-REPO-0005`; an ended job's token is `FG-AUTH-0001`;
//! a runner's token and a person are refused. Synthetic data only.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use forge_auth::{AccountKind, Credential, Principal};
use forge_core::{ObjectId, ProjectId, RefName, RepoId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_runners::{self, lease, NewRunner};
use forge_db::pipelines::{self as db, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use forge_db::projects::{self, NewProject};
use forge_db::repositories;
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{BrowseLimits, Browser};
use forge_git::{Git, GitConfig, StorageRoot};
use forge_pipelines::{CopyMark, MarkFuture, MarkMeter, Pipelines};
use forge_web::routes::git::{GitHttpLimits, SmartHttp};
use forge_web::{AppConfig, AppState, Lookups};
use http_body_util::BodyExt;
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

fn commit_on(repo: &std::path::Path, parent: Option<&str>, content: &str) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME\n").as_bytes()),
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

struct Reply {
    status: StatusCode,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn code(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v["error_code"].as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

enum Who {
    As(Principal),
    Bearer(String),
}

struct World {
    t: FreshDb,
    ada: UserId,
    web: ProjectId,
    web_repo: RepoId,
    api: ProjectId,
    git: Git,
    storage: StorageRoot,
    c1: String,
    c2: String,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

fn job(name: &str) -> NewCiJob {
    NewCiJob {
        name: name.to_owned(),
        stage: "build".to_owned(),
        wave: 0,
        needs: Vec::new(),
        image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
        timeout_s: 600,
        tags: Vec::new(),
        secrets: Vec::new(),
    }
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let ada = users::create(
            &t.db,
            &ctx("req-user-ada"),
            &NewUser::human("ada", "Synthetic Ada", "ada@example.test"),
        )
        .await
        .expect("user");
        let web = projects::create(
            &t.db,
            &ctx("req-web"),
            &NewProject::new("web", ada.namespace_id, "Web", ada.user_id),
        )
        .await
        .expect("web");
        let api = projects::create(
            &t.db,
            &ctx("req-api"),
            &NewProject::new("api", ada.namespace_id, "Api", ada.user_id),
        )
        .await
        .expect("api");
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
        let c1 = commit_on(&path, None, "one\n");
        let c2 = commit_on(&path, Some(&c1), "two\n");
        plumb(&path, &["update-ref", "refs/heads/main", &c2], None);
        Self {
            t,
            ada: ada.user_id,
            web: web.project_id,
            web_repo: web.repository_id,
            api: api.project_id,
            git,
            storage,
            c1,
            c2,
            _dirs: (home, root),
        }
    }

    async fn pipeline(&self, project: ProjectId, sha: &str, name: &str) -> i64 {
        db::create(
            &self.t.db,
            &NewPipeline {
                ai_eval_job: None,
                request: PipelineRequest {
                    project,
                    git_ref: RefName::parse("refs/heads/main").expect("ref"),
                    sha: ObjectId::parse(sha).expect("sha"),
                    source: Source::Push,
                    merge_request: None,
                    triggered_by: None,
                },
                request_id: format!("req-seed-{name}"),
                definition_hash: Some(db::definition_hash(b"version: 1")),
                protected: false,
                copy_mark: CopyMark::Open,
                planned: Planned::Jobs(vec![job(name)]),
            },
        )
        .await
        .expect("seed")
        .pipeline_id
    }

    async fn call(&self, method: &str, uri: &str, who: Who, body: Option<Vec<u8>>) -> Reply {
        self.call_as(
            method,
            uri,
            who,
            body,
            "application/x-git-upload-pack-request",
        )
        .await
    }

    async fn call_as(
        &self,
        method: &str,
        uri: &str,
        who: Who,
        body: Option<Vec<u8>>,
        content_type: &str,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Who::Bearer(token) = &who {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let mut req = match body {
            Some(b) => builder
                .header("content-type", content_type)
                .body(Body::from(b)),
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
        .with_smart_http(SmartHttp::new(
            self.git.clone(),
            self.storage.clone(),
            GitHttpLimits::default(),
        ))
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
        let body = res
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        Reply { status, body }
    }
}

fn info_refs(job: i64) -> String {
    format!("/api/v1/runner/jobs/{job}/repo.git/info/refs?service=git-upload-pack")
}

fn rpc(job: i64) -> String {
    format!("/api/v1/runner/jobs/{job}/repo.git/git-upload-pack")
}

fn pkt(line: &str) -> String {
    format!("{:04x}{line}", line.len() + 4)
}

/// A protocol v0 fetch of `want` (no haves).
fn want(oid: &str) -> Vec<u8> {
    format!(
        "{}0000{}",
        pkt(&format!("want {oid} ofs-delta\n")),
        pkt("done\n")
    )
    .into_bytes()
}

#[tokio::test]
async fn a_job_fetches_only_its_pinned_commit_while_it_lives() {
    let w = World::new().await;
    let runner = ci_runners::register(
        &w.t.db,
        &ChangeContext::new(Actor::User(w.ada), RequestId::parse("req-reg").expect("id")),
        &NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner");
    let own = w.pipeline(w.web, &w.c1, "build").await;
    let other_project = w.pipeline(w.api, &"d".repeat(40), "api").await;
    let other_sha = w.pipeline(w.web, &w.c2, "next").await;
    let mut claimed = Vec::new();
    for _ in 0..3 {
        claimed.push(
            lease::claim(&w.t.db, runner.runner.id)
                .await
                .expect("claim")
                .expect("job"),
        );
    }
    let mine = claimed
        .iter()
        .find(|c| c.pipeline.id == own)
        .expect("own job");
    let token = mine.token.reveal();
    let job = mine.job_id;

    // The advertisement: exactly the pipeline's ref.
    let adv = w
        .call("GET", &info_refs(job), Who::Bearer(token.clone()), None)
        .await;
    assert_eq!(adv.status, StatusCode::OK, "{}", adv.text());
    let refs: String = adv
        .text()
        .lines()
        .map(|l| l.split('\0').next().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        refs.contains(&format!("{} refs/pipelines/{own}/head", w.c1)),
        "{refs}"
    );
    assert!(!refs.contains("refs/heads/main"), "{refs}");
    assert!(!refs.contains(&w.c2), "{refs}");
    assert!(
        !refs.contains(&format!("refs/pipelines/{other_sha}/")),
        "{refs}"
    );

    // The pinned commit is served.
    let pack = w
        .call(
            "POST",
            &rpc(job),
            Who::Bearer(token.clone()),
            Some(want(&w.c1)),
        )
        .await;
    assert_eq!(pack.status, StatusCode::OK);
    assert!(
        pack.body.windows(4).any(|b| b == b"PACK"),
        "{}",
        pack.text()
    );
    // A newer commit of the same branch is not the job's: refused in-band.
    let refused = w
        .call(
            "POST",
            &rpc(job),
            Who::Bearer(token.clone()),
            Some(want(&w.c2)),
        )
        .await;
    assert_eq!(refused.status, StatusCode::OK);
    assert!(refused.text().contains("ERR"), "{}", refused.text());
    assert!(!refused.body.windows(4).any(|b| b == b"PACK"));

    // Another project's job and another commit's job: the invisible
    // project's 404, before any git runs.
    for other in [other_project, other_sha] {
        let other_job = claimed
            .iter()
            .find(|c| c.pipeline.id == other)
            .expect("job")
            .job_id;
        let r = w
            .call(
                "GET",
                &info_refs(other_job),
                Who::Bearer(token.clone()),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.text());
        assert_eq!(r.code(), "FG-REPO-0005");
        let r = w
            .call(
                "POST",
                &rpc(other_job),
                Who::Bearer(token.clone()),
                Some(want(&w.c1)),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    // No such job: the same 404.
    let r = w
        .call(
            "GET",
            &info_refs(job + 1000),
            Who::Bearer(token.clone()),
            None,
        )
        .await;
    assert_eq!(r.code(), "FG-REPO-0005");

    // Not a job: the runner's own token, a person.
    let r = w
        .call(
            "GET",
            &info_refs(job),
            Who::Bearer(runner.token.reveal()),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());
    assert_eq!(r.code(), "FG-AUTH-0003");
    let person =
        Principal::authenticated(w.ada, true, true, Credential::Session, AccountKind::Human);
    let r = w.call("GET", &info_refs(job), Who::As(person), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text());

    // A service other than upload-pack is refused.
    let r = w
        .call(
            "GET",
            &format!("/api/v1/runner/jobs/{job}/repo.git/info/refs?service=git-receive-pack"),
            Who::Bearer(token.clone()),
            None,
        )
        .await;
    assert_eq!(r.code(), "FG-WEB-0007");

    // The job ends: its token is revoked with it, and the next fetch is
    // unauthenticated.
    lease::complete(
        &w.t.db,
        runner.runner.id,
        job,
        mine.attempt,
        lease::JobResult::Succeeded,
    )
    .await
    .expect("complete");
    let r = w
        .call("GET", &info_refs(job), Who::Bearer(token.clone()), None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.text());
    assert_eq!(r.code(), "FG-AUTH-0001");
    w.t.drop_now().await;
}

/// M4 C4 review: the pipeline ref pins its commit only while the pipeline
/// runs; the completion that ends the pipeline deletes it.
#[tokio::test]
async fn the_pipeline_ref_is_deleted_when_the_pipeline_ends() {
    let w = World::new().await;
    let runner = ci_runners::register(
        &w.t.db,
        &ChangeContext::new(Actor::User(w.ada), RequestId::parse("req-reg").expect("id")),
        &NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner");
    let own = w.pipeline(w.web, &w.c1, "build").await;
    let claimed = lease::claim(&w.t.db, runner.runner.id)
        .await
        .expect("claim")
        .expect("job");
    let job = claimed.job_id;
    let adv = w
        .call(
            "GET",
            &info_refs(job),
            Who::Bearer(claimed.token.reveal()),
            None,
        )
        .await;
    assert_eq!(adv.status, StatusCode::OK, "{}", adv.text());
    let repo = w.web_repo;
    let name = RefName::parse(&format!("refs/pipelines/{own}/head")).expect("ref");
    let pinned = |w: &World| {
        let (git, storage, name) = (w.git.clone(), w.storage.clone(), name.clone());
        async move {
            git.read_ref(&storage, repo, &name, std::time::Duration::from_secs(20))
                .await
                .expect("read")
        }
    };
    assert_eq!(
        pinned(&w).await.map(|o| o.as_str().to_owned()),
        Some(w.c1.clone())
    );
    let done = w
        .call_as(
            "POST",
            &format!("/api/v1/runner/jobs/{job}/complete"),
            Who::Bearer(runner.token.reveal()),
            Some(
                serde_json::json!({"attempt": claimed.attempt, "result": "succeeded", "exit_code": 0})
                    .to_string()
                    .into_bytes(),
            ),
            "application/json",
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text());
    assert!(done.text().contains("succeeded"), "{}", done.text());
    assert_eq!(pinned(&w).await, None, "the ended pipeline's ref is gone");
    w.t.drop_now().await;
}

/// M4 C7: a merge request pipeline runs on the merge result, a commit only
/// the hidden `refs/merge-requests/<iid>/merge` reaches (no branch has it).
/// Its job fetches exactly that commit through its pipeline ref.
#[tokio::test]
async fn a_merge_request_job_fetches_its_merge_result_commit() {
    let w = World::new().await;
    let path = w.storage.existing(w.web_repo).expect("repo path");
    let side = commit_on(&path, Some(&w.c1), "side\n");
    let tree = plumb(&path, &["rev-parse", &format!("{side}^{{tree}}")], None);
    let merged = plumb(
        &path,
        &[
            "commit-tree",
            tree.as_str(),
            "-m",
            "merge result",
            "-p",
            &w.c2,
            "-p",
            &side,
        ],
        None,
    );
    plumb(
        &path,
        &["update-ref", "refs/merge-requests/1/merge", &merged],
        None,
    );
    let runner = ci_runners::register(
        &w.t.db,
        &ChangeContext::new(Actor::User(w.ada), RequestId::parse("req-reg").expect("id")),
        &NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner");
    let own = w.pipeline(w.web, &merged, "test").await;
    let claimed = lease::claim(&w.t.db, runner.runner.id)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(claimed.pipeline.id, own);
    let token = claimed.token.reveal();
    let adv = w
        .call(
            "GET",
            &info_refs(claimed.job_id),
            Who::Bearer(token.clone()),
            None,
        )
        .await;
    assert_eq!(adv.status, StatusCode::OK, "{}", adv.text());
    assert!(
        adv.text()
            .contains(&format!("{merged} refs/pipelines/{own}/head")),
        "{}",
        adv.text()
    );
    assert!(
        !adv.text().contains("refs/merge-requests"),
        "{}",
        adv.text()
    );
    let pack = w
        .call(
            "POST",
            &rpc(claimed.job_id),
            Who::Bearer(token),
            Some(want(&merged)),
        )
        .await;
    assert_eq!(pack.status, StatusCode::OK);
    assert!(
        pack.body.windows(4).any(|b| b == b"PACK"),
        "{}",
        pack.text()
    );
    w.t.drop_now().await;
}
