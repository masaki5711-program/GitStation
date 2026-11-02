//! The job tasks of the worker against a migrated database: the reaper runs
//! at startup, the runner leaves kinds without a handler alone, and a job in
//! progress at shutdown is abandoned after the grace — still leased, so the
//! next reaper picks it up (never lost).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::RequestId;
use forge_db::jobs::{
    enqueue, health, ClaimedJob, JobFuture, JobHandler, JobKind, JobRunner, JobTimeout, JobsHealth,
    MaxAttempts, NewJob, WorkerId,
};
use forge_db::subscribers::Registry;
use forge_db::test_support::FreshDb;
use tokio::sync::{watch, Notify};

use super::seal_tests::seal_keys;
use super::{run, run_with};

fn job(kind: &str) -> NewJob {
    NewJob {
        kind: JobKind::parse(kind).expect("kind"),
        payload: serde_json::json!({}),
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(Duration::from_secs(600)).expect("timeout"),
        dedup_key: None,
        origin_request_id: RequestId::parse("req-worker-job").expect("id"),
        origin_event_seq: None,
    }
}

async fn health_of(t: &FreshDb) -> JobsHealth {
    health(&t.db).await.expect("health").expect("migrated")
}

/// Wait (bounded) until `done` holds.
async fn until(t: &FreshDb, what: &str, done: impl Fn(&JobsHealth) -> bool) -> JobsHealth {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let h = health_of(t).await;
        if done(&h) {
            return h;
        }
        assert!(Instant::now() < deadline, "{what}: {h:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stopped_cleanly(
    stop: watch::Sender<bool>,
    worker: tokio::task::JoinHandle<Result<(), forge_errors::ForgeError>>,
) {
    stop.send(true).expect("worker listens");
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("stops in time")
        .expect("no panic")
        .expect("clean stop");
}

#[tokio::test]
async fn the_worker_reaps_at_startup_and_stops_cleanly() {
    let t = FreshDb::migrated().await;
    // A lease left behind by a crashed worker five minutes ago.
    t.exec_superuser(
        "INSERT INTO forge.jobs (kind, payload, state, attempts, max_attempts, timeout_ms, \
         origin_request_id, worker_id, lease_until) VALUES ('test.crashed', '{}', 'running', \
         1, 3, 1000, 'r-1', 'w1-gone', now() - interval '5 minutes')",
    )
    .await;
    assert_eq!(health_of(&t).await.expired_leases, 1);
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = {
        let (dir, storage) = super::provision::tests::storage();
        tokio::spawn(async move {
            let _root = dir;
            run(&db, seal_keys(), &storage, None, stopped).await
        })
    };
    until(&t, "the reaper never ran", |h| h.oldest_expired.is_none()).await;
    stopped_cleanly(stop, worker).await;
    t.drop_now().await;
}

/// Signals each run of its kind.
struct Signals {
    kind: &'static str,
    ran: Arc<Notify>,
}

impl JobHandler for Signals {
    fn kind(&self) -> JobKind {
        JobKind::parse(self.kind).expect("kind")
    }
    fn run(&self, _job: ClaimedJob) -> JobFuture {
        let ran = Arc::clone(&self.ran);
        Box::pin(async move {
            ran.notify_one();
            Ok(())
        })
    }
}

#[tokio::test]
async fn the_runner_works_its_kinds_and_leaves_unhandled_ones_queued() {
    let t = FreshDb::migrated().await;
    // Older than the handled job: claimed first if kinds were ignored.
    enqueue(&t.db, &job("test.unhandled"))
        .await
        .expect("enqueue");
    enqueue(&t.db, &job("test.handled")).await.expect("enqueue");
    let ran = Arc::new(Notify::new());
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate());
    runner
        .register(Arc::new(Signals {
            kind: "test.handled",
            ran: Arc::clone(&ran),
        }))
        .expect("register");
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = tokio::spawn(async move {
        run_with(
            &db,
            runner,
            Registry::new(),
            Duration::from_secs(5),
            seal_keys(),
            stopped,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(20), ran.notified())
        .await
        .expect("the handled job ran");
    let h = until(&t, "the handled job was not completed", |h| {
        h.depth.iter().all(|k| k.kind != "test.handled")
    })
    .await;
    assert_eq!(h.depth.len(), 1, "{h:?}");
    assert_eq!(
        (h.depth[0].kind.as_str(), h.depth[0].ready),
        ("test.unhandled", 1)
    );
    stopped_cleanly(stop, worker).await;
    t.drop_now().await;
}

/// Never finishes; signals its start and its drop.
struct Hanging {
    started: Arc<Notify>,
    dropped: Arc<AtomicBool>,
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl JobHandler for Hanging {
    fn kind(&self) -> JobKind {
        JobKind::parse("test.hanging").expect("kind")
    }
    fn run(&self, _job: ClaimedJob) -> JobFuture {
        let started = Arc::clone(&self.started);
        let flag = DropFlag(Arc::clone(&self.dropped));
        Box::pin(async move {
            let _flag = flag;
            started.notify_one();
            std::future::pending::<()>().await;
            Ok(())
        })
    }
}

#[tokio::test]
async fn a_job_in_progress_at_shutdown_is_released_after_the_grace() {
    let t = FreshDb::migrated().await;
    // At its last attempt: a restart must not dead-letter it.
    let mut last = job("test.hanging");
    last.max_attempts = MaxAttempts::new(1).expect("max");
    enqueue(&t.db, &last).await.expect("enqueue");
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(AtomicBool::new(false));
    let mut runner = JobRunner::new(t.db.clone(), WorkerId::generate());
    runner
        .register(Arc::new(Hanging {
            started: Arc::clone(&started),
            dropped: Arc::clone(&dropped),
        }))
        .expect("register");
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = tokio::spawn(async move {
        run_with(
            &db,
            runner,
            Registry::new(),
            Duration::from_millis(200),
            seal_keys(),
            stopped,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the job started");
    let asked = Instant::now();
    stopped_cleanly(stop, worker).await;
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "{:?}",
        asked.elapsed()
    );
    assert!(dropped.load(Ordering::SeqCst), "the handler was aborted");
    // Handed back: ready now, not dead, no lease left behind.
    let h = health_of(&t).await;
    assert_eq!((h.dead_total, h.expired_leases), (0, 0), "{h:?}");
    assert_eq!(h.depth.len(), 1, "{h:?}");
    assert_eq!(
        (h.depth[0].kind.as_str(), h.depth[0].ready),
        ("test.hanging", 1)
    );
    t.exec_superuser("UPDATE forge.jobs SET lease_until = now() - interval '5 minutes'")
        .await;
    assert_eq!(
        health_of(&t).await.expired_leases,
        0,
        "not running any more"
    );
    t.drop_now().await;
}
