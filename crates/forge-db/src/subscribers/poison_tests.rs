//! Poison events and failures outside the handler (C5 review): an event
//! that cannot be decoded, a savepoint the handler left aborted, and
//! derived writes refused at COMMIT are counted like a handler failure
//! (stuck / quarantined after five), a hung handler times out into a
//! failure, and the infrastructure errors (event missing, subscriber not
//! registered) name their reason.

use std::time::Duration;

use forge_errors::ErrorCode;

use super::run::{run_once_with, Timeouts};
use super::tests::{cursor, published, scalar, step, Scripted};
use super::*;
use crate::query_error;
use crate::test_support::*;

/// The failure streak stored for `name`.
async fn failures(t: &FreshDb, name: &str) -> i64 {
    scalar(
        t,
        &format!("SELECT failures::bigint FROM forge.event_subscribers WHERE name = '{name}'"),
    )
    .await
}

async fn last_error_code(t: &FreshDb, name: &str) -> String {
    sqlx::query_scalar(
        "SELECT coalesce(last_error_code, 'none') FROM forge.event_subscribers WHERE name = $1",
    )
    .bind(name)
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("last_error_code")
}

async fn registered(t: &FreshDb, s: &dyn Subscriber) {
    register(&t.db, s, Start::Beginning)
        .await
        .expect("register");
}

/// One published event whose payload is nested deeper than serde_json
/// reads (128 levels): stored fine, never decodable.
async fn publish_undecodable(t: &FreshDb) {
    let payload = format!("{}{}", "[".repeat(200), "]".repeat(200));
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-deep'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         'repository.created', 1, 'repository', 1, 'system', 'r-deep', '{payload}')"
    ))
    .await;
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequencer");
}

#[tokio::test]
async fn an_undecodable_event_is_counted_until_an_ordered_subscriber_is_stuck() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("decoder", Ordering::Ordered);
    registered(&t, s.as_ref()).await;
    publish_undecodable(&t).await;
    for attempt in 1..=4 {
        let Progress::Failed(incident) = step(&t, &s).await else {
            panic!("attempt {attempt} is a counted failure");
        };
        assert_reason(&incident.error, ErrorCode::Internal, "decode");
        assert_eq!(incident.seq, Some(1));
        assert!(incident.event_id.is_some(), "{incident:?}");
        assert_eq!(
            incident.request_id.as_ref().map(|r| r.as_str()),
            Some("r-deep")
        );
    }
    let Progress::Stuck(incident) = step(&t, &s).await else {
        panic!("the fifth failure stops it");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "stuck");
    assert!(incident
        .error
        .to_string()
        .contains("last_error_code=FG-SYS-0001"));
    assert_eq!(
        (failures(&t, "decoder").await, cursor(&t, "decoder").await),
        (5, 0)
    );
    assert!(s.seen().is_empty(), "the handler never saw it");
    t.drop_now().await;
}

#[tokio::test]
async fn an_undecodable_event_is_quarantined_by_an_independent_subscriber() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("lossy-decoder", Ordering::Independent);
    registered(&t, s.as_ref()).await;
    publish_undecodable(&t).await;
    for _ in 1..=4 {
        assert!(matches!(step(&t, &s).await, Progress::Failed(_)));
    }
    assert!(matches!(step(&t, &s).await, Progress::Quarantined(_)));
    assert_eq!(cursor(&t, "lossy-decoder").await, 1);
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.subscriber_dead_letters \
             WHERE seq = 1 AND error_code = 'FG-SYS-0001'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

/// Swallows a failed statement: its savepoint is left aborted, so RELEASE
/// fails although `handle` returned Ok.
struct Swallows;

impl Subscriber for Swallows {
    fn name(&self) -> &'static str {
        "swallower"
    }
    fn ordering(&self) -> Ordering {
        Ordering::Independent
    }
    fn handle<'a>(&'a self, _e: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            let swallowed = sqlx::query("SELECT 1 / 0").execute(tx.conn()).await;
            assert!(swallowed.is_err(), "division by zero fails");
            Ok(())
        })
    }
}

