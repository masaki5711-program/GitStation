#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)
//! Recording an applied push (step 6): retries, dedup, and `FG-GIT-0012`
//! with the full list when the store stays down. The end-to-end rows (a
//! real push, a failing recorder) are in
//! `crates/forge-cli/tests/smart_http_e2e.rs`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use forge_core::namespace_resolver::LookupFuture;
use forge_core::{PushRecorder, RepoId, UserId};
use forge_git::{Git, GitConfig, StorageRoot};
use serde_json::Value;

use super::*;
use crate::routes::git::GitHttpLimits;

/// Fails the first `fail` calls (`FG-DB-0001`), then answers `then`.
struct Flaky {
    fail: u32,
    then: PushRecorded,
    calls: AtomicU32,
    seen: Mutex<Vec<PushRecord>>,
}

impl PushRecorder for Flaky {
    fn record_push<'a>(
        &'a self,
        _: &'a Pusher,
        push: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async move {
            push.check()?;
            self.seen.lock().expect("lock").push(push.clone());
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail {
                Err(ForgeError::new(ErrorCode::DbUnavailable).with_context("reason", "synthetic"))
            } else {
                Ok(self.then)
            }
        })
    }
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn events(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().expect("lock").clone())
            .expect("utf-8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect()
    }
}

fn receiver(dir: &std::path::Path, recorder: Arc<dyn PushRecorder>) -> Receiver {
    let git = Git::new(GitConfig::new("git", dir));
    let smart = SmartHttp::new(
        git,
        StorageRoot::open(dir).expect("root"),
        GitHttpLimits::default(),
    );
    Receiver {
        smart: Arc::new(smart),
        recorder,
        repository: ProjectRepository {
            id: RepoId::new(9),
            state: forge_core::RepositoryState::Ready,
        },
        principal: "user:4".to_owned(),
        pusher: Pusher {
            request_id: RequestId::parse("req-push-unit").expect("id"),
            user: UserId::new(4),
            credential_id: Some(2),
            source_ip: None,
            user_agent: None,
        },
        log: PushLog {
            request_id: RequestId::parse("req-push-unit").expect("id"),
            project_id: ProjectId::new(3),
            repo_id: 9,
            lang: Lang::En,
        },
        commands: Vec::new(),
        rewriter: ReportRewriter::new(false, BTreeSet::new(), RewriteTexts::default()),
        answer: None,
        bytes_in: 0,
    }
}

fn record() -> PushRecord {
    PushRecord {
        repository_id: RepoId::new(9),
        updates: vec![PushedRef {
            name: RefName::parse("refs/heads/main").expect("ref"),
            old: ObjectId::parse(&"a".repeat(40)).expect("oid"),
            new: ObjectId::parse(&"b".repeat(40)).expect("oid"),
            kind: PushUpdateKind::FastForward,
            commits: vec![ObjectId::parse(&"b".repeat(40)).expect("oid")],
            total_commits: 1,
            total_commits_partial: false,
        }],
    }
}

