//! The idle early return of forge.assign_seq (migration 20261027110100):
//! with nothing to number and a heartbeat younger than 5 s the function
//! writes nothing; otherwise it beats the heartbeat or numbers. The
//! heartbeat is set explicitly by the superuser (no reliance on how long a
//! run takes), and "no write" is checked on the row version (xmin).

use super::tests::insert_events;
use super::*;
use crate::test_support::*;

/// A migrated database whose feed-state row exists (one run created it).
async fn with_feed_state() -> FreshDb {
    let t = FreshDb::migrated().await;
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("the first run creates the feed state");
    t
}

/// The heartbeat `age` old, written now by the superuser.
async fn set_heartbeat(t: &FreshDb, age: &str) {
    t.exec_superuser(&format!(
        "UPDATE forge.event_feed_state SET sequencer_heartbeat = now() - interval '{age}'"
    ))
    .await;
}

async fn set_audit_sequencing(t: &FreshDb, on: bool) {
    t.exec_superuser(&format!(
        "UPDATE forge.event_feed_state SET audit_sequencing_enabled = {on}"
    ))
    .await;
}

/// The feed-state row version and heartbeat.
async fn state(t: &FreshDb) -> (String, String) {
    sqlx::query_as("SELECT xmin::text, sequencer_heartbeat::text FROM forge.event_feed_state")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("feed state")
}

async fn unsequenced_audit(t: &FreshDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.audit_log WHERE seq IS NULL")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

async fn run(t: &FreshDb, audit: AuditSequencing) -> Assigned {
    run_once(&t.db, BatchSize::MAX, audit)
        .await
        .expect("run_once")
}

#[tokio::test]
async fn a_two_second_old_heartbeat_with_nothing_waiting_is_left_alone() {
    let t = with_feed_state().await;
    set_heartbeat(&t, "2 seconds").await;
    let before = state(&t).await;
    assert!(run(&t, AuditSequencing::Skip).await.is_empty());
    assert_eq!(state(&t).await, before, "an idle run must not write");
    t.drop_now().await;
}

#[tokio::test]
async fn a_ten_second_old_heartbeat_is_beaten_even_with_nothing_waiting() {
    let t = with_feed_state().await;
    set_heartbeat(&t, "10 seconds").await;
    let before = state(&t).await;
    assert!(run(&t, AuditSequencing::Skip).await.is_empty());
    let after = state(&t).await;
    assert_ne!(after.0, before.0, "the row was written");
    let age_ms: i64 = sqlx::query_scalar(
        "SELECT (EXTRACT(epoch FROM now() - sequencer_heartbeat) * 1000)::bigint \
         FROM forge.event_feed_state",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("age");
    assert!(age_ms < 10_000, "the heartbeat was beaten: {age_ms} ms");
    t.drop_now().await;
}

#[tokio::test]
async fn a_waiting_event_is_numbered_despite_a_fresh_heartbeat() {
    let t = with_feed_state().await;
    insert_events(&t, 1).await;
    set_heartbeat(&t, "0 seconds").await;
    let before = state(&t).await;
    let assigned = run(&t, AuditSequencing::Skip).await;
    assert_eq!(assigned.events.map(SeqRange::count), Some(1));
    assert_ne!(state(&t).await.0, before.0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_waiting_audit_row_is_numbered_despite_a_fresh_heartbeat_when_audit_sequencing_is_on() {
    let t = with_feed_state().await;
    // The migrations' own db.migrate rows wait for an audit number.
    let waiting = unsequenced_audit(&t).await;
    assert!(waiting > 0);
    set_audit_sequencing(&t, true).await;
    set_heartbeat(&t, "0 seconds").await;
    let assigned = run(&t, AuditSequencing::IfEnabled).await;
    assert_eq!(assigned.events, None);
    assert_eq!(assigned.audit.map(SeqRange::count), Some(waiting));
    assert_eq!(unsequenced_audit(&t).await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_waiting_audit_row_keeps_the_run_idle_while_audit_sequencing_is_off() {
    let t = with_feed_state().await;
    assert!(unsequenced_audit(&t).await > 0);
    set_audit_sequencing(&t, false).await;
    set_heartbeat(&t, "0 seconds").await;
    let before = state(&t).await;
    assert!(run(&t, AuditSequencing::IfEnabled).await.is_empty());
    assert_eq!(state(&t).await, before, "audit sequencing is off: no write");
    // On, but this run did not ask for audit rows (Skip): idle as well.
    set_audit_sequencing(&t, true).await;
    set_heartbeat(&t, "0 seconds").await;
    let before = state(&t).await;
    assert!(run(&t, AuditSequencing::Skip).await.is_empty());
    assert_eq!(state(&t).await, before, "audit rows were not asked for");
    t.drop_now().await;
}
