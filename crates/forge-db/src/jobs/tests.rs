//! The queue statements against a migrated database: SKIP LOCKED claims,
//! the lease, fencing, the backoff, reap, dedup, enqueue inside a change and
//! the roles that may not claim.

use std::collections::BTreeSet;
use std::time::Duration;

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::json;
use sqlx::Connection as _;

use super::*;
use crate::test_support::*;

pub(crate) fn kind(k: &str) -> JobKind {
    JobKind::parse(k).expect("kind")
}

pub(crate) fn new_job(k: &str, dedup: Option<&str>, max: u8, timeout: Duration) -> NewJob {
    NewJob {
        kind: kind(k),
        payload: json!({ "ref": 7 }),
        max_attempts: MaxAttempts::new(max).expect("max"),
        timeout: JobTimeout::new(timeout).expect("timeout"),
        dedup_key: dedup.map(|d| DedupKey::parse(d).expect("dedup")),
        origin_request_id: RequestId::parse("req-job-origin").expect("id"),
        origin_event_seq: Some(42),
    }
}

pub(crate) async fn queued(t: &FreshDb, job: &NewJob) -> JobId {
    match enqueue(&t.db, job).await.expect("enqueue") {
        Enqueued::Queued(id) => id,
        Enqueued::Deduped => panic!("unexpected dedup"),
    }
}

pub(crate) async fn row<T>(t: &FreshDb, sql: &str, id: JobId) -> T
where
    T: for<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
{
    sqlx::query_as(sql)
        .bind(id.get())
        .fetch_one(&mut t.superuser().await)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

pub(crate) async fn state(t: &FreshDb, id: JobId) -> (String, i32, Option<String>, Option<String>) {
    row(
        t,
        "SELECT state, attempts, worker_id, last_error_code FROM forge.jobs WHERE id = $1",
        id,
    )
    .await
}

pub(crate) async fn count_jobs(t: &FreshDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.jobs")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

/// Let a lease expire now (instead of waiting timeout + 30 s).
pub(crate) async fn expire(t: &FreshDb, id: JobId) {
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET lease_until = now() - interval '1 second' WHERE id = {}",
        id.get()
    ))
    .await;
}

/// Make a queued job ready now (skip its backoff).
pub(crate) async fn ready_now(t: &FreshDb, id: JobId) {
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET run_after = now() WHERE id = {}",
        id.get()
    ))
    .await;
}

/// Seconds from now until the job's run_after.
async fn run_after_in(t: &FreshDb, id: JobId) -> f64 {
    let (secs,): (f64,) = row(
        t,
        "SELECT EXTRACT(epoch FROM run_after - now())::float8 FROM forge.jobs WHERE id = $1",
        id,
    )
    .await;
    secs
}

async fn claimed(t: &FreshDb, worker: &WorkerId, k: &str) -> ClaimedJob {
    claim(&t.db, worker, &[kind(k)])
        .await
        .expect("claim")
        .expect("a ready job")
}

fn has(err: &ForgeError, key: &str, value: &str) -> bool {
    err.context().iter().any(|(k, v)| *k == key && v == value)
}

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_claim_sets_state_attempts_worker_and_lease() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let w = WorkerId::generate();
    let job = claimed(&t, &w, "test.a").await;
    assert_eq!(job.lease.job_id, id);
    assert_eq!(job.lease.attempt, 1);
    assert_eq!(job.payload, json!({ "ref": 7 }));
    assert_eq!(job.origin_request_id, "req-job-origin");
    assert_eq!(job.origin_event_seq, Some(42));
    let (st, attempts, worker, _) = state(&t, id).await;
    assert_eq!(
        (st.as_str(), attempts, worker.as_deref()),
        ("running", 1, Some(w.as_str()))
    );
    // lease_until = now() + timeout (5 s) + 30 s.
    let (lease,): (f64,) = row(
        &t,
        "SELECT EXTRACT(epoch FROM lease_until - now())::float8 FROM forge.jobs WHERE id = $1",
        id,
    )
    .await;
    assert!((33.0..=35.5).contains(&lease), "{lease}");
    // Nothing else is ready.
    assert!(claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .is_none());
    t.drop_now().await;
}

