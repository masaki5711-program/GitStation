//! JobRunner when an abort or the bookkeeping after it does not go through:
//! a handler that does not end keeps its job leased (after a timeout and
//! after a stop), a hand-back that does not answer, a hand-back fenced by a
//! newer claim, and a result that cannot be recorded. Blocking handlers are
//! gated by channels, never by sleeps.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_errors::ErrorCode;
use serde_json::Value;
use sqlx::Connection as _;
use tokio::sync::{oneshot, Notify};

use super::runner_stop_tests::Captured;
use super::runner_tests::{has, runner, Script};
use super::tests::{expire, kind, new_job, queued, ready_now, state};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(5);

/// Signals its start, then blocks its thread (ignoring any abort) until the
/// gate opens.
fn gated(started: Arc<Notify>, gate: mpsc::Receiver<()>) -> Script {
    let gate = Arc::new(Mutex::new(gate));
    Arc::new(move |_| {
        let (started, gate) = (Arc::clone(&started), Arc::clone(&gate));
        Box::pin(async move {
            started.notify_one();
            // A closed gate opens it too.
            let _ = gate.lock().map(|g| g.recv());
            std::future::pending::<()>().await;
            Ok(())
        })
    })
}

/// Failure events whose context has `reason=<reason>`.
fn with_reason(logs: &Captured, reason: &str) -> Vec<Value> {
    logs.failures()
        .into_iter()
        .filter(|e| {
            e["fields"]["context"]
                .as_str()
                .is_some_and(|c| c.split(' ').any(|kv| kv == format!("reason={reason}")))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_handler_that_does_not_end_keeps_its_job_leased() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, Duration::from_millis(100))).await;
    let (open, gate) = mpsc::channel();
    let r = runner(&t, "test.a", gated(Arc::new(Notify::new()), gate))
        .with_abort_wait(Duration::from_millis(100));
    let logs = Captured::default();
    let ran = {
        let _guard = logs.install();
        r.run_once().await.expect("run")
    };
    match &ran {
        Some(Ran::LeftLeased { job_id, report }) => {
            assert_eq!(*job_id, id);
            assert_reason(report, ErrorCode::JobLeaseExpired, "left_leased");
            assert!(has(report, "after", "execution_timeout"), "{report}");
        }
        other => panic!("{other:?}"),
    }
    // Still leased: nobody can claim it while the old run may work.
    let (st, attempts, worker, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts), ("running", 1));
    assert_eq!(worker.as_deref(), Some(r.worker_id().as_str()));
    for reason in ["execution_timeout", "abort_pending", "left_leased"] {
        let events = with_reason(&logs, reason);
        assert_eq!(events.len(), 1, "{reason}: {:?}", logs.failures());
        assert_eq!(events[0]["fields"]["error_code"], "FG-DB-0007");
        assert_eq!(events[0]["fields"]["request_id"], "req-job-origin");
        let context = events[0]["fields"]["context"].as_str().unwrap_or_default();
        assert!(
            context.contains(&format!("job_id={}", id.get())),
            "{context}"
        );
        assert!(context.contains("kind=test.a"), "{context}");
    }
    open.send(()).expect("open the gate");
    t.drop_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_handler_that_does_not_end_keeps_its_job_leased() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let started = Arc::new(Notify::new());
    let (open, gate) = mpsc::channel();
    let r = runner(&t, "test.a", gated(Arc::clone(&started), gate))
        .with_abort_wait(Duration::from_millis(100));
    let ran = r
        .run_once_until(started.notified(), Duration::from_millis(50))
        .await
        .expect("run");
    match &ran {
        Some(Ran::LeftLeased { report, .. }) => {
            assert_reason(report, ErrorCode::JobLeaseExpired, "left_leased");
            assert!(has(report, "after", "shutdown"), "{report}");
        }
        other => panic!("{other:?}"),
    }
    let (st, attempts, _, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts), ("running", 1), "not released");
    open.send(()).expect("open the gate");
    t.drop_now().await;
}

fn hanging(started: Arc<Notify>) -> Script {
    Arc::new(move |_| {
        let started = Arc::clone(&started);
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        })
    })
}

#[tokio::test]
async fn a_hand_back_that_does_not_answer_leaves_the_job_leased() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let started = Arc::new(Notify::new());
    let r = runner(&t, "test.a", hanging(Arc::clone(&started)))
        .with_release_timeout(Duration::from_millis(200));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let mut holder = t.superuser().await;
    let run = r.run_once_until(
        async move {
            let _ = stop_rx.await;
        },
        Duration::from_millis(10),
    );
    // Once the job runs, another session locks its row, then the stop comes.
    let lock_then_stop = async {
        started.notified().await;
        let mut tx = holder.begin().await.expect("begin");
        sqlx::query("SELECT id FROM forge.jobs WHERE id = $1 FOR UPDATE")
            .bind(id.get())
            .execute(&mut *tx)
            .await
            .expect("lock");
        stop_tx.send(()).expect("stop");
        tx
    };
    let (ran, tx) = tokio::join!(run, lock_then_stop);
    match ran.expect("run") {
        Some(Ran::LeftLeased { report, .. }) => {
            assert_reason(&report, ErrorCode::JobLeaseExpired, "release_timeout");
            assert!(has(&report, "timeout_ms", "200"), "{report}");
        }
        other => panic!("{other:?}"),
    }
    tx.rollback().await.expect("rollback");
    // The outcome is unknown: the abandoned UPDATE may still apply once the
    // lock is gone (safe: fenced, and the handler has ended); otherwise the
    // lease expires and the reaper takes it. Never dead, never another
    // worker's.
    let (st, _, _, _) = state(&t, id).await;
    assert!(st == "running" || st == "queued", "{st}");
    t.drop_now().await;
}

