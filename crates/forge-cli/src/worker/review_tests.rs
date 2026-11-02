//! The production worker (`run`: the job handlers of this build and its
//! subscribers) against a migrated database and a temporary storage root
//! with real git: the `merge-request-refresh` subscriber is registered and
//! fed, opening a merge request records version 1, a push to its source
//! branch records version 2, each with its hidden refs, and doctor's
//! subscriber lag returns to 0 (M3 R1b).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_auth::{prove, Action, Credential, LoadedMergeRequest, LoadedProject, MergeRequestCtx};
use forge_core::{
    MergeRequestState, ObjectId, ProjectAccessLoader, ProjectRepository, PushRecord, PushRecorder,
    PushUpdateKind, PushedRef, Pusher, RefName, RepositoryState, RequestId,
};
use forge_db::review::{list_versions, REFRESH_SUBSCRIBER};
use forge_db::subscribers::{check_lag, states};
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_review::{NewFields, Review, ReviewRequest};
use tokio::sync::watch;

use super::retryable;
use crate::worker::provision::tests::{project, storage};
use crate::worker::run;
use crate::worker::seal_tests::seal_keys;

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

fn commit(repo: &Path, content: &str) -> String {
    let blob = plumb(
        repo,
        &["hash-object", "-w", "--stdin"],
        Some(content.as_bytes()),
    );
    let tree = plumb(
        repo,
        &["mktree"],
        Some(format!("100644 blob {blob}\tREADME.md\n").as_bytes()),
    );
    plumb(repo, &["commit-tree", &tree, "-m", "synthetic"], None)
}

fn hidden(repo: &Path) -> String {
    plumb(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/merge-requests/",
        ],
        None,
    )
}

