//! The sequencer's edges: a held lock times out without a gap, the
//! function's own count check refuses a shifted batch (XF001), a lower id
//! committed later gets the later seq, and an idle run writes nothing.

use forge_errors::ErrorCode;

use super::tests::insert_events;
use super::*;
use crate::test_support::*;

async fn run(db: &Db) -> Result<Assigned, ForgeError> {
    run_once(db, BatchSize::MAX, AuditSequencing::Skip).await
}

fn range(first: i64, last: i64) -> Option<SeqRange> {
    Some(SeqRange::new(first, last).expect("range"))
}

#[test]
fn a_half_empty_or_reversed_range_is_an_internal_error() {
    assert_eq!(range_of(None, None, "events").expect("none"), None);
    assert_eq!(
        range_of(Some(2), Some(4), "events").expect("some"),
        range(2, 4)
    );
    for (first, last) in [
        (Some(1), None),
        (None, Some(1)),
        (Some(5), Some(4)),
        (Some(0), Some(1)),
    ] {
        let err = range_of(first, last, "audit").expect_err("inconsistent");
        assert_eq!(err.code(), ErrorCode::Internal);
        let text = err.to_string();
        assert!(text.contains("reason=inconsistent_range"), "{text}");
        assert!(text.contains("rows=audit"), "{text}");
    }
}

#[tokio::test]
async fn a_held_sequencer_lock_times_out_and_the_next_run_continues_without_a_gap() {
    let t = FreshDb::migrated().await;
    let cfg = t.app_config(&[("FORGE_DATABASE_LOCK_TIMEOUT_MS", "200")]);
    let db = Db::connect(&cfg.db).await.expect("connect");
    insert_events(&t, 2).await;
    assert_eq!(run(&db).await.expect("run").events, range(1, 2));
    insert_events(&t, 3).await;
    let mut holder = t.superuser().await;
    let mut tx = holder.begin().await.expect("begin");
    sqlx::query("SELECT pg_advisory_xact_lock(1718579815, 1)")
        .execute(&mut *tx)
        .await
        .expect("hold the sequencer lock");
    let err = run(&db).await.expect_err("lock held");
    assert_reason(&err, ErrorCode::DbQueryFailed, "lock_timeout");
    assert!(
        err.to_string().contains("query=sequencer.assign_seq"),
        "{err}"
    );
    tx.commit().await.expect("release");
    assert_eq!(run(&db).await.expect("run").events, range(3, 5));
    db.close().await;
    t.drop_now().await;
}

#[tokio::test]
async fn the_count_check_refuses_a_batch_whose_numbers_were_shifted() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 2).await;
    // A trigger that runs after the guard and moves every new seq: the
    // function sees numbers that do not start at high_seq + 1.
    t.exec_superuser(
        "CREATE FUNCTION forge.test_shift_seq() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN NEW.seq := NEW.seq + 5; RETURN NEW; END $$; \
         CREATE TRIGGER zz_shift BEFORE UPDATE ON forge.events \
         FOR EACH ROW EXECUTE FUNCTION forge.test_shift_seq()",
    )
    .await;
    let err = run(&t.db).await.expect_err("shifted");
    assert_eq!(err.code(), ErrorCode::AppendOnlyViolation, "{err}");
    let text = err.to_string();
    assert!(text.contains("query=sequencer.assign_seq"), "{text}");
    assert!(text.contains("sqlstate=XF001"), "{text}");
    // Rolled back: nothing numbered, high_seq unchanged.
    let (numbered, high): (i64, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM forge.events WHERE seq IS NOT NULL), \
                (SELECT high_seq FROM forge.event_feed_state)",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("state");
    assert_eq!((numbered, high), (0, None));
    t.drop_now().await;
}

#[tokio::test]
async fn a_lower_id_committed_later_gets_the_later_seq() {
    // Gap-free numbering in commit-visibility order is the contract; id
    // order holds within a batch only.
    let t = FreshDb::migrated().await;
    let mut slow = t.superuser().await;
    let mut tx = slow.begin().await.expect("begin");
    let slow_id: i64 = {
        sqlx::query(
            "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
             VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-slow')",
        )
        .execute(&mut *tx)
        .await
        .expect("audit row");
        sqlx::query_scalar(
            "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
             aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
             'repository.created', 1, 'repository', 1, 'system', 'r-slow', '{}') RETURNING id",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("event")
    };
    insert_events(&t, 1).await;
    assert_eq!(run(&t.db).await.expect("run").events, range(1, 1));
    tx.commit().await.expect("commit the slow writer");
    assert_eq!(run(&t.db).await.expect("run").events, range(2, 2));
    let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT id, seq FROM forge.events ORDER BY id")
        .fetch_all(&mut t.superuser().await)
        .await
        .expect("rows");
    assert_eq!(rows.first(), Some(&(slow_id, 2)), "{rows:?}");
    assert!(
        rows.get(1)
            .is_some_and(|(id, seq)| *id > slow_id && *seq == 1),
        "{rows:?}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_idle_run_with_a_fresh_heartbeat_writes_nothing() {
    let t = FreshDb::migrated().await;
    run(&t.db).await.expect("first run creates the feed state");
    let state = || async {
        sqlx::query_as::<_, (String, String)>(
            "SELECT xmin::text, sequencer_heartbeat::text FROM forge.event_feed_state",
        )
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("feed state")
    };
    let before = state().await;
    assert!(run(&t.db).await.expect("idle run").is_empty());
    assert_eq!(state().await, before, "an idle run must not touch the row");
    insert_events(&t, 1).await;
    run(&t.db).await.expect("run");
    let after = state().await;
    assert_ne!(after.0, before.0);
    assert_ne!(after.1, before.1, "a run that numbered beats the heartbeat");
    t.drop_now().await;
}
