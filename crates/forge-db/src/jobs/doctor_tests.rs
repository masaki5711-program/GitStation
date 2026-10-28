//! What `forge doctor` reports about the queue: recent dead jobs
//! (FG-DB-0006), expired leases nobody reaped (FG-DB-0007), queue depth and
//! wait per kind (FG-DB-0006).

use std::time::Duration;

use forge_errors::ErrorCode;

use super::tests::{expire, new_job, queued};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(5);

async fn health_of(t: &FreshDb) -> JobsHealth {
    health(&t.db).await.expect("health").expect("migrated")
}

/// Ready rows of `kind` inserted directly (`n`, waiting `wait_s` seconds).
async fn ready_rows(t: &FreshDb, kind: &str, n: i64, wait_s: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
         origin_request_id, run_after) SELECT '{kind}', '{{}}', 'queued', 3, 1000, 'r-1', \
         now() - interval '{wait_s} seconds' FROM generate_series(1, {n})"
    ))
    .await;
}

#[tokio::test]
async fn an_empty_queue_is_healthy() {
    let t = FreshDb::migrated().await;
    let h = health_of(&t).await;
    assert_eq!((h.dead_recent, h.dead_total, h.expired_leases), (0, 0, 0));
    assert!(h.depth.is_empty());
    h.check_dead().expect("no dead job");
    h.check_expired_leases().expect("no expired lease");
    h.check_queue_depth().expect("no queue");
    t.drop_now().await;
}

#[tokio::test]
async fn a_recent_dead_job_is_job_dead_lettered() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 1, T)).await;
    claim(&t.db, &WorkerId::generate(), &[tests::kind("test.a")])
        .await
        .expect("claim")
        .expect("ready");
    expire(&t, id).await;
    reap(&t.db).await.expect("reap");
    let err = health_of(&t).await.check_dead().expect_err("dead");
    assert_reason(&err, ErrorCode::JobDeadLettered, "dead_lettered");
    let text = err.to_string();
    for part in [
        format!("job_id={}", id.get()),
        "kind=test.a".to_owned(),
        "last_error_code=FG-DB-0007".to_owned(),
        "origin_request_id=req-job-origin".to_owned(),
        "dead_recent=1".to_owned(),
    ] {
        assert!(text.contains(&part), "{part}: {text}");
    }
    // Out of the window it is only counted.
    t.exec_superuser("UPDATE forge.jobs SET finished_at = now() - interval '2 days'")
        .await;
    let h = health_of(&t).await;
    assert_eq!((h.dead_recent, h.dead_total), (0, 1));
    h.check_dead().expect("old dead jobs do not alert");
    t.drop_now().await;
}

#[tokio::test]
async fn a_lease_nobody_reaped_is_reported() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    claim(&t.db, &w, &[tests::kind("test.a")])
        .await
        .expect("claim")
        .expect("ready");
    // Just expired: the reaper has time (60 s).
    expire(&t, id).await;
    health_of(&t)
        .await
        .check_expired_leases()
        .expect("within the grace");
    t.exec_superuser("UPDATE forge.jobs SET lease_until = now() - interval '5 minutes'")
        .await;
    let err = health_of(&t)
        .await
        .check_expired_leases()
        .expect_err("expired");
    assert_reason(&err, ErrorCode::JobLeaseExpired, "reaper_behind");
    let text = err.to_string();
    assert!(
        text.contains(&format!("worker_id={}", w.as_str())),
        "{text}"
    );
    assert!(text.contains("expired_leases=1"), "{text}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_deep_or_waiting_queue_is_reported_per_kind() {
    let t = FreshDb::migrated().await;
    ready_rows(&t, "test.small", 3, 1).await;
    let h = health_of(&t).await;
    assert_eq!(h.depth.len(), 1);
    assert_eq!(
        (h.depth[0].kind.as_str(), h.depth[0].ready),
        ("test.small", 3)
    );
    h.check_queue_depth().expect("a small queue is fine");
    // A future job is not ready and not counted.
    t.exec_superuser(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
         origin_request_id, run_after) VALUES ('test.later', '{}', 'queued', 3, 1000, 'r-1', \
         now() + interval '1 hour')",
    )
    .await;
    assert_eq!(health_of(&t).await.depth.len(), 1);
    ready_rows(&t, "test.fleet", QUEUE_DEPTH_ALERT + 1, 1).await;
    let err = health_of(&t)
        .await
        .check_queue_depth()
        .expect_err("too deep");
    assert_reason(&err, ErrorCode::JobDeadLettered, "queue_depth");
    assert!(err.to_string().contains("kind=test.fleet"), "{err}");
    t.exec_superuser("DELETE FROM forge.jobs WHERE kind = 'test.fleet'")
        .await;
    ready_rows(&t, "test.stuck", 1, 3600).await;
    let err = health_of(&t)
        .await
        .check_queue_depth()
        .expect_err("waits too long");
    assert_reason(&err, ErrorCode::JobDeadLettered, "queue_wait");
    assert!(err.to_string().contains("kind=test.stuck"), "{err}");
    t.drop_now().await;
}
