//! The worker loop against a migrated database: it numbers events while it
//! runs and stops on shutdown.

use std::time::{Duration, Instant};

use forge_db::sequencer::backlog;
use forge_db::test_support::FreshDb;
use tokio::sync::watch;

use super::run;

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

/// Wait (bounded) until nothing waits for a number.
async fn until_sequenced(t: &FreshDb) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let b = backlog(&t.db).await.expect("backlog").expect("migrated");
        if b.unsequenced_events == 0 && b.heartbeat_age_ms.is_some() {
            b.check_lag().expect("no lag while the worker runs");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the worker never caught up: {b:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_worker_numbers_events_until_it_is_told_to_stop() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 5).await;
    let (stop, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = tokio::spawn(async move { run(&db, stopped).await });
    until_sequenced(&t).await;
    // Events written while it runs are numbered too.
    insert_events(&t, 3).await;
    until_sequenced(&t).await;
    stop.send(true).expect("worker listens");
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("stops in time")
        .expect("no panic")
        .expect("clean stop");
    t.drop_now().await;
}

#[tokio::test]
async fn the_worker_stops_when_the_shutdown_sender_is_gone() {
    let t = FreshDb::migrated().await;
    let (stop, stopped) = watch::channel(false);
    drop(stop);
    tokio::time::timeout(Duration::from_secs(10), run(&t.db, stopped))
        .await
        .expect("stops in time")
        .expect("clean stop");
    t.drop_now().await;
}
