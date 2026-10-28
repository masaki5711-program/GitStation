//! JobRunner against a migrated database: no handlers claims nothing,
//! success, execution timeout, a panicking handler, max_attempts → dead +
//! FG-DB-0006, a dropped run reaped later (FG-DB-0007 / FG-DB-0006).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::Notify;

use super::tests::{expire, kind, new_job, queued, ready_now, state};
use super::*;
use crate::test_support::*;

pub(crate) type Script = Arc<dyn Fn(ClaimedJob) -> JobFuture + Send + Sync>;

pub(crate) struct Scripted {
    pub(crate) kind: JobKind,
    pub(crate) run: Script,
}

impl JobHandler for Scripted {
    fn kind(&self) -> JobKind {
        self.kind.clone()
    }
    fn run(&self, job: ClaimedJob) -> JobFuture {
        (self.run)(job)
    }
}

pub(crate) fn runner(t: &FreshDb, k: &str, run: Script) -> JobRunner {
    let mut r = JobRunner::new(t.db.clone(), WorkerId::generate());
    r.register(Arc::new(Scripted { kind: kind(k), run }))
        .expect("register");
    r
}

pub(crate) fn has(err: &ForgeError, key: &str, value: &str) -> bool {
    err.context().iter().any(|(k, v)| *k == key && v == value)
}

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_runner_without_handlers_claims_nothing() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let r = JobRunner::new(t.db.clone(), WorkerId::generate());
    assert!(r.kinds().is_empty());
    assert!(r.run_once().await.expect("run").is_none());
    assert!(r.reap_and_report().await.expect("reap").is_empty());
    assert_eq!(state(&t, id).await.0, "queued");
    t.drop_now().await;
}

#[tokio::test]
async fn a_second_handler_for_a_kind_is_refused() {
    let t = FreshDb::migrated().await;
    let ok: Script = Arc::new(|_| Box::pin(async { Ok(()) }));
    let mut r = runner(&t, "test.a", Arc::clone(&ok));
    let err = r
        .register(Arc::new(Scripted {
            kind: kind("test.a"),
            run: ok,
        }))
        .expect_err("duplicate");
    assert_reason(&err, ErrorCode::Internal, "duplicate_handler");
    t.drop_now().await;
}

#[tokio::test]
async fn a_successful_job_is_completed() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let r = runner(
        &t,
        "test.a",
        Arc::new(|job| {
            Box::pin(async move {
                assert_eq!(job.payload["ref"], 7);
                Ok(())
            })
        }),
    );
    assert!(matches!(r.run_once().await.expect("run"), Some(Ran::Succeeded(j)) if j == id));
    assert_eq!(state(&t, id).await.0, "succeeded");
    assert!(r.run_once().await.expect("run").is_none());
    t.drop_now().await;
}