/// Signals its drop, then holds the drop (and so the abort) until `go`.
struct HeldDrop {
    dropping: Option<oneshot::Sender<()>>,
    go: mpsc::Receiver<()>,
}

impl Drop for HeldDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.dropping.take() {
            let _ = tx.send(());
        }
        let _ = self.go.recv();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_hand_back_after_a_newer_claim_is_fenced() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let started = Arc::new(Notify::new());
    let (dropping_tx, dropping) = oneshot::channel();
    let (go, go_rx) = mpsc::channel();
    let held = Arc::new(Mutex::new(Some(HeldDrop {
        dropping: Some(dropping_tx),
        go: go_rx,
    })));
    let s = Arc::clone(&started);
    let r = Arc::new(
        runner(
            &t,
            "test.a",
            Arc::new(move |_| {
                let s = Arc::clone(&s);
                let guard = held.lock().ok().and_then(|mut h| h.take());
                Box::pin(async move {
                    let _guard = guard;
                    s.notify_one();
                    std::future::pending::<()>().await;
                    Ok(())
                })
            }),
        )
        .with_abort_wait(Duration::from_secs(20)),
    );
    let run = {
        let (r, started) = (Arc::clone(&r), Arc::clone(&started));
        tokio::spawn(async move {
            r.run_once_until(started.notified(), Duration::from_millis(10))
                .await
        })
    };
    // The abort is held: meanwhile the lease is lost and the job re-claimed.
    tokio::time::timeout(Duration::from_secs(10), dropping)
        .await
        .expect("the handler is being dropped")
        .expect("signal");
    expire(&t, id).await;
    assert_eq!(reap(&t.db).await.expect("reap").len(), 1);
    ready_now(&t, id).await;
    let newer = claim(&t.db, &WorkerId::generate(), &[kind("test.a")])
        .await
        .expect("claim")
        .expect("ready");
    go.send(()).expect("let the drop finish");
    match run.await.expect("no panic").expect("run") {
        Some(Ran::Fenced(e)) => {
            assert_reason(&e, ErrorCode::JobLeaseExpired, "fenced");
            assert!(has(&e, "operation", "jobs.release"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    // The newer claim is untouched.
    let (st, attempts, worker, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts), ("running", 2));
    assert_eq!(worker.as_deref(), Some(newer.lease.worker_id.as_str()));
    t.drop_now().await;
}

#[tokio::test]
async fn a_result_that_cannot_be_recorded_is_logged_once_with_the_job() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let (started, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (s, g) = (Arc::clone(&started), Arc::clone(&go));
    let r = runner(
        &t,
        "test.a",
        Arc::new(move |_| {
            let (s, g) = (Arc::clone(&s), Arc::clone(&g));
            Box::pin(async move {
                s.notify_one();
                g.notified().await;
                Ok(())
            })
        }),
    );
    let logs = Captured::default();
    let _guard = logs.install();
    // While the job runs, the app role loses UPDATE: complete() fails.
    let revoke = async {
        started.notified().await;
        t.exec_superuser("REVOKE UPDATE ON forge.jobs FROM forge_app")
            .await;
        go.notify_one();
    };
    let (ran, ()) = tokio::join!(r.run_once(), revoke);
    match ran.expect("the run reports, not fails") {
        Some(Ran::Unrecorded { job_id, error }) => {
            assert_eq!(job_id, id);
            assert_reason(&error, ErrorCode::DbQueryFailed, "permission_denied");
            for (k, v) in [
                ("job_id", id.get().to_string()),
                ("kind", "test.a".to_owned()),
                ("attempt", "1".to_owned()),
                ("origin_request_id", "req-job-origin".to_owned()),
                ("query", "jobs.complete".to_owned()),
            ] {
                assert!(has(&error, k, &v), "{k}={v}: {error}");
            }
        }
        other => panic!("{other:?}"),
    }
    let logged = logs.failures();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_eq!(logged[0]["fields"]["error_code"], "FG-DB-0002");
    assert_eq!(logged[0]["fields"]["request_id"], "req-job-origin");
    t.exec_superuser("GRANT UPDATE ON forge.jobs TO forge_app")
        .await;
    assert_eq!(state(&t, id).await.0, "running", "left to the reaper");
    t.drop_now().await;
}
