//! The task loop's failure path with scripted runs instead of a database:
//! what is logged (captured JSON tracing output), when it is logged again,
//! recovery, shutdown in the middle of a backoff, a panicking task, and a
//! backlog drained in full batches without a period between them.
//! Time is paused (tokio test-util): no real sleeps.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::after_sequencing;
use super::supervise::{supervise, Next, RunFuture, Runner};

const PERIOD: Duration = Duration::from_millis(200);
const MAX: Duration = Duration::from_secs(5);
const TASK: &str = "scripted";

/// The JSON log lines written on this thread while installed (the tests run
/// on a current-thread runtime, so the spawned tasks log here too).
#[derive(Clone, Default)]
pub(super) struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    pub(super) fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().expect("log buffer").clone();
        String::from_utf8(bytes)
            .expect("utf-8 logs")
            .lines()
            .map(|l| serde_json::from_str(l).expect("a JSON log line"))
            .collect()
    }

    /// The failure events (telemetry::log_failure: they carry error_code).
    pub(super) fn failures(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["fields"]["error_code"].is_string())
            .collect()
    }

    fn with_message(&self, message: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e["fields"]["message"] == message)
            .collect()
    }
}

/// `key` of the space-separated `context` field of a failure event.
pub(super) fn context(event: &Value, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    event["fields"]["context"]
        .as_str()?
        .split(' ')
        .find_map(|kv| kv.strip_prefix(prefix.as_str()).map(str::to_owned))
}

pub(super) fn request_id(event: &Value) -> String {
    event["fields"]["request_id"]
        .as_str()
        .expect("request_id")
        .to_owned()
}

fn unavailable() -> ForgeError {
    ForgeError::new(ErrorCode::DbUnavailable).with_context("reason", "connect_refused")
}

/// A runner whose n-th run (from 1) returns `script(n)`.
fn scripted(mut script: impl FnMut(u32) -> Result<(), ForgeError> + Send + 'static) -> Runner {
    let mut n = 0;
    Runner {
        name: TASK,
        period: PERIOD,
        run: Box::new(move || -> RunFuture {
            n += 1;
            let result = script(n).map(|()| Next::AfterPeriod);
            Box::pin(async move { result })
        }),
    }
}

/// Run `runners` for `for_` of (paused) time, then request shutdown.
async fn run_for(runners: Vec<Runner>, for_: Duration) -> Result<(), ForgeError> {
    let (stop, stopped) = watch::channel(false);
    let worker = tokio::spawn(supervise(runners, MAX, stopped));
    tokio::time::sleep(for_).await;
    stop.send(true).expect("the worker listens");
    worker.await.expect("no panic")
}