#[tokio::test]
async fn an_execution_timeout_aborts_the_run_and_retries() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, Duration::from_millis(100))).await;
    let r = runner(
        &t,
        "test.a",
        Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            })
        }),
    );
    let started = Instant::now();
    match r.run_once().await.expect("run") {
        Some(Ran::Retrying { failure, .. }) => {
            assert_reason(&failure, ErrorCode::JobLeaseExpired, "execution_timeout");
            assert!(has(&failure, "timeout_ms", "100"), "{failure}");
        }
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    let (st, attempts, _, code) = state(&t, id).await;
    assert_eq!(
        (st.as_str(), attempts, code.as_deref()),
        ("queued", 1, Some("FG-DB-0007"))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_panicking_handler_is_an_internal_failure_and_retried() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    #[allow(clippy::panic)] // the point of the test
    let r = runner(
        &t,
        "test.a",
        Arc::new(|_| Box::pin(async { panic!("handler bug") })),
    );
    match r.run_once().await.expect("run") {
        Some(Ran::Retrying { failure, .. }) => {
            assert_reason(&failure, ErrorCode::Internal, "job_panicked");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(state(&t, id).await.0, "queued");
    t.drop_now().await;
}

#[tokio::test]
async fn a_job_that_fails_max_attempts_times_is_dead_and_reported() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 2, T)).await;
    let r = runner(
        &t,
        "test.a",
        Arc::new(|_| {
            Box::pin(async {
                Err(JobFailure::retry(
                    ForgeError::new(ErrorCode::DbQueryFailed)
                        .with_context("reason", "lock_timeout"),
                ))
            })
        }),
    );
    assert!(matches!(
        r.run_once().await.expect("run"),
        Some(Ran::Retrying { .. })
    ));
    ready_now(&t, id).await;
    match r.run_once().await.expect("run") {
        Some(Ran::Dead { job_id, report }) => {
            assert_eq!(job_id, id);
            assert_eq!(report.code(), ErrorCode::JobDeadLettered, "{report}");
            for (k, v) in [
                ("reason", "attempts_exhausted"),
                ("kind", "test.a"),
                ("attempts", "2"),
                ("last_error_code", "FG-DB-0002"),
                ("origin_request_id", "req-job-origin"),
            ] {
                assert!(has(&report, k, v), "{k}={v}: {report}");
            }
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(state(&t, id).await.0, "dead");
    assert!(r.run_once().await.expect("run").is_none());
    t.drop_now().await;
}

/// Sets the flag when dropped (the handler's future was dropped).
pub(crate) struct DropFlag(pub(crate) Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn a_dropped_run_leaves_its_job_to_the_reaper() {
    let t = FreshDb::migrated().await;
    let again = queued(&t, &new_job("test.a", None, 3, T)).await;
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let (s, d) = (Arc::clone(&started), Arc::clone(&dropped));
    let r = Arc::new(runner(
        &t,
        "test.a",
        Arc::new(move |_| {
            let (s, flag) = (Arc::clone(&s), DropFlag(Arc::clone(&d)));
            Box::pin(async move {
                let _flag = flag;
                s.notify_one();
                std::future::pending::<()>().await;
                Ok(())
            })
        }),
    ));
    let running = {
        let r = Arc::clone(&r);
        tokio::spawn(async move { r.run_once().await })
    };
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the handler started");
    running.abort();
    assert!(running.await.expect_err("aborted").is_cancelled());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the handler's task was not aborted"
        );
        tokio::task::yield_now().await;
    }
    // The job is still leased to the dropped run: nothing else takes it.
    assert_eq!(state(&t, again).await.0, "running");
    assert!(r.run_once().await.expect("run").is_none());
    // Its lease expires; the reaper queues it again.
    expire(&t, again).await;
    match r.reap_and_report().await.expect("reap").as_slice() {
        [Reported::Requeued(e)] => {
            assert_reason(e, ErrorCode::JobLeaseExpired, "lease_expired");
            assert!(has(e, "worker_id", r.worker_id().as_str()), "{e}");
            assert!(has(e, "attempt", "1"), "{e}");
            assert!(has(e, "job_id", &again.get().to_string()), "{e}");
            assert!(has(e, "origin_request_id", "req-job-origin"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    let (st, attempts, _, code) = state(&t, again).await;
    assert_eq!(
        (st.as_str(), attempts, code.as_deref()),
        ("queued", 1, Some("FG-DB-0007"))
    );
    // With its attempts used up, an expired lease is a dead job.
    let last = queued(&t, &new_job("test.b", None, 1, T)).await;
    claim(&t.db, &WorkerId::generate(), &[kind("test.b")])
        .await
        .expect("claim")
        .expect("ready");
    expire(&t, last).await;
    match r.reap_and_report().await.expect("reap").as_slice() {
        [Reported::Dead(e)] => {
            assert_reason(e, ErrorCode::JobDeadLettered, "lease_expired");
            assert!(has(e, "last_error_code", "FG-DB-0007"), "{e}");
            assert!(has(e, "job_id", &last.get().to_string()), "{e}");
            assert!(has(e, "origin_request_id", "req-job-origin"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_permanent_failure_is_dead_at_the_first_attempt() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 5, T)).await;
    let r = runner(
        &t,
        "test.a",
        Arc::new(|_| {
            Box::pin(async {
                Err(JobFailure::permanent(
                    ForgeError::new(ErrorCode::Internal).with_context("reason", "target_gone"),
                ))
            })
        }),
    );
    match r.run_once().await.expect("run") {
        Some(Ran::Dead { job_id, report }) => {
            assert_eq!(job_id, id);
            assert_reason(&report, ErrorCode::JobDeadLettered, "permanent_failure");
            assert!(has(&report, "attempts", "1"), "{report}");
            assert!(has(&report, "last_error_code", "FG-SYS-0001"), "{report}");
        }
        other => panic!("{other:?}"),
    }
    let (st, attempts, _, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts), ("dead", 1));
    t.drop_now().await;
}
