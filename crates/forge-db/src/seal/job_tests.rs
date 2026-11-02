//! The seal job's policy and its refusals: `seal_due` seals a minute after
//! the last seal or when more than 1000 rows wait; a missing feed state or
//! an `audit_high_seq` below the last seal is an error, never "up to date";
//! a row v1 cannot express stops sealing; a cancelled call leaves nothing
//! and the next one seals the same range.

use std::time::Duration;

use forge_errors::ErrorCode;

use super::golden::golden_keys;
use super::tests::{insert_audit, number_all, seal_all};
use super::*;
use crate::test_support::*;

async fn seal_count(t: &FreshDb) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.audit_seal")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

fn sealed(outcome: SealOutcome) -> Sealed {
    match outcome {
        SealOutcome::Sealed(s) => s,
        other @ (SealOutcome::Busy | SealOutcome::UpToDate | SealOutcome::NotDue) => {
            panic!("expected a seal, got {other:?}")
        }
    }
}

#[tokio::test]
async fn seal_due_seals_after_the_interval_or_when_more_than_eager_rows_wait() {
    let t = FreshDb::migrated().await;
    let keys = golden_keys();
    let due = |policy| seal_due(&t.db, &keys, policy);
    // The first seal is always due.
    insert_audit(&t, 3).await;
    number_all(&t).await;
    sealed(due(SealPolicy::WORKER).await.expect("seal"));
    assert_eq!(
        due(SealPolicy::WORKER).await.expect("seal"),
        SealOutcome::UpToDate
    );
    // A few rows a moment after the last seal: not yet.
    insert_audit(&t, 2).await;
    number_all(&t).await;
    assert_eq!(
        due(SealPolicy::WORKER).await.expect("seal"),
        SealOutcome::NotDue
    );
    // Once the interval has passed (here: zero), they are.
    let now = SealPolicy {
        interval: Duration::ZERO,
        ..SealPolicy::WORKER
    };
    assert_eq!(sealed(due(now).await.expect("seal")).range.count(), 2);
    // More than eager_rows waiting: at once.
    insert_audit(&t, SealPolicy::WORKER.eager_rows + 1).await;
    number_all(&t).await;
    let eager = sealed(due(SealPolicy::WORKER).await.expect("seal"));
    assert_eq!(eager.range.count(), SealPolicy::WORKER.eager_rows + 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_missing_feed_state_after_numbering_is_audit_seal_mismatch_chain() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 2).await;
    number_all(&t).await;
    seal_all(&t).await;
    t.exec_superuser(
        "ALTER TABLE forge.event_feed_state DISABLE TRIGGER event_feed_state_append_only; \
         DELETE FROM forge.event_feed_state; \
         ALTER TABLE forge.event_feed_state ENABLE ALWAYS TRIGGER event_feed_state_append_only;",
    )
    .await;
    let err = seal_pending(&t.db, &golden_keys())
        .await
        .expect_err("no feed state");
    assert_reason(&err, ErrorCode::AuditSealMismatch, "chain");
    let text = err.to_string();
    for part in ["detail=feed_state_missing", "audit_high_seq=none"] {
        assert!(text.contains(part), "{text}");
    }
    assert_eq!(seal_count(&t).await, 1, "nothing more sealed");
    t.drop_now().await;
}

