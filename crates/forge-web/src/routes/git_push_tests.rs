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
            kind: forge_core::UserKind::Human,
            credential_id: Some(2),
            source_ip: None,
            user_agent: None,
        },
        log: PushLog {
            request_id: RequestId::parse("req-push-unit").expect("id"),
            project_id: ProjectId::new(3),
            repo_id: 9,
            lang: Lang::En,
            route: "receive_pack",
        },
        commands: Vec::new(),
        rewriter: ReportRewriter::new(false, BTreeSet::new(), RewriteTexts::default()),
        answer: None,
        bytes_in: 0,
        before: Ok(RefSnapshot::default()),
        snapshot_ms: 0,
        checks: None,
        hook_declined: false,
        history_import: None,
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
    r.record(&record(), &forge_core::PushedCommits::unknown())
        .await;
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

/// R5 review round 5: an incomplete listing (here: the ref snapshot before
/// the push failed) logs one FG-GIT-0016 warn with its reason, the rows
/// listed and the push's request id, and is recorded incomplete; every
/// listing logs the ref count and its durations.
#[tokio::test]
async fn an_incomplete_listing_logs_one_warn_with_its_reason() {
    let dir = tempfile::tempdir().expect("dir");
    let flaky = Arc::new(Flaky {
        fail: 0,
        then: PushRecorded::Recorded,
        calls: AtomicU32::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let mut r = receiver(dir.path(), Arc::clone(&flaky) as Arc<dyn PushRecorder>);
    r.before = Err(ForgeError::new(ErrorCode::GitTimeout)
        .with_context("subcommand", "for-each-ref")
        .with_context("refs", 1234));
    r.snapshot_ms = 7;
    let (buf, _guard) = capture();
    let commits = r.introduced(&record()).await;
    assert_eq!(commits, forge_core::PushedCommits::unknown());
    let events = buf.events();
    let warns: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::PushCommitsIncomplete.id())
        .collect();
    assert_eq!(warns.len(), 1, "{events:?}");
    let w = warns[0];
    assert_eq!(w["level"], "WARN");
    assert_eq!(w["request_id"], "req-push-unit");
    let text = w.to_string();
    for want in [
        "reason=snapshot_failed".to_owned(),
        format!("cause_code={}", ErrorCode::GitTimeout.id()),
        "listed=0".to_owned(),
        "refs=1234".to_owned(),
    ] {
        assert!(text.contains(&want), "{want}: {text}");
    }
    let listed: Vec<&Value> = events
        .iter()
        .filter(|e| e["message"] == "forge.git.push_commits_listed")
        .collect();
    assert_eq!(listed.len(), 1, "{events:?}");
    assert_eq!(listed[0]["snapshot_ms"], 7);
    assert_eq!(listed[0]["complete"], false);
    assert!(listed[0]["listing_ms"].is_u64());
    assert!(listed[0]["refs"].is_u64());
}

/// Records what each attempt was sent; the first call stalls for `stall`.
struct Slow {
    stall: Duration,
    sent: Mutex<Vec<(usize, bool)>>,
}

impl PushRecorder for Slow {
    fn record_push<'a>(
        &'a self,
        _: &'a Pusher,
        _: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async move { Ok(PushRecorded::Recorded) })
    }

    fn record_push_with_commits<'a>(
        &'a self,
        _: &'a Pusher,
        _: &'a PushRecord,
        commits: &'a forge_core::PushedCommits,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async move {
            let first = {
                let mut sent = self.sent.lock().expect("lock");
                sent.push((commits.commits.len(), commits.complete));
                sent.len() == 1
            };
            if first {
                tokio::time::sleep(self.stall).await;
            }
            Ok(PushRecorded::Recorded)
        })
    }
}

