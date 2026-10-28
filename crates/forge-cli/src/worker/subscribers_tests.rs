//! The worker with a registered subscriber, against a migrated database:
//! the subscriber is registered at the head, gets every event the
//! sequencer numbers afterwards, and the worker still stops on shutdown;
//! a stuck subscriber is logged once, with the source event's request id.

use std::sync::atomic::{AtomicI64, Ordering as Atomic};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::subscribers::{
    register, run_once, states, DerivedTx, HandleFuture, Ordering, Progress, Registry, SourceEvent,
    Start, Subscriber,
};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use tokio::sync::watch;

use super::super::loop_tests::{context, request_id, Captured};
use super::super::run_with;
use super::super::seal_tests::seal_keys;
use super::NAME;

/// Counts the events it handles.
struct Counting(AtomicI64);

impl Subscriber for Counting {
    fn name(&self) -> &'static str {
        "worker-test"
    }

    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }

    fn handle<'a>(
        &'a self,
        _event: &'a SourceEvent,
        _tx: &'a mut DerivedTx<'_>,
    ) -> HandleFuture<'a> {
        Box::pin(async move {
            self.0.fetch_add(1, Atomic::SeqCst);
            Ok(())
        })
    }
}

async fn insert_events(t: &FreshDb, n: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-1'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), 'repository.created', 1, 'repository', g, 'system', 'r-1', '{{}}' \
         FROM generate_series(1, {n}) AS g"
    ))
    .await;
}

#[tokio::test]
async fn the_worker_feeds_registered_subscribers_the_events_it_numbers() {
    let t = FreshDb::migrated().await;
    let counting = Arc::new(Counting(AtomicI64::new(0)));
    let registry = Registry::new().with(counting.clone()).expect("registry");
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let runner = JobRunner::new(t.db.clone(), WorkerId::generate()); // clone: pool handles
    let worker = tokio::spawn(async move {
        run_with(
            &db,
            runner,
            registry,
            Duration::from_secs(5),
            seal_keys(),
            stopped,
        )
        .await
    });
    // Registered at the head: only events written from now on reach it.
    let deadline = Instant::now() + Duration::from_secs(30);
    while states(&t.db)
        .await
        .expect("states")
        .is_none_or(|s| s.is_empty())
    {
        assert!(Instant::now() < deadline, "never registered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    insert_events(&t, 4).await;
    while counting.0.load(Atomic::SeqCst) < 4 {
        assert!(Instant::now() < deadline, "the subscriber never caught up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.send(true).expect("worker listens");
    // A run may be waiting for a notification: at most one poll (5 s).
    tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("stops in time")
        .expect("no panic")
        .expect("clean stop");
    let s = states(&t.db).await.expect("states").expect("migrated");
    assert_eq!((s[0].name.as_str(), s[0].lag_events), ("worker-test", 0));
    assert_eq!(counting.0.load(Atomic::SeqCst), 4);
    t.drop_now().await;
}

/// Always fails.
struct Failing;

impl Subscriber for Failing {
    fn name(&self) -> &'static str {
        "worker-stuck"
    }

    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }

    fn handle<'a>(
        &'a self,
        _event: &'a SourceEvent,
        _tx: &'a mut DerivedTx<'_>,
    ) -> HandleFuture<'a> {
        Box::pin(async {
            Err(ForgeError::new(ErrorCode::GitUnavailable).with_context("reason", "test_failure"))
        })
    }
}

#[tokio::test]
async fn a_stuck_subscriber_is_logged_once_with_the_source_request_id() {
    let t = FreshDb::migrated().await;
    let failing = Arc::new(Failing);
    register(&t.db, failing.as_ref(), Start::Beginning)
        .await
        .expect("register");
    insert_events(&t, 1).await;
    forge_db::sequencer::run_once(
        &t.db,
        forge_db::sequencer::BatchSize::MAX,
        forge_db::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
    // Four failures already counted: the task's round makes the fifth.
    for _ in 0..4 {
        let progress = run_once(&t.db, failing.as_ref()).await.expect("step");
        assert!(matches!(progress, Progress::Failed(_)), "{progress:?}");
    }
    let registry = Registry::new().with(failing).expect("registry");
    let state = tokio::sync::Mutex::new(None);
    let logs = Captured::default();
    let guard = logs.install();
    // The round logs the stuck subscriber; the first wait only connects.
    tokio::time::timeout(
        Duration::from_secs(60),
        super::run(&t.db, &registry, &state),
    )
    .await
    .expect("one run is bounded")
    .expect("the run itself succeeds");
    drop(guard);
    let stuck: Vec<_> = logs
        .failures()
        .into_iter()
        .filter(|e| context(e, "reason").as_deref() == Some("stuck"))
        .collect();
    assert_eq!(stuck.len(), 1, "{stuck:?}");
    let e = &stuck[0];
    assert_eq!(e["fields"]["error_code"], "FG-DB-0008");
    assert_eq!(request_id(e), "r-1");
    assert_eq!(context(e, "task").as_deref(), Some(NAME));
    assert_eq!(context(e, "source_seq").as_deref(), Some("1"));
    assert_eq!(context(e, "subscriber").as_deref(), Some("worker-stuck"));
    assert!(context(e, "source_event_id").is_some(), "{e}");
    t.drop_now().await;
}
