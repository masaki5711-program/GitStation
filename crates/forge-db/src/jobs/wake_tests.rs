//! Waking the job runners (plan 18 S2): an enqueue is claimed long before
//! the safety-net period, a rolled-back enqueue wakes no one, a scheduled
//! job starts at its due time, a runner whose listener is down polls every
//! second, and a job the safety net found although the listener was up is
//! FG-DB-0024 — at most once a minute.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::RequestId;
use forge_errors::{ErrorCode, ForgeError};
use serde_json::Value;
use tokio::sync::Notify;

use super::runner_stop_tests::Captured;
use super::runner_tests::{runner, Script};
use super::tests::{count_jobs, kind, new_job, queued};
use super::*;
use crate::test_support::*;
use crate::wake::{Channel, WakeHub, Woken};

const T: Duration = Duration::from_secs(5);
/// The default of `jobs.safety_poll_ms`.
const SAFETY: Duration = Duration::from_secs(10);
/// "Well before the safety period": a wake takes milliseconds, the bound
/// leaves room for a loaded test server.
const WELL_BEFORE: Duration = Duration::from_secs(3);
/// Far longer than a connect or a notification takes.
const SOON: Duration = Duration::from_secs(30);

/// A connected hub on `t.db`, the runners made after it wait on it.
async fn hub_on(t: &mut FreshDb, safety: Duration) -> WakeHub {
    let hub = WakeHub::spawn(&t.db, &Channel::ALL).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    t.db = t.db.clone().with_wake(hub.clone(), safety); // clone: handles
    hub
}

/// A handler that tells `ran` and succeeds.
fn telling(ran: &Arc<Notify>) -> Script {
    let ran = Arc::clone(ran);
    Arc::new(move |_| {
        let ran = Arc::clone(&ran);
        Box::pin(async move {
            ran.notify_one();
            Ok(())
        })
    })
}

/// What `forge worker` does with one slot: run, and wait when idle.
fn work(r: &Arc<JobRunner>) -> tokio::task::JoinHandle<()> {
    let r = Arc::clone(r);
    tokio::spawn(async move {
        loop {
            if r.run_once().await.expect("run").is_none() {
                r.wait_for_work().await.expect("wait");
            }
        }
    })
}

/// A ready job no notification announces (written behind the queue's
/// back), ready for `ready_s` seconds — or due in `-ready_s`.
async fn silent_job(t: &FreshDb, k: &str, ready_s: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
         origin_request_id, run_after) VALUES ('{k}', '{{}}', 'queued', 3, 5000, 'req-silent', \
         now() - interval '{ready_s} seconds')"
    ))
    .await;
}