/// R5 review round 6: the attempts before the final one carry the listed
/// commits and end RECORD_FINAL_RESERVE early; the final one carries none.
#[test]
fn the_final_record_attempt_sends_no_commits() {
    let left = Duration::from_secs(30);
    for attempt in 1..RECORD_ATTEMPTS {
        let plan = attempt_plan(attempt, left);
        assert!(plan.with_commits);
        assert_eq!(plan.timeout, left - RECORD_FINAL_RESERVE);
    }
    let last = attempt_plan(RECORD_ATTEMPTS, left);
    assert!(!last.with_commits);
    assert_eq!(last.timeout, left);
    assert_eq!(
        attempt_plan(1, Duration::from_secs(5)).timeout,
        Duration::ZERO,
        "no time for a non-final attempt"
    );
}

/// R5 review round 6: a first attempt stalled by the rows (DB under load)
/// times out early; the final attempt, without the rows, still writes the
/// audit row, and the dropped rows are said once (FG-GIT-0016).
#[tokio::test(start_paused = true)]
async fn a_stalled_record_keeps_the_audit_row_without_the_commits() {
    let dir = tempfile::tempdir().expect("dir");
    let slow = Arc::new(Slow {
        stall: Duration::from_secs(120),
        sent: Mutex::new(Vec::new()),
    });
    let r = receiver(dir.path(), Arc::clone(&slow) as Arc<dyn PushRecorder>);
    let commits = forge_core::PushedCommits {
        commits: vec![ObjectId::parse(&"b".repeat(40)).expect("oid")],
        complete: true,
    };
    let (buf, _guard) = capture();
    r.record(&record(), &commits).await;
    let sent = slow.sent.lock().expect("lock").clone();
    assert_eq!(sent.first(), Some(&(1, true)), "{sent:?}");
    assert_eq!(sent.last(), Some(&(0, false)), "the final attempt: unknown");
    let events = buf.events();
    assert!(
        events
            .iter()
            .all(|e| e["error_code"] != ErrorCode::PushRecordFailed.id()),
        "{events:?}"
    );
    let dropped: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::PushCommitsIncomplete.id())
        .collect();
    assert_eq!(dropped.len(), 1, "{events:?}");
    assert!(dropped[0].to_string().contains("reason=record_fallback"));
    assert!(events
        .iter()
        .any(|e| e["message"] == "forge.git.push_recorded" && e["push_commits_dropped"] == true));
}

// ── M3 X8: what the push check run decided, logged once by serve ─────

/// A run for the fixture's repository (the hook script and the policy;
/// whether the hook then writes a result is up to the test).
fn check_run(r: &Receiver) -> forge_git::push_hook::PushCheckRun {
    r.smart.git.install_push_hook().expect("hook");
    r.smart
        .git
        .push_check_run(
            &r.smart.storage,
            &std::env::current_exe().expect("exe"),
            &PushCheckPolicy {
                request_id: "req-push-unit".to_owned(),
                lang: Lang::En,
                repo_id: RepoId::new(9),
                limits: ScanLimits::new(1),
                history_import: false,
            },
        )
        .expect("run")
}