#[tokio::test(start_paused = true)]
async fn a_failure_is_logged_once_with_its_code_task_and_retry_delay() {
    let logs = Captured::default();
    let _guard = logs.install();
    run_for(
        vec![scripted(|_| Err(unavailable()))],
        Duration::from_secs(30),
    )
    .await
    .expect("clean stop");
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "{failures:#?}");
    let f = &failures[0];
    assert_eq!(f["fields"]["error_code"], "FG-DB-0001");
    assert_eq!(context(f, "task").as_deref(), Some(TASK));
    assert_eq!(context(f, "reason").as_deref(), Some("connect_refused"));
    assert_eq!(context(f, "failures").as_deref(), Some("1"));
    assert_eq!(context(f, "retry_in_ms").as_deref(), Some("400"));
    assert!(!request_id(f).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_persisting_failure_is_logged_again_every_minute_in_the_same_streak() {
    let logs = Captured::default();
    let _guard = logs.install();
    // Runs at 0, 0.4, 1.2, 2.8, 6, then every 5 s: re-logs at 61 s and 121 s.
    run_for(
        vec![scripted(|_| Err(unavailable()))],
        Duration::from_secs(125),
    )
    .await
    .expect("clean stop");
    let failures = logs.failures();
    assert_eq!(failures.len(), 3, "{failures:#?}");
    let ids: Vec<String> = failures.iter().map(request_id).collect();
    assert!(ids.iter().all(|id| *id == ids[0]), "one streak id: {ids:?}");
    let counts: Vec<u32> = failures
        .iter()
        .map(|f| {
            context(f, "failures")
                .and_then(|n| n.parse().ok())
                .expect("failures=N")
        })
        .collect();
    assert_eq!(counts[0], 1, "{counts:?}");
    assert!(counts.windows(2).all(|w| w[0] < w[1]), "{counts:?}");
    assert!(failures
        .iter()
        .all(|f| context(f, "retry_in_ms").as_deref() == Some("5000")
            || context(f, "failures").as_deref() == Some("1")));
}

#[tokio::test(start_paused = true)]
async fn a_changed_failure_is_logged_at_once() {
    let logs = Captured::default();
    let _guard = logs.install();
    let script = |n: u32| {
        Err(if n <= 2 {
            unavailable()
        } else {
            ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("query", "sequencer.assign_seq")
                .with_context("reason", "lock_timeout")
        })
    };
    run_for(vec![scripted(script)], Duration::from_secs(10))
        .await
        .expect("clean stop");
    let codes: Vec<Value> = logs
        .failures()
        .iter()
        .map(|f| f["fields"]["error_code"].clone())
        .collect();
    assert_eq!(codes, ["FG-DB-0001", "FG-DB-0002"]);
}

#[tokio::test(start_paused = true)]
async fn recovery_is_logged_with_the_streak_request_id() {
    let logs = Captured::default();
    let _guard = logs.install();
    let script = |n: u32| if n <= 2 { Err(unavailable()) } else { Ok(()) };
    run_for(vec![scripted(script)], Duration::from_secs(5))
        .await
        .expect("clean stop");
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "{failures:#?}");
    let recovered = logs.with_message("forge.worker.task_recovered");
    assert_eq!(recovered.len(), 1, "{recovered:#?}");
    let r = &recovered[0]["fields"];
    assert_eq!(r["failures"], 2);
    assert_eq!(r["task"], TASK);
    assert_eq!(r["request_id"], request_id(&failures[0]));
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_a_backoff_sleep_returns_at_once() {
    let (stop, stopped) = watch::channel(false);
    let worker = tokio::spawn(supervise(
        vec![scripted(|_| Err(unavailable()))],
        MAX,
        stopped,
    ));
    // At 30 s the last run was at 26 s: the task sleeps until 31 s.
    tokio::time::sleep(Duration::from_secs(30)).await;
    let asked = Instant::now();
    stop.send(true).expect("the worker listens");
    worker.await.expect("no panic").expect("clean stop");
    assert_eq!(
        Instant::now().duration_since(asked),
        Duration::ZERO,
        "the backoff sleep was not waited out"
    );
}

/// Sets its flag when dropped (the future holding it was aborted).
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicking_task_is_internal_and_aborts_the_other_tasks() {
    let started = Arc::new(Notify::new());
    let aborted = Arc::new(AtomicBool::new(false));
    let panics = {
        let started = Arc::clone(&started);
        Runner {
            name: "panics",
            period: PERIOD,
            run: Box::new(move || -> RunFuture {
                let started = Arc::clone(&started);
                Box::pin(async move {
                    started.notified().await;
                    panic!("scripted panic");
                })
            }),
        }
    };
    let waits = {
        let aborted = Arc::clone(&aborted);
        Runner {
            name: "waits",
            period: PERIOD,
            run: Box::new(move || -> RunFuture {
                let flag = DropFlag(Arc::clone(&aborted));
                let started = Arc::clone(&started);
                Box::pin(async move {
                    let _flag = flag;
                    started.notify_one();
                    std::future::pending::<()>().await;
                    Ok(Next::AfterPeriod)
                })
            }),
        }
    };
    let (_stop, stopped) = watch::channel(false);
    let err = tokio::time::timeout(
        Duration::from_secs(60),
        supervise(vec![panics, waits], MAX, stopped),
    )
    .await
    .expect("the supervisor returns")
    .expect_err("a panic is an error");
    assert_eq!(err.code(), ErrorCode::Internal);
    let has = |key: &str, value: &str| err.context().iter().any(|(k, v)| *k == key && v == value);
    assert!(has("task", "panics"), "{err}");
    assert!(has("reason", "task_panicked"), "{err}");
    assert!(aborted.load(Ordering::SeqCst), "the other task was aborted");
}

#[tokio::test(start_paused = true)]
async fn a_backlog_larger_than_a_batch_drains_without_waiting_a_period_between_full_batches() {
    use forge_db::sequencer::{Assigned, BatchSize, SeqRange};

    let batch = BatchSize::MAX;
    let start = Instant::now();
    let runs = Arc::new(Mutex::new(Vec::new()));
    let sequencer = {
        let runs = Arc::clone(&runs);
        // 2500 waiting rows: two full batches, then 500, then idle.
        let mut high: i64 = 0;
        let backlog: i64 = 2500;
        Runner {
            name: "sequencer",
            period: PERIOD,
            run: Box::new(move || -> RunFuture {
                let take = (backlog - high).min(i64::from(batch.get()));
                let assigned = Assigned {
                    events: SeqRange::new(high + 1, high + take),
                    audit: None,
                };
                high += take;
                runs.lock()
                    .expect("runs")
                    .push((Instant::now().duration_since(start), take));
                Box::pin(async move { Ok(after_sequencing(assigned, batch)) })
            }),
        }
    };
    run_for(vec![sequencer], Duration::from_millis(500))
        .await
        .expect("clean stop");
    let runs = runs.lock().expect("runs").clone();
    assert_eq!(
        runs.get(..4),
        Some(
            &[
                (Duration::ZERO, 1000),
                (Duration::ZERO, 1000),
                (Duration::ZERO, 500),
                // Not a full batch: back to the period.
                (PERIOD, 0),
            ][..]
        ),
        "{runs:?}"
    );
}