async fn within(what: &str, limit: Duration, ran: &Notify) -> Duration {
    let started = Instant::now();
    tokio::time::timeout(limit, ran.notified())
        .await
        .unwrap_or_else(|_| panic!("{what}: not run within {limit:?}"));
    started.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enqueued_job_is_claimed_without_waiting_for_the_poll() {
    let mut t = FreshDb::migrated().await;
    let hub = hub_on(&mut t, SAFETY).await;
    let ran = Arc::new(Notify::new());
    let r = Arc::new(runner(&t, "test.a", telling(&ran)));
    assert!(r.waits_on_wake());
    let worker = work(&r);
    // The runner looked, found nothing and waits: nothing but a wake ends
    // that wait for the next 10 s.
    tokio::time::sleep(Duration::from_millis(500)).await;
    queued(&t, &new_job("test.a", None, 3, T)).await;
    let took = within("the enqueued job", SAFETY + T, &ran).await;
    assert!(
        took < WELL_BEFORE,
        "claimed {took:?} after the enqueue (safety period {SAFETY:?})"
    );
    // Again, from the idle wait after a run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    queued(&t, &new_job("test.a", None, 3, T)).await;
    let took = within("the second job", SAFETY + T, &ran).await;
    assert!(took < WELL_BEFORE, "{took:?}");
    worker.abort();
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test]
async fn a_rolled_back_enqueue_wakes_no_one() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &[Channel::Jobs]).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    let job = new_job("test.a", None, 3, T);

    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let added = enqueue_on(&mut tx, &job).await.expect("enqueue");
    assert!(matches!(added, Enqueued::Queued(_)), "{added:?}");
    tx.rollback().await.expect("rollback");
    assert_eq!(rx.wait(Duration::from_millis(1500)).await, Woken::SafetyNet);
    assert_eq!(count_jobs(&t).await, 0);

    // The same enqueue, committed: the waiter is told the kind — and only
    // when the transaction commits.
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    enqueue_on(&mut tx, &job).await.expect("enqueue");
    assert_eq!(rx.wait(Duration::from_millis(500)).await, Woken::SafetyNet);
    tx.commit().await.expect("commit");
    assert_eq!(
        rx.wait(SOON).await,
        Woken::Notified(vec!["test.a".to_owned()])
    );
    assert_eq!(count_jobs(&t).await, 1);
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test]
async fn every_statement_that_makes_a_job_claimable_notifies_its_kind() {
    let t = FreshDb::migrated().await;
    let hub = WakeHub::spawn(&t.db, &[Channel::Jobs]).expect("hub");
    let mut rx = hub.subscribe_unseen(Channel::Jobs);
    assert_eq!(rx.wait(SOON).await, Woken::Reconnected);
    let told = |k: &str| Woken::Notified(vec![k.to_owned()]);
    let quiet = Duration::from_millis(500);

    // An enqueue; a deduplicated one adds nothing and tells no one.
    queued(&t, &new_job("test.a", Some("d1"), 3, T)).await;
    assert_eq!(rx.wait(SOON).await, told("test.a"));
    let again = enqueue(&t.db, &new_job("test.a", Some("d1"), 3, T))
        .await
        .expect("enqueue");
    assert_eq!(again, Enqueued::Deduped);
    assert_eq!(rx.wait(quiet).await, Woken::SafetyNet);

    // A delayed enqueue tells at once (the waiter reads the due time).
    let mut conn = t.db.write_pool().acquire().await.expect("connection");
    let later = new_job("test.b", None, 3, T);
    enqueue_after_on(&mut conn, &later, Duration::from_secs(60))
        .await
        .expect("enqueue after");
    assert_eq!(rx.wait(SOON).await, told("test.b"));

    // A superseding enqueue.
    let mut request = new_job("test.c", Some("r1"), 3, T);
    request.payload = serde_json::json!({ "merge_request_id": "7" });
    let first = enqueue_superseding_on(&mut conn, &request, 7)
        .await
        .expect("superseding");
    assert!(matches!(first.enqueued, Enqueued::Queued(_)), "{first:?}");
    assert_eq!(rx.wait(SOON).await, told("test.c"));
    let same = enqueue_superseding_on(&mut conn, &request, 7)
        .await
        .expect("superseding");
    assert_eq!((same.enqueued, same.superseded), (Enqueued::Deduped, 0));
    assert_eq!(rx.wait(quiet).await, Woken::SafetyNet);
    drop(conn);

    // A job handed back at shutdown is ready now.
    let w = WorkerId::generate();
    let job = claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .expect("a job");
    assert_eq!(rx.wait(quiet).await, Woken::SafetyNet);
    assert!(release(&t.db, &job).await.expect("release"));
    assert_eq!(rx.wait(SOON).await, told("test.a"));
    // A release that changes nothing (the lease is gone) tells no one.
    let job = claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .expect("a job");
    let done = complete(&t.db, &job).await.expect("complete");
    assert!(matches!(done, Completed::Applied), "{done:?}");
    assert!(!release(&t.db, &job).await.expect("release"));
    assert_eq!(rx.wait(quiet).await, Woken::SafetyNet);

    // A failure that schedules a retry tells (the due time changed); one
    // that ends the job does not.
    let retried = queued(&t, &new_job("test.d", None, 3, T)).await;
    assert_eq!(rx.wait(SOON).await, told("test.d"));
    let job = claim(&t.db, &w, &[kind("test.d")])
        .await
        .expect("claim")
        .expect("a job");
    let failure = JobFailure::retry(ForgeError::new(ErrorCode::Internal));
    let failed = fail(&t.db, &job, &failure).await.expect("fail");
    assert!(matches!(failed, Failed::Retrying { .. }), "{failed:?}");
    assert_eq!(rx.wait(SOON).await, told("test.d"));
    super::tests::ready_now(&t, retried).await;
    let job = claim(&t.db, &w, &[kind("test.d")])
        .await
        .expect("claim")
        .expect("a job");
    let failure = JobFailure::permanent(ForgeError::new(ErrorCode::Internal));
    let failed = fail(&t.db, &job, &failure).await.expect("fail");
    assert!(matches!(failed, Failed::Dead), "{failed:?}");
    assert_eq!(rx.wait(quiet).await, Woken::SafetyNet);

    // So does the reaper, for the leases it queues again.
    let expired = queued(&t, &new_job("test.e", None, 3, T)).await;
    assert_eq!(rx.wait(SOON).await, told("test.e"));
    claim(&t.db, &w, &[kind("test.e")])
        .await
        .expect("claim")
        .expect("a job");
    super::tests::expire(&t, expired).await;
    let reaped = reap(&t.db).await.expect("reap");
    assert_eq!((reaped.len(), reaped[0].dead), (1, false), "{reaped:?}");
    assert_eq!(rx.wait(SOON).await, told("test.e"));
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scheduled_job_starts_at_its_due_time() {
    let mut t = FreshDb::migrated().await;
    let hub = hub_on(&mut t, SAFETY).await;
    let ran = Arc::new(Notify::new());
    let r = Arc::new(runner(&t, "test.a", telling(&ran)));
    // Due in 2 s when the runner goes idle: no notification will come.
    silent_job(&t, "test.a", -2).await;
    let worker = work(&r);
    let took = within("the scheduled job", SAFETY + T, &ran).await;
    assert!(
        took >= Duration::from_millis(1500) && took < Duration::from_secs(5),
        "due in 2 s, run after {took:?}"
    );
    // Enqueued with a delay while the runner waits its 10 s: the enqueue
    // wakes it, and its next wait ends at the due time.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut conn = t.db.write_pool().acquire().await.expect("connection");
    enqueue_after_on(
        &mut conn,
        &new_job("test.a", None, 3, T),
        Duration::from_secs(2),
    )
    .await
    .expect("enqueue after");
    drop(conn);
    let took = within("the delayed job", SAFETY + T, &ran).await;
    assert!(
        took >= Duration::from_millis(1500) && took < Duration::from_secs(5),
        "delayed 2 s, run after {took:?}"
    );
    worker.abort();
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}

#[tokio::test]
async fn a_runner_whose_listener_is_down_polls_every_second() {
    let mut t = FreshDb::migrated().await;
    let hub = hub_on(&mut t, SAFETY).await;
    let ran = Arc::new(Notify::new());
    let r = runner(&t, "test.a", telling(&ran));
    hub.stop(SOON).await.expect("stop");
    assert!(!hub.is_live(Channel::Jobs));
    // The loss ends one wait at once (look now); after that a wait is the
    // one-second poll of before plan 18 — not 10 s, and never a spin.
    r.wait_for_work().await.expect("wait");
    assert!(r.run_once().await.expect("run").is_none());
    let started = Instant::now();
    r.wait_for_work().await.expect("wait");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(900) && waited < WELL_BEFORE,
        "{waited:?}"
    );
    silent_job(&t, "test.a", 0).await;
    r.wait_for_work().await.expect("wait");
    assert!(r.run_once().await.expect("run").is_some());
    // A runner on a Db without a hub does not wait at all: its caller
    // keeps the poll it had.
    let plain = FreshDb::migrated().await;
    let p = runner(&plain, "test.a", telling(&ran));
    assert!(!p.waits_on_wake());
    let started = Instant::now();
    p.wait_for_work().await.expect("wait");
    assert!(started.elapsed() < Duration::from_millis(500));
    plain.drop_now().await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_safety_net_find_logs_wakeup_missed_once_per_minute() {
    let logs = Captured::default();
    let _guard = logs.install();
    let mut t = FreshDb::migrated().await;
    let safety = Duration::from_millis(300);
    let hub = hub_on(&mut t, safety).await;
    let ran = Arc::new(Notify::new());
    let r = runner(&t, "test.a", telling(&ran));
    let missed = |logs: &Captured| -> Vec<Value> {
        logs.failures()
            .into_iter()
            .filter(|e| e["fields"]["error_code"] == "FG-DB-0024")
            .collect()
    };

    // An announced job found after a wake is no miss.
    assert!(r.run_once().await.expect("run").is_none());
    let mut arrived = hub.subscribe(Channel::Jobs);
    queued(&t, &new_job("test.a", None, 3, T)).await;
    assert!(matches!(arrived.wait(SOON).await, Woken::Notified(_)));
    r.wait_for_work().await.expect("wait");
    assert!(r.run_once().await.expect("run").is_some());
    assert!(missed(&logs).is_empty(), "{:?}", missed(&logs));

    // A job no notification announced, ready for a minute: the runner
    // waits its whole safety period, then finds it.
    assert!(r.run_once().await.expect("run").is_none());
    silent_job(&t, "test.a", 60).await;
    let started = Instant::now();
    r.wait_for_work().await.expect("wait");
    assert!(started.elapsed() >= safety);
    assert!(r.run_once().await.expect("run").is_some());
    let first = missed(&logs);
    assert_eq!(first.len(), 1, "{:?}", logs.failures());
    assert_eq!(first[0]["level"], "WARN", "{first:?}");
    assert_eq!(first[0]["fields"]["request_id"], "req-silent", "{first:?}");
    let context = first[0]["fields"]["context"].as_str().unwrap_or_default();
    for part in [
        "channel=forge_jobs",
        "safety_poll_ms=300",
        "kind=test.a",
        "oldest_ready_ms=",
        "job_id=",
        "worker_id=",
    ] {
        assert!(context.contains(part), "{part} in {context}");
    }

    // The same again within the minute: found and run, not logged again.
    assert!(r.run_once().await.expect("run").is_none());
    silent_job(&t, "test.a", 60).await;
    r.wait_for_work().await.expect("wait");
    assert!(r.run_once().await.expect("run").is_some());
    assert_eq!(missed(&logs).len(), 1, "{:?}", missed(&logs));
    // The limit is the hub's, per channel and process.
    let id = RequestId::generate();
    let another = || ForgeError::new(ErrorCode::WakeupMissed);
    assert!(!hub.missed(Channel::Jobs, another(), &id));
    assert_eq!(missed(&logs).len(), 1);
    assert!(hub.missed(Channel::MergeQueue, another(), &id));
    assert!(!hub.missed(Channel::MergeQueue, another(), &id));
    assert_eq!(missed(&logs).len(), 2);
    hub.stop(SOON).await.expect("stop");
    t.drop_now().await;
}
