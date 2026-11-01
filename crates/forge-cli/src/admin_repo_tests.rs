//! `forge admin repo reprovision` against a migrated database: a dead job
//! gets a new attempt that the worker's handler then completes; a live job
//! and a ready repository change nothing.

use std::sync::Arc;

use forge_db::jobs::{self, JobRunner, Ran, WorkerId};
use forge_db::test_support::{assert_reason, FreshDb};
use forge_errors::ErrorCode;

use super::*;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::{project, storage};
use crate::worker::provision::ProvisionHandler;

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

#[tokio::test]
async fn reprovision_requeues_a_dead_job() {
    let t = FreshDb::migrated().await;
    let (_root, storage) = storage();
    let p = project(&t, "sam").await;
    let repo = p.repository_id;
    // The creating job is still queued: nothing to do.
    let out = reprovision_with(&t.db, &rid("req-cli-rp-1"), repo.get())
        .await
        .expect("live");
    assert!(out.contains("is already queued"), "{out}");
    // It dies (as after FG-REPO-0004 adopt_refused, once the operator has
    // dealt with the directory).
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-REPO-0004' WHERE id = {}",
        p.provision_job.get()
    ))
    .await;
    let logs = Captured::default();
    let guard = logs.install();
    let out = reprovision_with(&t.db, &rid("req-cli-rp-2"), repo.get())
        .await
        .expect("requeued");
    drop(guard);
    assert!(out.contains("queued"), "{out}");
    let events = logs.events();
    let logged: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["event"] == "forge.admin.repo.reprovisioned")
        .collect();
    assert_eq!(logged.len(), 1, "{events:?}");
    assert_eq!(logged[0]["fields"]["outcome"], "queued");
    assert_eq!(logged[0]["fields"]["request_id"], "req-cli-rp-2");
    // The worker's handler runs the new job and the repository is ready.
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    let handler = ProvisionHandler::new(&t.db, storage.clone()).expect("handler");
    runner.register(Arc::new(handler)).expect("register");
    let ran = runner.run_once().await.expect("claim").expect("a job");
    assert!(
        matches!(ran, Ran::Succeeded(id) if id != p.provision_job),
        "{ran:?}"
    );
    let target = repositories::provision_target(&t.db, repo)
        .await
        .expect("target");
    assert_eq!(target.state, forge_core::RepositoryState::Ready);
    let health = jobs::health(&t.db)
        .await
        .expect("health")
        .expect("migrated");
    assert_eq!(health.dead_total, 1, "the old dead job stays as it was");
    // Ready now: refused with a clear code, nothing enqueued.
    let err = reprovision_with(&t.db, &rid("req-cli-rp-3"), repo.get())
        .await
        .expect_err("ready");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "repository_ready");
    let err = reprovision_with(&t.db, &rid("req-cli-rp-4"), 987_654)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    t.drop_now().await;
}

// ── M1 phase 9 MR2: record-push ──

mod record_push {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use forge_core::{ObjectId, PushUpdateKind, PushedRef, RefName, UserId};
    use forge_db::test_support::{assert_reason, FreshDb};
    use forge_errors::ErrorCode;

