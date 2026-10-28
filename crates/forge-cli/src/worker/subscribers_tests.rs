//! The worker with a registered subscriber, against a migrated database:
//! the subscriber is registered at the head, gets every event the
//! sequencer numbers afterwards, and the worker still stops on shutdown.

use std::sync::atomic::{AtomicI64, Ordering as Atomic};
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::subscribers::{
    states, DerivedTx, HandleFuture, Ordering, Registry, SourceEvent, Subscriber,
};
use forge_db::test_support::FreshDb;
use tokio::sync::watch;

use super::super::run_with;
use super::super::seal_tests::seal_keys;

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