#[tokio::test]
async fn a_failed_savepoint_release_is_counted_as_a_failure() {
    let t = FreshDb::migrated().await;
    registered(&t, &Swallows).await;
    published(&t, 1).await;
    let progress = run_once(&t.db, &Swallows)
        .await
        .expect("counted, not an error");
    let Progress::Failed(incident) = progress else {
        panic!("a failure: {progress:?}");
    };
    assert_eq!(incident.error.code(), ErrorCode::DbQueryFailed);
    assert!(
        incident
            .error
            .to_string()
            .contains("query=subscribers.release"),
        "{}",
        incident.error
    );
    assert_eq!(incident.seq, Some(1));
    assert_eq!(
        (
            failures(&t, "swallower").await,
            cursor(&t, "swallower").await
        ),
        (1, 0)
    );
    assert_eq!(last_error_code(&t, "swallower").await, "FG-DB-0002");
    t.drop_now().await;
}

/// Writes a derived event of the previous event by hand (lineage right,
/// but its subscriber never advanced to it): refused at COMMIT
/// (XF002 derived_progress).
struct Forges;

impl Subscriber for Forges {
    fn name(&self) -> &'static str {
        "forger"
    }
    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }
    fn handle<'a>(&'a self, e: &'a SourceEvent, tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(async move {
            if e.seq < 2 {
                return Ok(());
            }
            sqlx::query(
                "INSERT INTO forge.events (event_id, event_type, schema_version, \
                 aggregate_type, aggregate_id, actor_kind, request_id, caused_by_seq, root_seq, \
                 causation_depth, payload) VALUES (gen_random_uuid(), 'repository.indexed', 1, \
                 'repository', 1, 'system', 'r', $1, $1, 1, '{}')",
            )
            .bind(e.seq - 1)
            .execute(tx.conn())
            .await
            .map_err(|err| query_error("test.forged", err))?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn derived_writes_refused_at_commit_are_counted_until_stuck() {
    let t = FreshDb::migrated().await;
    registered(&t, &Forges).await;
    published(&t, 2).await;
    assert!(matches!(
        run_once(&t.db, &Forges).await.expect("first"),
        Progress::Handled { seq: 1, .. }
    ));
    for attempt in 1..=4 {
        let progress = run_once(&t.db, &Forges)
            .await
            .expect("counted, not an error");
        let Progress::Failed(incident) = progress else {
            panic!("attempt {attempt}: {progress:?}");
        };
        assert_reason(&incident.error, ErrorCode::AuditMissing, "derived_progress");
        assert_eq!(incident.seq, Some(2));
        assert_eq!(failures(&t, "forger").await, i64::from(attempt));
    }
    let progress = run_once(&t.db, &Forges).await.expect("stuck");
    let Progress::Stuck(incident) = progress else {
        panic!("the fifth refusal stops it: {progress:?}");
    };
    assert!(incident
        .error
        .to_string()
        .contains("last_error_code=FG-DB-0013"));
    assert_eq!(cursor(&t, "forger").await, 1);
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.events WHERE caused_by_seq IS NOT NULL"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// The recount after a refused COMMIT claims the row while the server may
/// still be releasing the refused transaction's lock (the client reads the
/// COMMIT error first): it waits for a lock released shortly, where a
/// step's claim reports Busy.
#[tokio::test]
async fn the_recount_claim_waits_for_a_lock_released_shortly() {
    use super::cursor::{claim, Wait};
    let t = FreshDb::migrated().await;
    registered(&t, &Forges).await;
    let mut holder = t.superuser().await;
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("SELECT 1 FROM forge.event_subscribers WHERE name = 'forger' FOR UPDATE")
        .execute(&mut holder)
        .await
        .expect("hold the row");
    let mut step_tx = t.db.write_pool().begin().await.expect("begin");
    assert!(
        claim(&mut step_tx, &Forges, Wait::No)
            .await
            .expect("claim")
            .is_none(),
        "a step skips a held row"
    );
    step_tx.rollback().await.expect("rollback");
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sqlx::query("ROLLBACK")
            .execute(&mut holder)
            .await
            .expect("release");
    });
    // One pooled connection throughout, to see that the wait is
    // transaction-local and never leaks to the connection's next user.
    let mut conn = t.db.write_pool().acquire().await.expect("acquire");
    // A session value unlike the pool's (5 s, the same as the wait) so a
    // leak would show.
    sqlx::query("SET lock_timeout = '1234ms'")
        .execute(&mut *conn)
        .await
        .expect("session lock_timeout");
    let pooled = lock_timeout(&mut conn).await;
    assert_eq!(pooled, "1234ms");
    let mut recount_tx = sqlx::Connection::begin(&mut *conn).await.expect("begin");
    let claimed = claim(&mut recount_tx, &Forges, Wait::Bounded)
        .await
        .expect("claim");
    assert!(
        matches!(claimed, Some(Some(c)) if c.last_seq == 0),
        "{claimed:?}"
    );
    assert_eq!(lock_timeout(&mut recount_tx).await, "5s");
    recount_tx.rollback().await.expect("rollback");
    release.await.expect("released");
    assert_eq!(lock_timeout(&mut conn).await, pooled);
    // The pool must not keep the session value set above.
    conn.close_on_drop();
    drop(conn);
    t.drop_now().await;
}