#[tokio::test]
async fn audit_high_below_the_last_seal_is_audit_seal_mismatch_chain() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 2).await;
    number_all(&t).await;
    let last = seal_all(&t).await[0].range.last();
    t.exec_superuser(&format!(
        "ALTER TABLE forge.event_feed_state DISABLE TRIGGER event_feed_state_append_only; \
         UPDATE forge.event_feed_state SET audit_high_seq = {}; \
         ALTER TABLE forge.event_feed_state ENABLE ALWAYS TRIGGER event_feed_state_append_only;",
        last - 1
    ))
    .await;
    let err = seal_pending(&t.db, &golden_keys())
        .await
        .expect_err("moved back");
    assert_reason(&err, ErrorCode::AuditSealMismatch, "chain");
    let text = err.to_string();
    for part in [
        "detail=audit_high_below_seal".to_owned(),
        format!("last_sealed_seq={last}"),
        format!("audit_high_seq={}", last - 1),
    ] {
        assert!(text.contains(&part), "{text}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_row_v1_cannot_express_is_internal_and_nothing_is_sealed() {
    let t = FreshDb::migrated().await;
    // Only raw SQL can store details this deep (the application never does).
    let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id, \
         details) VALUES ('system', 'repository.viewed', 'succeeded', 'repository', 'r-deep', \
         '{{\"x\": {deep}}}')"
    ))
    .await;
    number_all(&t).await;
    let err = seal_pending(&t.db, &golden_keys())
        .await
        .expect_err("inexpressible");
    assert_eq!(err.code(), ErrorCode::Internal);
    let text = err.to_string();
    for part in ["operation=seal.v1", "reason=details_unreadable", "seq="] {
        assert!(text.contains(part), "{text}");
    }
    assert_eq!(seal_count(&t).await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_cancelled_seal_leaves_no_seal_and_the_next_call_seals_the_same_range() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 3).await;
    number_all(&t).await;
    // The insert waits for this lock; the caller gives up first (a dropped
    // future, like an operation timeout or a worker shutdown).
    let mut holder = t.superuser().await;
    sqlx::raw_sql("BEGIN; LOCK TABLE forge.audit_seal IN EXCLUSIVE MODE;")
        .execute(&mut holder)
        .await
        .expect("lock");
    let keys = golden_keys();
    match tokio::time::timeout(Duration::from_secs(2), seal_pending(&t.db, &keys)).await {
        // Cancelled mid-transaction, or the server's lock_timeout ended it.
        Err(_) | Ok(Err(_)) => {}
        Ok(Ok(outcome)) => panic!("sealed through the lock: {outcome:?}"),
    }
    sqlx::raw_sql("COMMIT")
        .execute(&mut holder)
        .await
        .expect("unlock");
    assert_eq!(seal_count(&t).await, 0, "nothing committed");
    let again = sealed(seal_pending(&t.db, &keys).await.expect("seal"));
    assert_eq!(again.range.first(), 1, "the same range, from the start");
    assert!(!again.more);
    t.drop_now().await;
}

/// A call that seals nothing must give up the seal lock before it returns
/// (`finish` rolls back explicitly). sqlx sends the ROLLBACK of a dropped
/// transaction only when a spawned task returns the connection to the pool;
/// until then the transaction-scoped advisory lock stays held, and the next
/// caller on another pooled connection saw `Busy` — the seal tests that
/// failed only under load. Without load the spawned task usually wins the
/// race, so this test guards the fix rather than reproducing the failure on
/// every run.
#[tokio::test]
async fn a_call_that_seals_nothing_does_not_leave_the_lock_held() {
    let t = FreshDb::migrated().await;
    let keys = golden_keys();
    // Keep several idle connections in the write pool, so the next call
    // takes another connection at once.
    {
        let a = t.db.write_pool().acquire().await.expect("a");
        let b = t.db.write_pool().acquire().await.expect("b");
        let c = t.db.write_pool().acquire().await.expect("c");
        drop((a, b, c));
    }
    tokio::task::yield_now().await;
    for round in 0..20 {
        // Nothing numbered yet: up to date (the lock was taken and released).
        assert_eq!(
            seal_pending(&t.db, &keys).await.expect("seal"),
            SealOutcome::UpToDate,
            "round {round}"
        );
    }
    insert_audit(&t, 1).await;
    number_all(&t).await;
    sealed(
        seal_due(&t.db, &keys, SealPolicy::WORKER)
            .await
            .expect("seal"),
    );
    insert_audit(&t, 1).await;
    number_all(&t).await;
    for round in 0..20 {
        assert_eq!(
            seal_due(&t.db, &keys, SealPolicy::WORKER)
                .await
                .expect("seal"),
            SealOutcome::NotDue,
            "round {round}"
        );
    }
    t.drop_now().await;
}
