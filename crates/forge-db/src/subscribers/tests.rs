//! Subscriber steps against a migrated database: one event per
//! transaction, the monotonic cursor, NOWAIT contention, stop / quarantine
//! after five failures, registration, and what forge_app may not change.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex};

use forge_errors::ErrorCode;
use serde_json::json;
use sqlx::Connection as _;

use super::*;
use crate::change::DomainEvent;
use crate::sequencer::{self, AuditSequencing, BatchSize};
use crate::test_support::*;

/// A scripted subscriber: records the seqs it saw, fails while `failing`,
/// derives `derive` events per source event and writes a jobs row (a
/// derived table) each time, so a rollback is visible.
pub(crate) struct Scripted {
    pub name: &'static str,
    pub ordering: Ordering,
    pub failing: AtomicBool,
    pub derive: AtomicUsize,
    pub seen: Mutex<Vec<i64>>,
}

impl Scripted {
    pub(crate) fn new(name: &'static str, ordering: Ordering) -> Arc<Self> {
        Arc::new(Self {
            name,
            ordering,
            failing: AtomicBool::new(false),
            derive: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn seen(&self) -> Vec<i64> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

impl Subscriber for Scripted {
    fn name(&self) -> &'static str {
        self.name
    }

    fn ordering(&self) -> Ordering {
        self.ordering
    }

    fn handle<'a>(&'a self, event: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            self.seen.lock().expect("not poisoned").push(event.seq);
            sqlx::query(
                "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
                 origin_request_id, origin_event_seq) VALUES ('test.job', '{}', 'queued', 1, \
                 1000, $1, $2)",
            )
            .bind(&event.request_id)
            .bind(event.seq)
            .execute(tx.conn())
            .await
            .map_err(|e| crate::query_error("test.job", e))?;
            for n in 0..self.derive.load(Atomic::SeqCst) {
                tx.emit(derived_event(n)).await?;
            }
            if self.failing.load(Atomic::SeqCst) {
                return Err(ForgeError::new(ErrorCode::GitUnavailable)
                    .with_context("reason", "test_failure"));
            }
            Ok(())
        })
    }
}

pub(crate) fn derived_event(n: usize) -> DomainEvent {
    DomainEvent {
        event_type: "repository.indexed",
        schema_version: 1,
        aggregate_type: "repository",
        aggregate_id: i64::try_from(n).expect("small"),
        project_id: None,
        payload: json!({ "n": n }),
        dedup_key: None,
    }
}

/// `n` events written and numbered.
pub(crate) async fn published(t: &FreshDb, n: i64) {
    sequencer::tests::insert_events(t, n).await;
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
}

pub(crate) async fn scalar(t: &FreshDb, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut t.superuser().await)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

pub(crate) async fn cursor(t: &FreshDb, name: &str) -> i64 {
    scalar(
        t,
        &format!("SELECT last_seq FROM forge.event_subscribers WHERE name = '{name}'"),
    )
    .await
}

pub(crate) async fn step(t: &FreshDb, s: &Scripted) -> Progress {
    run_once(&t.db, s).await.expect("run_once")
}

#[tokio::test]
async fn each_step_handles_one_event_and_the_cursor_only_moves_forward() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("counter", Ordering::Ordered);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    published(&t, 3).await;
    for expected in 1..=3 {
        let Progress::Handled { seq, derived } = step(&t, &s).await else {
            panic!("step {expected} is handled");
        };
        assert_eq!((seq, derived), (expected, 0));
        assert_eq!(cursor(&t, "counter").await, expected);
        // One event per transaction: the jobs row of this step only.
        assert_eq!(
            scalar(&t, "SELECT count(*) FROM forge.jobs").await,
            expected
        );
    }
    assert!(matches!(step(&t, &s).await, Progress::CaughtUp));
    assert_eq!(s.seen(), vec![1, 2, 3]);
    // last_txid names the transaction that advanced it.
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.event_subscribers WHERE name = 'counter' \
             AND last_txid IS NOT NULL AND failures = 0"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_new_subscriber_starts_at_the_head_and_keeps_its_cursor() {
    let t = FreshDb::migrated().await;
    published(&t, 4).await;
    let s = Scripted::new("late", Ordering::Independent);
    register(&t.db, s.as_ref(), Start::Head)
        .await
        .expect("register");
    assert_eq!(cursor(&t, "late").await, 4);
    assert!(matches!(step(&t, &s).await, Progress::CaughtUp));
    published(&t, 1).await;
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 5, .. }
    ));
    // Registering again keeps the cursor; another ordering is refused.
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("again");
    assert_eq!(cursor(&t, "late").await, 5);
    let other = Scripted::new("late", Ordering::Ordered);
    let err = register(&t.db, other.as_ref(), Start::Head)
        .await
        .expect_err("ordering differs");
    assert_reason(&err, ErrorCode::Internal, "ordering_mismatch");
    t.drop_now().await;
}