/// The wait is bounded: a row held past it is reported as held (`None`;
/// the recount then returns the refusal with `counted=false`,
/// `not_counted=row_held`), never waited on for ever or taken as a pass.
#[tokio::test]
async fn the_recount_claim_gives_up_on_a_row_held_past_the_wait() {
    use super::cursor::{claim, Wait};
    let t = FreshDb::migrated().await;
    registered(&t, &Forges).await;
    let mut holder = t.superuser().await;
    sqlx::query("BEGIN")
        .execute(&mut holder)
        .await
        .expect("begin");
    sqlx::query("SELECT 1 FROM forge.event_subscribers WHERE name = 'forger' FOR UPDATE")
        .execute(&mut holder)
        .await
        .expect("hold the row");
    let started = std::time::Instant::now();
    let mut recount_tx = t.db.write_pool().begin().await.expect("begin");
    let claimed = claim(&mut recount_tx, &Forges, Wait::Bounded)
        .await
        .expect("a held row is not an error of the claim");
    assert!(claimed.is_none(), "{claimed:?}");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(4500) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
    recount_tx.rollback().await.expect("rollback");
    sqlx::query("ROLLBACK")
        .execute(&mut holder)
        .await
        .expect("release");
    t.drop_now().await;
}

async fn lock_timeout(conn: &mut sqlx::PgConnection) -> String {
    sqlx::query_scalar("SELECT current_setting('lock_timeout')")
        .fetch_one(conn)
        .await
        .expect("lock_timeout")
}

/// Never returns.
struct Hangs;