    use super::super::*;
    use crate::worker::provision::tests::project;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The line forge-web logs, in the production JSON format.
    fn logged(applied: &PushApplied) -> String {
        let buf = Buf::default();
        let writer = buf.clone(); // clone: the subscriber writes, the test reads
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || applied.log().expect("log"));
        let bytes = buf.0.lock().expect("lock").clone();
        String::from_utf8(bytes).expect("utf-8")
    }

    fn oid(c: char) -> ObjectId {
        ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
    }

    fn push_line(repo: RepoId, project: ProjectId, user: UserId, request: &str) -> String {
        let pusher = Pusher {
            request_id: RequestId::parse(request).expect("request id"),
            user,
            credential_id: None,
            source_ip: None,
            user_agent: None,
        };
        let push = PushRecord {
            repository_id: repo,
            updates: vec![PushedRef {
                name: RefName::parse("refs/heads/main").expect("ref"),
                old: oid('a'),
                new: oid('b'),
                kind: PushUpdateKind::FastForward,
                commits: vec![oid('b')],
                total_commits: 1,
            }],
        };
        logged(&PushApplied::new(&pusher, project, &push))
            .trim_end()
            .to_owned()
    }

    async fn pushed_rows(t: &FreshDb) -> Vec<serde_json::Value> {
        t.json_rows(
            "SELECT actor_kind, actor_user_id, request_id, actor_label FROM forge.audit_log \
             WHERE action = 'repository.pushed' ORDER BY id",
        )
        .await
    }

    async fn pushed_events(t: &FreshDb) -> usize {
        t.json_rows("SELECT id FROM forge.events WHERE event_type = 'repository.pushed'")
            .await
            .len()
    }

    #[tokio::test]
    async fn record_push_cli_replays_once() {
        let t = FreshDb::migrated().await;
        let p = project(&t, "rita").await;
        let owner = t
            .json_rows("SELECT id FROM forge.users WHERE username = 'rita'")
            .await[0]["id"]
            .as_i64()
            .expect("owner");
        let user = UserId::new(owner);
        let line = push_line(p.repository_id, p.project_id, user, "req-push-lost");
        // A log excerpt: other events and plain text around the push line.
        let text = format!(
            "{{\"event\":\"forge.http.request\",\"request_id\":\"req-push-lost\"}}\n\
             not json at all\n{line}\n"
        );
        let cli = RequestId::parse("req-cli-replay-1").expect("id");
        let out = record_push_with(&t.db, &cli, &text)
            .await
            .expect("replayed");
        assert!(out.contains("push req-push-lost"), "{out}");
        assert!(out.contains(": recorded"), "{out}");
        // The runbook step run again (or the push was recorded after all).
        let again = RequestId::parse("req-cli-replay-2").expect("id");
        let out = record_push_with(&t.db, &again, &text).await.expect("again");
        assert!(out.contains("already recorded (nothing changed)"), "{out}");
        let rows = pushed_rows(&t).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row["actor_kind"], "user");
        assert_eq!(row["actor_user_id"], owner);
        // The logged request id: the audit row joins the push's log lines.
        assert_eq!(row["request_id"], "req-push-lost");
        assert!(
            row["actor_label"]
                .as_str()
                .is_some_and(|l| l.starts_with("forge-cli:")),
            "{row}"
        );
        assert_eq!(pushed_events(&t).await, 1);
        t.drop_now().await;
    }

    #[tokio::test]
    async fn record_push_refuses_a_log_it_cannot_replay() {
        let t = FreshDb::migrated().await;
        let p = project(&t, "sven").await;
        let cli = RequestId::parse("req-cli-replay-3").expect("id");
        let err = record_push_with(&t.db, &cli, "nothing here\n")
            .await
            .expect_err("empty");
        assert_reason(&err, ErrorCode::PushRecordFailed, "push_log_empty");
        let good = push_line(p.repository_id, p.project_id, UserId::new(1), "req-push-ok");
        let broken = r#"{"event":"forge.git.push_applied","request_id":"r","push":"{"}"#;
        let err = record_push_with(&t.db, &cli, &format!("{good}\n{broken}\n"))
            .await
            .expect_err("broken");
        assert_reason(&err, ErrorCode::PushRecordFailed, "push_log_invalid");
        assert!(err.to_string().contains("line=2"), "{err}");
        // Checked before anything is written: the good line was not recorded.
        assert!(pushed_rows(&t).await.is_empty());
        let missing = std::env::temp_dir().join("forge-no-such-push-log-file");
        let err = read_log(&missing).expect_err("missing");
        assert_reason(&err, ErrorCode::PushRecordFailed, "push_log_unreadable");
        t.drop_now().await;
    }
}
