//! Waking subscribers: the first wait connects, a NOTIFY from the sequencer
//! wakes the listener well before the poll, a quiet wait ends at the poll
//! interval, and a dispatcher round then handles what waits.

use std::time::{Duration, Instant};

use super::tests::{cursor, Scripted};
use super::wake::{Wake, Woken};
use super::*;
use crate::sequencer::{self, AuditSequencing, BatchSize};
use crate::test_support::*;

#[tokio::test]
async fn a_notify_from_the_sequencer_wakes_the_listener_before_the_poll() {
    let t = FreshDb::migrated().await;
    let mut wake = Wake::new();
    assert_eq!(
        wake.wait(&t.db, Duration::from_secs(1))
            .await
            .expect("connect"),
        Woken::Connected
    );
    // Quiet: the wait ends at the poll interval (with a live link).
    let started = Instant::now();
    assert_eq!(
        wake.wait(&t.db, Duration::from_millis(300))
            .await
            .expect("poll"),
        Woken::Polled
    );
    assert!(started.elapsed() >= Duration::from_millis(300));
    // The sequencer notifies when it numbers events.
    sequencer::tests::insert_events(&t, 2).await;
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
    // A notification, not the poll: the wait is far longer than any test.
    assert_eq!(
        wake.wait(&t.db, Duration::from_secs(30))
            .await
            .expect("notified"),
        Woken::Notified
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_dispatcher_round_handles_every_subscriber_that_is_behind() {
    let t = FreshDb::migrated().await;
    let a = Scripted::new("first", Ordering::Ordered);
    let b = Scripted::new("second", Ordering::Independent);
    let registry = Registry::new()
        .with(a.clone())
        .expect("a")
        .with(b.clone())
        .expect("b");
    let mut dispatcher = Dispatcher::start(&t.db, &registry).await.expect("start");
    assert_eq!(
        dispatcher.wait_for(&t.db).await.expect("connect"),
        Woken::Connected
    );
    let idle = dispatcher.round(&t.db).await.expect("idle round");
    assert_eq!(
        (idle.handled, idle.more, idle.incidents.len()),
        (0, false, 0)
    );
    super::tests::published(&t, 3).await;
    assert_eq!(
        dispatcher.wait_for(&t.db).await.expect("wake"),
        Woken::Notified
    );
    let round = dispatcher.round(&t.db).await.expect("round");
    assert_eq!((round.handled, round.more), (6, false));
    assert!(round.incidents.is_empty(), "{:?}", round.incidents);
    assert_eq!(
        (cursor(&t, "first").await, cursor(&t, "second").await),
        (3, 3)
    );
    assert_eq!(a.seen(), vec![1, 2, 3]);
    assert_eq!(b.seen(), vec![1, 2, 3]);
    // A failing subscriber reports an incident and waits; the other goes on.
    a.failing.store(true, std::sync::atomic::Ordering::SeqCst);
    super::tests::published(&t, 1).await;
    let round = dispatcher.round(&t.db).await.expect("round");
    assert_eq!(round.handled, 1);
    assert_eq!(round.incidents.len(), 1);
    assert_eq!(round.incidents[0].seq, Some(4));
    assert!(
        round.incidents[0]
            .error
            .to_string()
            .contains("subscriber=first"),
        "{:?}",
        round.incidents
    );
    // (The per-subscriber retry delay is time-based: unit test in dispatch.)
    // Registration rejects duplicates and bad names.
    let dup = Registry::new().with(a.clone()).expect("a").with(a);
    assert!(dup.is_err());
    t.drop_now().await;
}

#[tokio::test]
async fn a_round_past_its_budget_stops_and_asks_for_more() {
    let t = FreshDb::migrated().await;
    let a = Scripted::new("budgeted", Ordering::Ordered);
    let registry = Registry::new().with(a.clone()).expect("a");
    let mut dispatcher = Dispatcher::start(&t.db, &registry).await.expect("start");
    super::tests::published(&t, 3).await;
    // No budget: the first step still runs, then the round ends.
    let round = dispatcher
        .round_within(&t.db, Duration::ZERO)
        .await
        .expect("round");
    assert_eq!((round.handled, round.more), (1, true));
    assert_eq!(cursor(&t, "budgeted").await, 1);
    let round = dispatcher.round(&t.db).await.expect("round");
    assert_eq!((round.handled, round.more), (2, false));
    assert_eq!(a.seen(), vec![1, 2, 3]);
    t.drop_now().await;
}