impl Subscriber for Hangs {
    fn name(&self) -> &'static str {
        "hangs"
    }
    fn ordering(&self) -> Ordering {
        Ordering::Ordered
    }
    fn handle<'a>(&'a self, _e: &'a SourceEvent, _tx: &'a mut DerivedTx<'_>) -> HandleFuture<'a> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_hung_handler_times_out_and_counts_as_a_failure() {
    let t = FreshDb::migrated().await;
    registered(&t, &Hangs).await;
    published(&t, 1).await;
    let short = Timeouts {
        handler: Duration::from_millis(200),
        ..Timeouts::DEFAULT
    };
    let progress =
        tokio::time::timeout(Duration::from_secs(20), run_once_with(&t.db, &Hangs, short))
            .await
            .expect("bounded by the handler timeout")
            .expect("counted, not an error");
    let Progress::Failed(incident) = progress else {
        panic!("a failure: {progress:?}");
    };
    assert_reason(&incident.error, ErrorCode::Internal, "handler_timeout");
    assert!(incident.error.to_string().contains("timeout_ms=200"));
    assert_eq!(
        (failures(&t, "hangs").await, cursor(&t, "hangs").await),
        (1, 0)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unregistered_subscriber_and_a_missing_event_are_errors_with_their_reason() {
    let t = FreshDb::migrated().await;
    let s = Scripted::new("ghost", Ordering::Ordered);
    let err = run_once(&t.db, s.as_ref())
        .await
        .expect_err("not registered");
    assert_reason(&err, ErrorCode::Internal, "subscriber_not_registered");
    assert!(err.to_string().contains("subscriber=ghost"), "{err}");
    // high_seq says 3 events were numbered, but only 2 exist (the gap a
    // retention overrun leaves, C7).
    registered(&t, s.as_ref()).await;
    published(&t, 2).await;
    t.exec_superuser(
        "ALTER TABLE forge.event_feed_state DISABLE TRIGGER USER; \
         UPDATE forge.event_feed_state SET high_seq = 3; \
         ALTER TABLE forge.event_feed_state ENABLE TRIGGER USER; \
         UPDATE forge.event_subscribers SET last_seq = 2 WHERE name = 'ghost'",
    )
    .await;
    let err = run_once(&t.db, s.as_ref()).await.expect_err("missing");
    assert_reason(&err, ErrorCode::Internal, "event_missing");
    assert!(err.to_string().contains("seq=3"), "{err}");
    assert_eq!(cursor(&t, "ghost").await, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn doctor_reports_behind_from_the_real_sequenced_at_and_ignores_disabled_ones() {
    let t = FreshDb::migrated().await;
    let active = Scripted::new("slow", Ordering::Independent);
    let off = Scripted::new("switched-off", Ordering::Ordered);
    registered(&t, active.as_ref()).await;
    registered(&t, off.as_ref()).await;
    published(&t, 2).await;
    // Numbered just now: pending, but not behind yet.
    let fresh = states(&t.db).await.expect("states").expect("migrated");
    check_lag(&fresh).expect("not behind within a minute");
    // Disabled and two minutes old: still not reported.
    t.exec_superuser(
        "ALTER TABLE forge.events DISABLE TRIGGER events_append_only; \
         UPDATE forge.events SET sequenced_at = now() - interval '2 minutes'; \
         ALTER TABLE forge.events ENABLE TRIGGER events_append_only; \
         ALTER TABLE forge.event_subscribers DISABLE TRIGGER USER; \
         UPDATE forge.event_subscribers SET enabled = false; \
         ALTER TABLE forge.event_subscribers ENABLE TRIGGER USER",
    )
    .await;
    let disabled = states(&t.db).await.expect("states").expect("migrated");
    check_lag(&disabled).expect("disabled subscribers are not behind");
    t.exec_superuser(
        "ALTER TABLE forge.event_subscribers DISABLE TRIGGER USER; \
         UPDATE forge.event_subscribers SET enabled = true WHERE name = 'slow'; \
         ALTER TABLE forge.event_subscribers ENABLE TRIGGER USER",
    )
    .await;
    let found = states(&t.db).await.expect("states").expect("migrated");
    let slow = found.iter().find(|s| s.name == "slow").expect("row");
    assert!(
        slow.oldest_pending_age_ms.is_some_and(|a| a >= 120_000),
        "{slow:?}"
    );
    let err = check_lag(&found).expect_err("behind");
    assert_reason(&err, ErrorCode::SubscriberLag, "behind");
    let text = err.to_string();
    for part in ["subscriber=slow", "seq=1", "lag_events=2"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    t.drop_now().await;
}

#[test]
fn a_cursor_that_moved_under_the_lock_is_an_error() {
    let err = super::cursor::progress_moved(0, "subscribers.advance").expect_err("moved");
    assert_reason(&err, ErrorCode::Internal, "cursor_moved");
    assert!(err.to_string().contains("rows=0"), "{err}");
    super::cursor::progress_moved(1, "subscribers.advance").expect("exactly one row");
}