#[tokio::test]
async fn a_subscriber_held_by_another_worker_is_busy_not_an_error() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("shared", Ordering::Ordered);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    published(&t, 1).await;
    // Another worker's step holds the row.
    let mut other = t.db.write_pool().acquire().await.expect("acquire");
    let mut held = other.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM forge.event_subscribers WHERE name = 'shared' FOR UPDATE")
        .execute(&mut *held)
        .await
        .expect("lock");
    assert!(matches!(step(&t, &s).await, Progress::Busy));
    assert!(s.seen().is_empty());
    held.rollback().await.expect("rollback");
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 1, .. }
    ));
    t.drop_now().await;
}

#[tokio::test]
async fn an_ordered_subscriber_stops_after_five_failures() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("sync", Ordering::Ordered);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    published(&t, 2).await;
    s.failing.store(true, Atomic::SeqCst);
    for attempt in 1..=4 {
        let Progress::Failed(incident) = step(&t, &s).await else {
            panic!("attempt {attempt} is a plain failure");
        };
        assert_eq!(incident.error.code(), ErrorCode::GitUnavailable);
        assert_eq!(incident.seq, Some(1));
        assert_eq!(
            incident.request_id.as_ref().map(|r| r.as_str()),
            Some("r-1")
        );
    }
    let Progress::Stuck(incident) = step(&t, &s).await else {
        panic!("the fifth failure stops it");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "stuck");
    let text = incident.error.to_string();
    for part in [
        "subscriber=sync",
        "seq=1",
        "event_type=repository.created",
        "lag_events=2",
        "failures=5",
        "last_error_code=FG-GIT-",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    // It never skips: still at 0, retried on the same event, and the
    // failed attempts left nothing behind.
    assert!(matches!(step(&t, &s).await, Progress::Stuck(_)));
    assert_eq!(cursor(&t, "sync").await, 0);
    assert_eq!(scalar(&t, "SELECT count(*) FROM forge.jobs").await, 0);
    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.subscriber_dead_letters").await,
        0
    );
    // Once the handler works again it resumes and the streak is cleared.
    s.failing.store(false, Atomic::SeqCst);
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 1, .. }
    ));
    assert_eq!(
        scalar(
            &t,
            "SELECT failures::bigint FROM forge.event_subscribers WHERE name = 'sync' \
             AND failing_since IS NULL AND last_error_code IS NULL"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_independent_subscriber_quarantines_after_five_failures() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("notify", Ordering::Independent);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    published(&t, 2).await;
    s.failing.store(true, Atomic::SeqCst);
    for _ in 1..=4 {
        assert!(matches!(step(&t, &s).await, Progress::Failed(_)));
    }
    let Progress::Quarantined(incident) = step(&t, &s).await else {
        panic!("the fifth failure quarantines");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "quarantined");
    assert_eq!(cursor(&t, "notify").await, 1);
    let (seq, code, attempts): (i64, String, i32) = sqlx::query_as(
        "SELECT seq, error_code, attempts FROM forge.subscriber_dead_letters \
         WHERE subscriber = 'notify'",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("dead letter");
    assert_eq!(
        (seq, code.as_str(), attempts),
        (1, ErrorCode::GitUnavailable.id(), 5)
    );
    // It moved on: the next event is handled once the handler works.
    s.failing.store(false, Atomic::SeqCst);
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 2, .. }
    ));
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_subscriber_does_nothing() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("off", Ordering::Independent);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    published(&t, 1).await;
    t.exec_superuser("UPDATE forge.event_subscribers SET enabled = false WHERE name = 'off'")
        .await;
    assert!(matches!(step(&t, &s).await, Progress::Disabled));
    let unknown = Scripted::new("unknown", Ordering::Independent);
    let err = run_once(&t.db, unknown.as_ref())
        .await
        .expect_err("not registered");
    assert_reason(&err, ErrorCode::Internal, "subscriber_not_registered");
    t.drop_now().await;
}

#[tokio::test]
async fn forge_app_cannot_flip_enabled_or_retention_blocking() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("guarded", Ordering::Independent);
    register(&t.db, s.as_ref(), Start::Head)
        .await
        .expect("register");
    for sql in [
        "UPDATE forge.event_subscribers SET enabled = false WHERE name = 'guarded'",
        "UPDATE forge.event_subscribers SET retention_blocking = false WHERE name = 'guarded'",
        "INSERT INTO forge.event_subscribers (name, ordering, last_seq, updated_at, \
         retention_blocking) VALUES ('x', 'ordered', 0, now(), false)",
        "DELETE FROM forge.subscriber_dead_letters",
    ] {
        let err = sqlx::query(sql)
            .execute(t.db.write_pool())
            .await
            .map_err(|e| crate::query_error("test.flip", e))
            .expect_err(sql);
        assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    }
    t.drop_now().await;
}
