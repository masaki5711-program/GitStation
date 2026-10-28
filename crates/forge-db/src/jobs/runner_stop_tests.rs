//! JobRunner when a run is cut short: an aborted handler is drained before
//! the job is settled (and logged when it does not end in time), a stop
//! releases the job without counting the attempt, a job that ends within
//! the grace completes, and a dead job logs its cause and then FG-DB-0006
//! with the origin request id.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;
use tokio::sync::Notify;

use super::runner_tests::{has, runner, DropFlag};
use super::tests::{kind, new_job, queued, state};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(5);

/// JSON log lines written on this thread while installed.
///
/// One global subscriber writes to the capture installed on the current
/// thread. Thread-local subscribers (`set_default`) are not used: a callsite
/// first hit by a parallel test while no capture was alive stays cached as
/// "never", and its events are lost (2026-10-28, FG-DB-0002 event missing).
#[derive(Clone, Default)]
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

/// Writes to this thread's capture, or nowhere.
struct ThreadSink;

impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| match s.borrow_mut().as_mut() {
            Some(c) => c.write(buf),
            None => Ok(buf.len()),
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Clears this thread's capture on drop.
pub(crate) struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    pub(crate) fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the only global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        CaptureGuard
    }

    /// The failure events (they carry error_code).
    pub(crate) fn failures(&self) -> Vec<Value> {
        let bytes = self.0.lock().expect("log buffer").clone();
        String::from_utf8(bytes)
            .expect("utf-8 logs")
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).expect("a JSON log line"))
            .filter(|e| e["fields"]["error_code"].is_string())
            .collect()
    }
}

/// Blocks its thread for `block` (ignoring cancellation), then awaits.
fn blocking_handler(
    block: Duration,
    finished: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
) -> super::runner_tests::Script {
    Arc::new(move |_| {
        let (finished, flag) = (Arc::clone(&finished), DropFlag(Arc::clone(&dropped)));
        Box::pin(async move {
            let _flag = flag;
            tokio::task::yield_now().await;
            std::thread::sleep(block);
            finished.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timed_out_handler_is_drained_before_the_job_is_settled() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, Duration::from_millis(100))).await;
    let (finished, dropped) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let r = runner(
        &t,
        "test.a",
        blocking_handler(
            Duration::from_millis(600),
            Arc::clone(&finished),
            Arc::clone(&dropped),
        ),
    );
    match r.run_once().await.expect("run") {
        Some(Ran::Retrying { failure, .. }) => {
            assert_reason(&failure, ErrorCode::JobLeaseExpired, "execution_timeout");
        }
        other => panic!("{other:?}"),
    }
    // The old run had ended before the job became claimable again.
    assert!(
        finished.load(Ordering::SeqCst),
        "settled while the handler still ran"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the handler's future was dropped"
    );
    assert_eq!(state(&t, id).await.0, "queued");
    t.drop_now().await;
}

#[tokio::test]
async fn a_stopped_job_is_released_without_counting_the_attempt() {
    let t = FreshDb::migrated().await;
    // At its last attempt: a shutdown must not make it dead.
    let id = queued(&t, &new_job("test.a", None, 1, T)).await;
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let (s, d) = (Arc::clone(&started), Arc::clone(&dropped));
    let r = runner(
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
    );
    let stop = started.notified();
    match r
        .run_once_until(stop, Duration::from_millis(50))
        .await
        .expect("run")
    {
        Some(Ran::Released { job_id, report }) => {
            assert_eq!(job_id, id);
            assert_reason(&report, ErrorCode::JobLeaseExpired, "released_on_shutdown");
            assert!(has(&report, "job_id", &id.get().to_string()), "{report}");
            assert!(has(&report, "kind", "test.a"), "{report}");
            assert!(
                has(&report, "origin_request_id", "req-job-origin"),
                "{report}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(
        dropped.load(Ordering::SeqCst),
        "the handler ended before the release"
    );
    let (st, attempts, worker, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts, worker), ("queued", 0, None));
    // Ready at once, with its attempt back.
    let again = claim(&t.db, &WorkerId::generate(), &[kind("test.a")])
        .await
        .expect("claim")
        .expect("released jobs are ready");
    assert_eq!(again.lease.attempt, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_job_that_ends_within_the_grace_completes() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let started = Arc::new(Notify::new());
    let s = Arc::clone(&started);
    let r = runner(
        &t,
        "test.a",
        Arc::new(move |_| {
            let s = Arc::clone(&s);
            Box::pin(async move {
                s.notify_one();
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(())
            })
        }),
    );
    let ran = r
        .run_once_until(started.notified(), Duration::from_secs(5))
        .await
        .expect("run");
    assert!(matches!(ran, Some(Ran::Succeeded(j)) if j == id), "{ran:?}");
    assert_eq!(state(&t, id).await.0, "succeeded");
    t.drop_now().await;
}

#[tokio::test]
async fn a_dead_job_logs_its_cause_then_fg_db_0006_with_the_origin_request_id() {
    let t = FreshDb::migrated().await;
    queued(&t, &new_job("test.a", None, 1, T)).await;
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
    let logs = Captured::default();
    let ran = {
        let _guard = logs.install();
        r.run_once().await.expect("run")
    };
    assert!(matches!(ran, Some(Ran::Dead { .. })), "{ran:?}");
    let codes: Vec<String> = logs
        .failures()
        .iter()
        .map(|e| {
            e["fields"]["error_code"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(codes, vec!["FG-DB-0002", "FG-DB-0006"]);
    for e in logs.failures() {
        assert_eq!(e["fields"]["request_id"], "req-job-origin", "{e}");
    }
    t.drop_now().await;
}