#[tokio::test]
async fn a_locked_job_is_skipped_and_concurrent_claimers_get_different_jobs() {
    let t = FreshDb::migrated().await;
    let first = queued(&t, &new_job("test.a", None, 3, T)).await;
    let second = queued(&t, &new_job("test.a", None, 3, T)).await;
    // Another session holds the first row: the claim skips it, never waits.
    let mut holder = t.superuser().await;
    let mut tx = holder.begin().await.expect("begin");
    sqlx::query("SELECT id FROM forge.jobs WHERE id = $1 FOR UPDATE")
        .bind(first.get())
        .execute(&mut *tx)
        .await
        .expect("lock");
    let got = claimed(&t, &WorkerId::generate(), "test.a").await;
    assert_eq!(got.lease.job_id, second);
    tx.rollback().await.expect("rollback");
    drop(holder);
    // Many claimers at once: every job exactly once.
    let mut ids = BTreeSet::from([first]);
    for _ in 0..19 {
        ids.insert(queued(&t, &new_job("test.a", None, 3, T)).await);
    }
    ids.remove(&second);
    let mut claimers = Vec::new();
    for _ in 0..4 {
        let db = t.db.clone(); // clone: each claimer owns its pool handles
        claimers.push(tokio::spawn(async move {
            let w = WorkerId::generate();
            let mut got = Vec::new();
            while let Some(job) = claim(&db, &w, &[kind("test.a")]).await.expect("claim") {
                got.push(job.lease.job_id);
            }
            got
        }));
    }
    let mut all = Vec::new();
    for c in claimers {
        all.extend(c.await.expect("claimer"));
    }
    let unique: BTreeSet<JobId> = all.iter().copied().collect();
    assert_eq!(unique.len(), all.len(), "a job was claimed twice: {all:?}");
    assert_eq!(unique, ids);
    t.drop_now().await;
}

#[tokio::test]
async fn a_claim_takes_only_its_kinds_and_ready_jobs() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let other = queued(&t, &new_job("test.b", None, 3, T)).await;
    assert!(claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .is_none());
    assert!(claim(&t.db, &w, &[]).await.expect("claim").is_none());
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET run_after = now() + interval '1 hour' WHERE id = {}",
        other.get()
    ))
    .await;
    assert!(claim(&t.db, &w, &[kind("test.b")])
        .await
        .expect("claim")
        .is_none());
    ready_now(&t, other).await;
    assert_eq!(claimed(&t, &w, "test.b").await.lease.job_id, other);
    t.drop_now().await;
}

#[tokio::test]
async fn a_stale_worker_is_fenced_and_the_newer_result_is_kept() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let (a, b) = (WorkerId::generate(), WorkerId::generate());
    let stale = claimed(&t, &a, "test.a").await;
    expire(&t, id).await;
    assert_eq!(reap(&t.db).await.expect("reap").len(), 1);
    ready_now(&t, id).await;
    let fresh = claimed(&t, &b, "test.a").await;
    assert_eq!(fresh.lease.attempt, 2);
    // The stale worker cannot settle the newer attempt.
    let failure = JobFailure::retry(ForgeError::new(ErrorCode::Internal));
    let Failed::Fenced(e) = fail(&t.db, &stale, &failure).await.expect("fail") else {
        panic!("a stale worker was not fenced");
    };
    assert_eq!(e.code(), ErrorCode::JobLeaseExpired, "{e}");
    assert!(has(&e, "reason", "fenced"), "{e}");
    assert!(has(&e, "worker_id", a.as_str()), "{e}");
    assert!(has(&e, "attempt", "1"), "{e}");
    assert!(has(&e, "job_id", &id.get().to_string()), "{e}");
    assert!(matches!(
        complete(&t.db, &fresh).await.expect("complete"),
        Completed::Applied
    ));
    match complete(&t.db, &stale).await.expect("complete") {
        Completed::Fenced(e) => assert_eq!(e.code(), ErrorCode::JobLeaseExpired),
        Completed::Applied => panic!("a stale worker completed the job"),
    }
    let (st, attempts, worker, code) = state(&t, id).await;
    assert_eq!(st, "succeeded");
    assert_eq!(attempts, 2);
    assert_eq!(worker.as_deref(), Some(b.as_str()));
    assert_eq!(code.as_deref(), Some("FG-DB-0007"), "the reap is recorded");
    // The same worker re-claiming: the old attempt is fenced by `attempts`.
    let id = queued(&t, &new_job("test.s", None, 3, T)).await;
    let old = claimed(&t, &a, "test.s").await;
    expire(&t, id).await;
    reap(&t.db).await.expect("reap");
    ready_now(&t, id).await;
    let new = claimed(&t, &a, "test.s").await;
    assert!(matches!(
        complete(&t.db, &old).await.expect("complete"),
        Completed::Fenced(_)
    ));
    assert_eq!(state(&t, id).await.0, "running");
    assert!(matches!(
        complete(&t.db, &new).await.expect("complete"),
        Completed::Applied
    ));
    t.drop_now().await;
}

