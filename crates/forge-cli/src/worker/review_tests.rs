//! The production pieces of the worker (`jobs::handlers` and `registry`,
//! what `run` starts) against a migrated database and a temporary storage
//! root with real git, driven step by step (no polling, no wall clock):
//! the `merge-request-refresh` subscriber is registered and fed, opening a
//! merge request records version 1, a push to its source branch records
//! version 2, each with its hidden refs, and doctor's subscriber lag is 0
//! (M3 R1b).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use forge_auth::{prove, Action, Credential, LoadedMergeRequest, LoadedProject, MergeRequestCtx};
use forge_core::{
    MergeRequestState, ObjectId, ProjectAccessLoader, ProjectRepository, PushRecord, PushRecorder,
    PushUpdateKind, PushedRef, Pusher, RefName, RepositoryState, RequestId,
};
use forge_db::jobs::{JobRunner, Ran, WorkerId};
use forge_db::review::{list_versions, REFRESH_SUBSCRIBER};
use forge_db::sequencer::{self, AuditSequencing, BatchSize};
use forge_db::subscribers::{self, check_lag, register, states, Progress, Registry, Start};
use forge_db::test_support::FreshDb;
use forge_errors::ErrorCode;
use forge_git::browse::{BrowseLimits, Browser};
use forge_review::{NewFields, Review, ReviewRequest};

use super::retryable;
use crate::worker::jobs::handlers;
use crate::worker::provision::tests::{project, storage};
use crate::worker::registry;

pub(super) fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
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

pub(super) fn hidden(repo: &Path) -> String {
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

/// Run every step the worker would until nothing is left: number the
/// events, let each subscriber handle what waits for it, run every ready
/// job. A job that fails fails the test (nothing here should retry).
pub(super) async fn drain(t: &FreshDb, runner: &JobRunner, registry: &Registry) {
    loop {
        let mut progressed = false;
        let assigned = sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
            .await
            .expect("sequencer");
        progressed |= assigned.events.is_some();
        for subscriber in registry.iter() {
            loop {
                match subscribers::run_once(&t.db, subscriber.as_ref())
                    .await
                    .expect("subscriber step")
                {
                    Progress::Handled { .. } => progressed = true,
                    Progress::CaughtUp => break,
                    other @ (Progress::Busy
                    | Progress::Disabled
                    | Progress::Failed(_)
                    | Progress::Stuck(_)
                    | Progress::Quarantined(_)) => panic!("subscriber: {other:?}"),
                }
            }
        }
        while let Some(ran) = runner.run_once().await.expect("job runner") {
            match ran {
                Ran::Succeeded(_) => progressed = true,
                other @ (Ran::Retrying { .. }
                | Ran::Dead { .. }
                | Ran::Fenced(_)
                | Ran::Released { .. }
                | Ran::LeftLeased { .. }
                | Ran::Unrecorded { .. }) => panic!("job: {other:?}"),
            }
        }
        if !progressed {
            return;
        }
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
    // The whole set, over every registered code: a new code is permanent
    // until it is added here on purpose.
    let retried: Vec<&str> = ErrorCode::all()
        .filter(|c| retryable(&forge_errors::ForgeError::new(*c)))
        .map(ErrorCode::id)
        .collect();
    let mut expected = vec![
        ErrorCode::GitTimeout.id(),
        ErrorCode::GitBusy.id(),
        ErrorCode::GitProcessFailed.id(),
        ErrorCode::GitUnavailable.id(),
        ErrorCode::StorageIoFailed.id(),
        ErrorCode::DbUnavailable.id(),
        ErrorCode::DbQueryFailed.id(),
        ErrorCode::RepositoryNotReady.id(),
    ];
    let mut retried = retried;
    retried.sort_unstable();
    expected.sort_unstable();
    assert_eq!(retried, expected);
}

#[tokio::test]
async fn the_worker_records_merge_request_versions_from_opens_and_pushes() {
    let t = FreshDb::migrated().await;
    let (_dir, storage) = storage();
    let web = project(&t, "alice").await;
    let repo = web.repository_id;
    // What `run` starts: this build's handlers and subscribers, the
    // subscribers registered at the head before anything else.
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    for handler in handlers(&t.db, &storage).expect("handlers") {
        runner.register(handler).expect("register");
    }
    let registry = registry(&forge_core::config::ReviewConfig::default()).expect("registry");
    for subscriber in registry.iter() {
        register(&t.db, subscriber.as_ref(), Start::Head)
            .await
            .expect("register subscriber");
    }
    // The provisioning job (also a production handler) creates the repository.
    drain(&t, &runner, &registry).await;
    let path = storage.root.existing(repo).expect("provisioned");
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
    let principal = forge_auth::Principal::authenticated(
        alice,
        false,
        true,
        Credential::Session,
        forge_auth::AccountKind::Human,
    );
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
    drain(&t, &runner, &registry).await;
    let refs = hidden(&path);
    assert!(
        refs.contains(&format!("refs/merge-requests/1/v1 {c1}")),
        "{refs}"
    );
    assert!(
        refs.contains(&format!("refs/merge-requests/1/head {c1}")),
        "{refs}"
    );

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
    drain(&t, &runner, &registry).await;
    let refs = hidden(&path);
    for wanted in [
        format!("refs/merge-requests/1/v1 {c1}"),
        format!("refs/merge-requests/1/v2 {c2}"),
        format!("refs/merge-requests/1/head {c2}"),
    ] {
        assert!(refs.contains(&wanted), "{wanted}: {refs}");
    }
    let versions = list_versions(&t.db, mr.id).await.expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].pushed_by, Some(alice), "the pusher");

    // doctor: the subscriber is registered and caught up.
    let all = states(&t.db).await.expect("states").expect("migrated");
    check_lag(&all).expect("no lag alert");
    let refresh = all
        .iter()
        .find(|s| s.name == REFRESH_SUBSCRIBER)
        .expect("registered");
    assert_eq!(refresh.lag_events, 0);
    t.drop_now().await;
}

/// R1b turns subscribers on in production: the build's registry holds the
/// refresh subscriber, so the worker starts its `subscribers` task.
#[test]
fn the_production_registry_starts_the_subscribers_task() {
    let registry = registry(&forge_core::config::ReviewConfig::default()).expect("registry");
    let names: Vec<&str> = registry.iter().map(|s| s.name()).collect();
    assert_eq!(names, [REFRESH_SUBSCRIBER]);
    let tasks = crate::worker::tasks(&registry, None);
    assert!(
        tasks.contains(&crate::worker::Task::Subscribers),
        "{tasks:?}"
    );
}