async fn run(fail: u32, then: PushRecorded) -> (u32, Vec<Value>) {
    let dir = tempfile::tempdir().expect("dir");
    let flaky = Arc::new(Flaky {
        fail,
        then,
        calls: AtomicU32::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let r = receiver(dir.path(), Arc::clone(&flaky) as Arc<dyn PushRecorder>);
    let buf = Buf::default();
    let writer = buf.clone(); // clone: the subscriber writes, the test reads
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    r.record(&record()).await;
    (flaky.calls.load(Ordering::SeqCst), buf.events())
}

#[tokio::test(start_paused = true)]
async fn a_push_is_recorded_after_transient_failures() {
    let (calls, events) = run(2, PushRecorded::Recorded).await;
    assert_eq!(calls, 3);
    let done: Vec<&Value> = events
        .iter()
        .filter(|e| e["message"] == "forge.git.push_recorded")
        .collect();
    assert_eq!(done.len(), 1, "{events:?}");
    assert_eq!(done[0]["attempts"], 3);
    assert!(
        events.iter().all(|e| e["error_code"].is_null()),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deduped_record_is_success() {
    let (calls, events) = run(0, PushRecorded::Deduped).await;
    assert_eq!(calls, 1);
    assert!(
        events
            .iter()
            .any(|e| e["message"] == "forge.git.push_recorded" && e["deduped"] == true),
        "{events:?}"
    );
}

/// The store stays down: three attempts, then one `FG-GIT-0012` with the
/// cause, the attempts and the request id (the replay finds its
/// `forge.git.push_applied` line by it).
#[tokio::test(start_paused = true)]
async fn a_push_that_stays_unrecorded_is_push_record_failed() {
    let (calls, events) = run(u32::MAX, PushRecorded::Recorded).await;
    assert_eq!(calls, RECORD_ATTEMPTS);
    let failed: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::PushRecordFailed.id())
        .collect();
    assert_eq!(failed.len(), 1, "{events:?}");
    let text = failed[0].to_string();
    assert!(text.contains("req-push-unit"), "{text}");
    assert!(text.contains("attempts=3"), "{text}");
    assert!(text.contains("cause_code=FG-DB-0001"), "{text}");
    assert!(text.contains("repo_id=9"), "{text}");
}

fn capture() -> (Buf, tracing::subscriber::DefaultGuard) {
    let buf = Buf::default();
    let writer = buf.clone(); // clone: the subscriber writes, the test reads
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_writer(move || writer.clone())
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}

/// git's answer could not be read (or git failed / was killed): the
/// repository says which updates were applied, and those are recorded —
/// the minimal `forge.git.push_applied` line first, then the final one.
#[tokio::test]
async fn an_unreadable_answer_is_reconciled_from_the_repository() {
    let dir = tempfile::tempdir().expect("dir");
    let flaky = Arc::new(Flaky {
        fail: 0,
        then: PushRecorded::Recorded,
        calls: AtomicU32::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let mut r = receiver(dir.path(), Arc::clone(&flaky) as Arc<dyn PushRecorder>);
    let repo = r
        .smart
        .storage
        .init_bare(&r.smart.git, RepoId::new(9), uuid_like(), None)
        .await
        .expect("init");
    // One commit on refs/heads/applied (synthetic).
    let run = |args: &[&str], stdin: Option<&str>| {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("--git-dir")
            .arg(&repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        if stdin.is_some() {
            cmd.stdin(std::process::Stdio::piped());
        }
        cmd.stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("git");
        if let Some(text) = stdin {
            use std::io::Write as _;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(text.as_bytes())
                .expect("write");
        }
        let out = child.wait_with_output().expect("git");
        assert!(out.status.success(), "{args:?}");
        String::from_utf8(out.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    };
    let tree = run(&["mktree"], Some(""));
    let commit = run(&["commit-tree", &tree, "-m", "synthetic"], None);
    run(&["update-ref", "refs/heads/applied", &commit], None);
    let zero = "0".repeat(40);
    r.commands = vec![
        (
            "refs/heads/applied".to_owned(),
            zero.clone(),
            commit.clone(),
        ),
        (
            "refs/heads/refused".to_owned(),
            zero.clone(),
            commit.clone(),
        ),
        ("refs/heads/gone".to_owned(), "c".repeat(40), zero),
    ];
    let (buf, _guard) = capture();
    r.reconcile("report_unreadable").await;
    let seen = flaky.seen.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1, "one record");
    let names: Vec<&str> = seen[0].updates.iter().map(|u| u.name.as_str()).collect();
    // The created ref holds its new object; the absent ref was deleted.
    assert_eq!(names, ["refs/heads/applied", "refs/heads/gone"]);
    let events = buf.events();
    let stages: Vec<&str> = events
        .iter()
        .filter(|e| e["event"] == forge_core::PUSH_APPLIED_EVENT)
        .map(|e| e["stage"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(stages, ["minimal", "final"]);
    assert!(events
        .iter()
        .any(|e| e["message"] == "forge.git.served" && e["source"] == "report_unreadable"));
}

/// The repository cannot be read either: every command is logged once with
/// FG-GIT-0012 (`reason=reconcile_failed`), nothing is recorded.
#[tokio::test]
async fn a_failed_reconciliation_logs_every_command_with_fg_git_0012() {
    let dir = tempfile::tempdir().expect("dir");
    let flaky = Arc::new(Flaky {
        fail: 0,
        then: PushRecorded::Recorded,
        calls: AtomicU32::new(0),
        seen: Mutex::new(Vec::new()),
    });
    // No repository was created for RepoId 9.
    let mut r = receiver(dir.path(), Arc::clone(&flaky) as Arc<dyn PushRecorder>);
    r.commands = vec![("refs/heads/main".to_owned(), "a".repeat(40), "b".repeat(40))];
    let (buf, _guard) = capture();
    r.reconcile("killed").await;
    assert_eq!(flaky.calls.load(Ordering::SeqCst), 0);
    let failed: Vec<Value> = buf
        .events()
        .into_iter()
        .filter(|e| e["error_code"] == ErrorCode::PushRecordFailed.id())
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    let text = failed[0].to_string();
    assert!(text.contains("reason=reconcile_failed"), "{text}");
    assert!(
        text.contains(&format!(
            "refs/heads/main {} {}",
            "a".repeat(40),
            "b".repeat(40)
        )),
        "{text}"
    );
}

fn uuid_like() -> uuid::Uuid {
    uuid::Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0009)
}

/// M2 MR7: however the push task ends, the repository's cached browse ref
/// list is dropped (the next browse request reads the refs again).
#[tokio::test]
async fn a_finished_push_task_drops_the_browse_ref_cache() {
    use crate::routes::browse::tests::{plumb, repo, REPO};
    let r = repo().await;
    let browse = forge_git::browse::Browser::new(
        r.git.clone(),     // clone: a shared handle
        r.storage.clone(), // clone: a path
        forge_git::browse::BrowseLimits::default(),
    );
    let id = RepoId::new(REPO);
    assert_eq!(browse.refs(id).await.expect("refs").refs.len(), 3);
    plumb(&r.path, &["update-ref", "-d", "refs/tags/v1"], None);
    assert_eq!(browse.refs(id).await.expect("cached").refs.len(), 3);
    drop(RefsChanged(browse.clone(), id)); // clone: the test keeps its handle
    assert_eq!(browse.refs(id).await.expect("fresh").refs.len(), 2);
}