#[tokio::test]
async fn a_failure_retries_with_backoff_until_the_attempts_are_used_up() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 2, T)).await;
    let w = WorkerId::generate();
    let failure = JobFailure::retry(
        ForgeError::new(ErrorCode::DbQueryFailed).with_context("reason", "lock_timeout"),
    );
    let first = claimed(&t, &w, "test.a").await;
    let Failed::Retrying { retry_in } = fail(&t.db, &first, &failure).await.expect("fail") else {
        panic!("the first failure is retried");
    };
    assert!(retry_in <= Duration::from_secs(10), "{retry_in:?}");
    assert!(retry_in >= Duration::from_secs(8), "{retry_in:?}");
    let (st, _, _, code) = state(&t, id).await;
    assert_eq!(
        (st.as_str(), code.as_deref()),
        ("queued", Some("FG-DB-0002"))
    );
    let (last,): (String,) = row(
        &t,
        "SELECT last_error::text FROM forge.jobs WHERE id = $1",
        id,
    )
    .await;
    assert!(last.contains("lock_timeout"), "{last}");
    // Not ready during the backoff.
    assert!(claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .is_none());
    ready_now(&t, id).await;
    let second = claimed(&t, &w, "test.a").await;
    assert!(matches!(
        fail(&t.db, &second, &failure).await.expect("fail"),
        Failed::Dead
    ));
    let (st, attempts, _, _) = state(&t, id).await;
    assert_eq!((st.as_str(), attempts), ("dead", 2));
    // A permanent failure is dead at once.
    let id = queued(&t, &new_job("test.p", None, 5, T)).await;
    let job = claimed(&t, &w, "test.p").await;
    let never = JobFailure::permanent(ForgeError::new(ErrorCode::Internal));
    assert!(matches!(
        fail(&t.db, &job, &never).await.expect("fail"),
        Failed::Dead
    ));
    let (st, attempts, _, code) = state(&t, id).await;
    assert_eq!(
        (st.as_str(), attempts, code.as_deref()),
        ("dead", 1, Some("FG-SYS-0001"))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn reap_requeues_or_deads_expired_leases_in_one_statement_with_backoff() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let retry = queued(&t, &new_job("test.r", None, 3, T)).await;
    let last = queued(&t, &new_job("test.d", None, 1, T)).await;
    let alive = queued(&t, &new_job("test.l", None, 3, T)).await;
    claimed(&t, &w, "test.r").await;
    claimed(&t, &w, "test.d").await;
    claimed(&t, &w, "test.l").await;
    expire(&t, retry).await;
    expire(&t, last).await;
    let mut reaped = reap(&t.db).await.expect("reap");
    reaped.sort_by_key(|r| r.job_id);
    assert_eq!(reaped.len(), 2, "{reaped:?}");
    assert_eq!((reaped[0].job_id, reaped[0].dead), (retry, false));
    assert_eq!((reaped[1].job_id, reaped[1].dead), (last, true));
    assert_eq!(reaped[0].worker_id.as_deref(), Some(w.as_str()));
    assert_eq!(reaped[0].origin_request_id, "req-job-origin");
    let (st, attempts, _, code) = state(&t, retry).await;
    assert_eq!(
        (st.as_str(), attempts, code.as_deref()),
        ("queued", 1, Some("FG-DB-0007"))
    );
    let wait = run_after_in(&t, retry).await;
    assert!((8.0..=10.5).contains(&wait), "first backoff 10 s: {wait}");
    let (st, _, _, code) = state(&t, last).await;
    assert_eq!((st.as_str(), code.as_deref()), ("dead", Some("FG-DB-0007")));
    let (finished,): (bool,) = row(
        &t,
        "SELECT finished_at IS NOT NULL FROM forge.jobs WHERE id = $1",
        last,
    )
    .await;
    assert!(finished);
    assert_eq!(state(&t, alive).await.0, "running", "a live lease stays");
    // The second expiry of the same job waits twice as long.
    ready_now(&t, retry).await;
    claimed(&t, &w, "test.r").await;
    expire(&t, retry).await;
    reap(&t.db).await.expect("reap");
    let wait = run_after_in(&t, retry).await;
    assert!((18.0..=20.5).contains(&wait), "second backoff 20 s: {wait}");
    assert!(reap(&t.db).await.expect("reap").is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_dedup_key_prevents_a_double_enqueue() {
    let t = FreshDb::migrated().await;
    let job = new_job("test.a", Some("repo:7:seal"), 3, T);
    queued(&t, &job).await;
    assert_eq!(
        enqueue(&t.db, &job).await.expect("enqueue"),
        Enqueued::Deduped
    );
    // The key is per kind; no key never dedups.
    queued(&t, &new_job("test.b", Some("repo:7:seal"), 3, T)).await;
    queued(&t, &new_job("test.a", None, 3, T)).await;
    queued(&t, &new_job("test.a", None, 3, T)).await;
    assert_eq!(count_jobs(&t).await, 4);
    t.drop_now().await;
}
