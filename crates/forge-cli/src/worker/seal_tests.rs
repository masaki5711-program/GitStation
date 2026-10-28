//! The worker's `audit_seal` task against a migrated database: it seals the
//! numbered audit rows, drains a backlog larger than one seal without
//! waiting for the policy's minute, and a gap is logged exactly once (with
//! its code and a request id) while the other tasks keep running.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::config::SealKeys;
use forge_db::seal::{backlog, verify_range, SealBacklog, SealPolicy, VerifyScope, MAX_SEAL_ROWS};
use forge_db::sequencer::{self, AuditSequencing, BatchSize, SeqRange};
use forge_db::test_support::FreshDb;
use tokio::sync::watch;

use super::loop_tests::{context, request_id, Captured};
use super::run;

/// The keys every worker test runs with (synthetic).
pub(super) fn seal_keys() -> Arc<SealKeys> {
    Arc::new(SealKeys::parse(&format!("1 {}", "42".repeat(32))).expect("test key"))
}

async fn insert_audit(t: &FreshDb, n: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         SELECT 'system', 'repository.viewed', 'succeeded', 'repository', 'r-' || g \
         FROM generate_series(1, {n}) AS g"
    ))
    .await;
}

/// Number every waiting audit row (the worker itself does not number audit
/// rows before C9's sink comparison).
async fn number_all(t: &FreshDb) {
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("feed state");
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    while sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::IfEnabled)
        .await
        .expect("sequence")
        .audit
        .is_some()
    {}
}

async fn seal_state(t: &FreshDb) -> SealBacklog {
    backlog(&t.db).await.expect("backlog").expect("migrated")
}

/// How many seals cover 1..=last (and verify).
async fn seals_up_to(t: &FreshDb, last: i64) -> usize {
    let range = SeqRange::new(1, last).expect("range");
    verify_range(&t.db, Some(&seal_keys()), VerifyScope::Seqs(range))
        .await
        .expect("verifies")
        .seals
}

async fn stop(
    stop: watch::Sender<bool>,
    worker: tokio::task::JoinHandle<Result<(), forge_errors::ForgeError>>,
) {
    stop.send(true).expect("worker listens");
    tokio::time::timeout(Duration::from_secs(20), worker)
        .await
        .expect("stops in time")
        .expect("no panic")
        .expect("clean stop");
}

#[tokio::test]
async fn the_worker_seals_numbered_rows_and_drains_a_backlog_at_once() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, MAX_SEAL_ROWS + 10).await;
    number_all(&t).await;
    let started = Instant::now();
    let (tx, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = tokio::spawn(async move { run(&db, seal_keys(), stopped).await });
    // The second seal holds fewer than eager_rows rows a moment after the
    // first: only the drain (`more`) seals it before the policy's minute.
    let deadline = started + Duration::from_secs(40);
    loop {
        let b = seal_state(&t).await;
        if b.unsealed_numbered == 0 && b.last_sealed_seq.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "not sealed: {b:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(started.elapsed() < SealPolicy::WORKER.interval);
    stop(tx, worker).await;
    let b = seal_state(&t).await;
    assert_eq!(b.last_sealed_seq, b.audit_high_seq);
    let high = b.audit_high_seq.expect("numbered");
    assert_eq!(seals_up_to(&t, high).await, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn a_gap_is_logged_once_with_its_code_and_request_id_while_the_worker_runs_on() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 2).await;
    number_all(&t).await;
    let high = seal_state(&t).await.audit_high_seq.expect("numbered");
    // forge_seq skips a number within the guards' window.
    insert_audit(&t, 1).await;
    t.exec_superuser(&format!(
        "SET ROLE forge_seq; \
         UPDATE forge.audit_log SET seq = {skip} WHERE seq IS NULL AND request_id = 'r-1'; \
         UPDATE forge.event_feed_state SET audit_high_seq = {skip}; \
         RESET ROLE;",
        skip = high + 2
    ))
    .await;
    let logs = Captured::default();
    let _guard = logs.install();
    let (tx, stopped) = watch::channel(false);
    let db = t.db.clone(); // clone: the worker owns its pool handles
    let worker = tokio::spawn(async move { run(&db, seal_keys(), stopped).await });
    // Retried after the backoff (≤ 5 s) at least once in this time.
    tokio::time::sleep(Duration::from_secs(8)).await;
    // The other tasks still run: events written now get numbered.
    t.exec_superuser(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-ev'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         'repository.created', 1, 'repository', 1, 'system', 'r-1', '{}')",
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while sequencer::backlog(&t.db)
        .await
        .expect("backlog")
        .expect("migrated")
        .unsequenced_events
        > 0
    {
        assert!(Instant::now() < deadline, "the sequencer stopped");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop(tx, worker).await;
    let gaps: Vec<_> = logs
        .failures()
        .into_iter()
        .filter(|e| e["fields"]["error_code"] == "FG-DB-0011")
        .collect();
    assert_eq!(gaps.len(), 1, "logged once per streak: {gaps:?}");
    let gap = &gaps[0];
    assert!(!request_id(gap).is_empty());
    assert_eq!(context(gap, "task").as_deref(), Some("audit_seal"));
    assert_eq!(context(gap, "reason").as_deref(), Some("gap"));
    let missing = (high + 1).to_string();
    assert_eq!(context(gap, "missing_seq"), Some(missing));
    assert_eq!(
        seal_state(&t).await.last_sealed_seq,
        None,
        "nothing sealed past the gap"
    );
    t.drop_now().await;
}
