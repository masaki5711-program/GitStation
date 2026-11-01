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
}

impl PushRecorder for Flaky {
    fn record_push<'a>(
        &'a self,
        _: &'a Pusher,
        push: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async move {
            push.check()?;
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
        rewriter: ReportRewriter::new(false, BTreeSet::new(), String::new()),
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