/// Wait (bounded) until `done` returns `Some`.
async fn until<T, F, Fut>(what: &str, mut done: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(v) = done().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn transient_failures_are_retried_and_broken_jobs_are_not() {
    for code in [
        ErrorCode::GitTimeout,
        ErrorCode::GitBusy,
        ErrorCode::GitProcessFailed,
        ErrorCode::DbQueryFailed,
        ErrorCode::RepositoryNotReady,
    ] {
        assert!(retryable(&forge_errors::ForgeError::new(code)), "{code:?}");
    }
    for code in [ErrorCode::Internal, ErrorCode::NotFound] {
        assert!(!retryable(&forge_errors::ForgeError::new(code)), "{code:?}");
    }
}

#[tokio::test]
async fn the_worker_records_merge_request_versions_from_opens_and_pushes() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let web = project(&t, "alice").await;
    let repo = web.repository_id;
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker_storage = storage.clone(); // clone: the worker owns its handles
    let worker =
        tokio::spawn(async move { run(&db, seal_keys(), &worker_storage, None, stopped).await });

    // The provisioning job (also a production handler) creates the repository.
    let path = until("the repository", || async {
        let rows = t
            .json_rows(&format!(
                "SELECT state FROM forge.repositories WHERE id = {}",
                repo.get()
            ))
            .await;
        let ready = rows
            .first()
            .and_then(|r| r.get("state"))
            .and_then(|v| v.as_str())
            == Some("ready");
        ready.then(|| storage.root.existing(repo).expect("path"))
    })
    .await;
    let base = commit(&path, "base\n");
    let c1 = commit(&path, "first\n");
    plumb(&path, &["update-ref", "refs/heads/main", &base], None);
    plumb(&path, &["update-ref", "refs/heads/feature", &c1], None);

    // alice (the project's owner) opens a merge request.
    let rows = t
        .json_rows("SELECT id FROM forge.users WHERE username = 'alice'")
        .await;
    let alice = rows
        .first()
        .and_then(|r| r.get("id"))
        .and_then(serde_json::Value::as_i64)
        .expect("alice");
    let alice = forge_core::UserId::new(alice);
    let principal = forge_auth::Principal::authenticated(alice, false, true, Credential::Session);
    let record =
        t.db.load_project(web.project_id, Some(alice))
            .await
            .expect("load")
            .expect("project");
    let proof = prove(
        &principal,
        Action::MergeRequestCreate,
        MergeRequestCtx::new(
            LoadedProject {
                id: record.id,
                visibility: record.access.visibility,
                role: record.access.role,
                pending_deletion: record.access.pending_deletion,
            },
            LoadedMergeRequest {
                iid: None,
                author: alice,
                state: MergeRequestState::Open,
                draft: false,
                source_branch: RefName::parse("refs/heads/feature").expect("ref"),
                target_branch: RefName::parse("refs/heads/main").expect("ref"),
            },
        ),
    )
    .expect("proof");
    let browser = Browser::new(
        storage.git.clone(),  // clone: the browser owns its handles
        storage.root.clone(), // clone: as git
        BrowseLimits::default(),
    );
    let review = Review::new(t.db.clone(), Arc::new(browser));
    let mr = review
        .open(
            &ReviewRequest {
                request_id: RequestId::parse("req-open").expect("id"),
                credential_id: None,
            },
            &proof,
            ProjectRepository {
                id: repo,
                state: RepositoryState::Ready,
            },
            &NewFields {
                title: "Feature".to_owned(),
                description: String::new(),
            },
        )
        .await
        .expect("open");

    // The open's job records version 1 and its refs.
    until("version 1", || async {
        let refs = hidden(&path);
        (refs.contains(&format!("refs/merge-requests/1/v1 {c1}"))
            && refs.contains(&format!("refs/merge-requests/1/head {c1}")))
        .then_some(())
    })
    .await;

    // A push to the source: forge-web records it (here through the same
    // trait); the subscriber enqueues the job; it records version 2.
    let c2 = commit(&path, "second\n");
    plumb(&path, &["update-ref", "refs/heads/feature", &c2], None);
    let pusher = Pusher {
        request_id: RequestId::parse("req-push").expect("id"),
        user: alice,
        credential_id: None,
        source_ip: None,
        user_agent: None,
    };
    let recorder: &dyn PushRecorder = &t.db;
    let recorded = recorder
        .record_push(
            &pusher,
            &PushRecord {
                repository_id: repo,
                updates: vec![PushedRef {
                    name: RefName::parse("refs/heads/feature").expect("ref"),
                    old: ObjectId::parse(&c1).expect("oid"),
                    new: ObjectId::parse(&c2).expect("oid"),
                    kind: PushUpdateKind::FastForward,
                    commits: vec![ObjectId::parse(&c2).expect("oid")],
                    total_commits: 1,
                    total_commits_partial: false,
                }],
            },
        )
        .await
        .expect("record push");
    assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    until("version 2", || async {
        let refs = hidden(&path);
        (refs.contains(&format!("refs/merge-requests/1/v2 {c2}"))
            && refs.contains(&format!("refs/merge-requests/1/head {c2}"))
            && refs.contains(&format!("refs/merge-requests/1/v1 {c1}")))
        .then_some(())
    })
    .await;
    let versions = list_versions(&t.db, mr.id).await.expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].pushed_by, Some(alice), "the pusher");

    // doctor: the subscriber is registered and caught up.
    until("subscriber lag 0", || async {
        let all = states(&t.db).await.expect("states").expect("migrated");
        check_lag(&all).expect("no lag alert");
        all.iter()
            .find(|s| s.name == REFRESH_SUBSCRIBER)
            .filter(|s| s.lag_events == 0)
            .map(drop)
    })
    .await;

    stop.send(true).expect("worker listens");
    tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .expect("stops in time")
        .expect("no panic")
        .expect("clean stop");
    t.drop_now().await;
}

/// R1b turns subscribers on in production: the build's registry holds the
/// refresh subscriber, so the worker starts its `subscribers` task.
#[test]
fn the_production_registry_starts_the_subscribers_task() {
    let registry = crate::worker::registry().expect("registry");
    let names: Vec<&str> = registry.iter().map(|s| s.name()).collect();
    assert_eq!(names, [REFRESH_SUBSCRIBER]);
    let tasks = crate::worker::tasks(&registry, None);
    assert!(
        tasks.contains(&crate::worker::Task::Subscribers),
        "{tasks:?}"
    );
}