/// git applied refs but the hook left no result: git skipped it. One
/// FG-GIT-0023 alert (`hook_not_run`), and the run directory is gone.
#[tokio::test]
async fn refs_applied_without_a_hook_result_is_hook_not_run() {
    let dir = tempfile::tempdir().expect("dir");
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    let run = check_run(&r);
    let run_dir = run.dir().to_path_buf();
    r.checks = Some(run);
    let (buf, _guard) = capture();
    r.push_checked(Some(2));
    let events = buf.events();
    let failed: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::PushCheckFailed.id())
        .collect();
    assert_eq!(failed.len(), 1, "{events:?}");
    let context = failed[0]["context"].as_str().unwrap_or_default();
    assert!(context.contains("reason=hook_not_run"), "{context}");
    assert!(context.contains("applied=2"), "{context}");
    assert!(!run_dir.exists());

    // Nothing applied and git did not run the hook (an unpack error):
    // nothing to say.
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    r.checks = Some(check_run(&r));
    let (buf, _guard) = capture();
    r.push_checked(Some(0));
    assert!(buf.events().is_empty());
    // git said the hook declined, but no result: it never got that far.
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    r.checks = Some(check_run(&r));
    r.hook_declined = true;
    let (buf, _guard) = capture();
    r.push_checked(Some(0));
    let events = buf.events();
    assert!(
        events
            .iter()
            .any(|e| e["error_code"] == ErrorCode::PushCheckFailed.id()
                && e["context"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("reason=hook_no_result")),
        "{events:?}"
    );
    // The repository could not say which refs changed (reconciliation
    // failed) and there is no result: never counted as "nothing applied".
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    r.checks = Some(check_run(&r));
    let (buf, _guard) = capture();
    r.push_checked(None);
    let events = buf.events();
    assert!(
        events
            .iter()
            .any(|e| e["error_code"] == ErrorCode::PushCheckFailed.id()
                && e["context"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("reason=hook_unknown")),
        "{events:?}"
    );
}

/// A refusal the hook wrote is logged with its first finding's code, the
/// rule and the path — never content.
#[tokio::test]
async fn a_refused_check_is_logged_with_its_first_finding() {
    let dir = tempfile::tempdir().expect("dir");
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    let run = check_run(&r);
    let result = format!(
        "version\t1\nnonce\t{}\nverdict\trefused\nreason\t-\ndetail\t-\ncause_code\t-\n\
         incomplete\t-\nobjects\t3\nblobs_scanned\t1\nbytes_scanned\t40\nallowed\t0\n\
         total_findings\t1\npack_objects\t3\nskipped_large\t0\nskipped_binary\t0\n\
         total_history\t0\nfinding\tsecret\tprivate_key\t{}\t{}\tdeploy/id_rsa\n",
        run.dir()
            .file_name()
            .and_then(|n| n.to_str())
            .expect("nonce"),
        "c".repeat(40),
        "d".repeat(40),
    );
    std::fs::write(run.dir().join("result"), result).expect("result");
    r.checks = Some(run);
    let (buf, _guard) = capture();
    r.push_checked(Some(0));
    let events = buf.events();
    let found: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::PushSecretDetected.id())
        .collect();
    assert_eq!(found.len(), 1, "{events:?}");
    let context = found[0]["context"].as_str().unwrap_or_default();
    for part in [
        "event=forge.git.push_checked",
        "verdict=refused",
        "first_rule=private_key",
        "first_path=deploy/id_rsa",
        "findings=1",
    ] {
        assert!(context.contains(part), "{part}: {context}");
    }
}

/// A recorder the check tests never reach.
struct DownRecorderForChecks;

impl PushRecorder for DownRecorderForChecks {
    fn record_push<'a>(
        &'a self,
        _: &'a Pusher,
        _: &'a PushRecord,
    ) -> LookupFuture<'a, PushRecorded> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::Internal).with_context("reason", "unused"))
        })
    }
}

// ── 2026-11-12: the one-time history import ─────────────────────────────

/// An allowance store whose record answers `answer` (and keeps what it got).
struct Imports {
    fail: bool,
    seen: Mutex<Vec<forge_core::HistoryImportUse>>,
    released: AtomicU32,
}

impl forge_core::HistoryImports for Imports {
    fn claim_import<'a>(&'a self, _: RepoId, _: &'a RequestId) -> LookupFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }

    fn release_import<'a>(&'a self, _: RepoId, _: &'a RequestId) -> LookupFuture<'a, ()> {
        Box::pin(async move {
            self.released.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn record_import<'a>(
        &'a self,
        _: &'a Pusher,
        _: RepoId,
        used: &'a forge_core::HistoryImportUse,
    ) -> LookupFuture<'a, forge_core::HistoryImportRecorded> {
        Box::pin(async move {
            self.seen.lock().expect("lock").push(used.clone());
            if self.fail {
                Err(ForgeError::new(ErrorCode::DbUnavailable).with_context("reason", "synthetic"))
            } else {
                Ok(forge_core::HistoryImportRecorded::Consumed)
            }
        })
    }
}

fn accepted_import() -> HookOutcome {
    HookOutcome {
        verdict: Verdict::Accepted,
        reason: None,
        detail: None,
        cause_code: None,
        report: forge_git::push_scan::ScanReport {
            history: vec![forge_git::push_scan::Finding {
                kind: forge_git::push_scan::FindingKind::Secret {
                    rule: "private_key".to_owned(),
                },
                blob: "e".repeat(40),
                path: "old/id_rsa".to_owned(),
                commit: Some("f".repeat(40)),
            }],
            total_history: 1,
            ..forge_git::push_scan::ScanReport::default()
        },
    }
}

#[tokio::test]
async fn an_accepted_import_is_recorded_and_its_findings_logged() {
    let dir = tempfile::tempdir().expect("dir");
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    let imports = Arc::new(Imports {
        fail: false,
        seen: Mutex::new(Vec::new()),
        released: AtomicU32::new(0),
    });
    r.history_import = Some(Arc::clone(&imports) as Arc<dyn forge_core::HistoryImports>);
    let outcome = accepted_import();
    let (buf, _guard) = capture();
    r.log_checked(&outcome, "1");
    r.history_imported(&outcome).await;
    let events = buf.events();
    let logged: Vec<&Value> = events
        .iter()
        .filter(|e| e["message"] == "forge.git.push_history_finding")
        .collect();
    assert_eq!(logged.len(), 1, "{events:?}");
    assert_eq!(logged[0]["path"], "old/id_rsa");
    assert_eq!(logged[0]["rule"], "private_key");
    assert_eq!(logged[0]["request_id"], "req-push-unit");
    assert!(
        events
            .iter()
            .any(|e| e["message"] == "forge.git.history_imported" && e["consumed"] == true),
        "{events:?}"
    );
    let seen = imports.seen.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].total, 1);
    assert_eq!(seen[0].findings[0].path, "old/id_rsa");
    assert_eq!(
        seen[0].findings[0].commit.as_deref(),
        Some("f".repeat(40).as_str())
    );
}

#[tokio::test]
async fn a_history_import_that_cannot_be_recorded_is_fg_git_0026() {
    let dir = tempfile::tempdir().expect("dir");
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    r.history_import = Some(Arc::new(Imports {
        fail: true,
        seen: Mutex::new(Vec::new()),
        released: AtomicU32::new(0),
    }));
    let (buf, _guard) = capture();
    r.history_imported(&accepted_import()).await;
    let events = buf.events();
    let failed: Vec<&Value> = events
        .iter()
        .filter(|e| e["error_code"] == ErrorCode::HistoryImportRecordFailed.id())
        .collect();
    assert_eq!(failed.len(), 1, "{events:?}");
    let context = failed[0]["context"].as_str().unwrap_or_default();
    assert!(context.contains("reason=record_failed"), "{context}");
    assert!(context.contains("history_findings=1"), "{context}");
    assert!(context.contains("cause_code=FG-DB-0001"), "{context}");
    // A refused push never uses the allowance.
    let (buf, _guard) = capture();
    let mut refused = accepted_import();
    refused.verdict = Verdict::Refused;
    r.history_imported(&refused).await;
    assert!(buf.events().is_empty());
}

/// A push under the claim that applied no ref gives the claim back; one
/// whose refs landed keeps it (used up, or never re-armed).
#[tokio::test]
async fn a_push_that_applied_nothing_releases_its_claim() {
    let dir = tempfile::tempdir().expect("dir");
    let imports = Arc::new(Imports {
        fail: false,
        seen: Mutex::new(Vec::new()),
        released: AtomicU32::new(0),
    });
    let mut r = receiver(dir.path(), Arc::new(DownRecorderForChecks));
    r.history_import = Some(Arc::clone(&imports) as Arc<dyn forge_core::HistoryImports>);
    let (buf, _guard) = capture();
    r.after_push(Vec::new(), "report").await;
    assert_eq!(imports.released.load(Ordering::SeqCst), 1);
    assert!(
        buf.events()
            .iter()
            .any(|e| e["message"] == "forge.git.history_import_released"),
        "{:?}",
        buf.events()
    );
    assert!(imports.seen.lock().expect("lock").is_empty());
}
