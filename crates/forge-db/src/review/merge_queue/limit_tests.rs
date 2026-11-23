//! Migration `merge_queue_batch_limit_64` (plan 18 S7): a queue's bound and
//! a batch's size accept 1..=64 and refuse 0 and 65, by the constraint's
//! name; a new queue starts at the schema's bound.

use sqlx::Connection as _;

use crate::test_support::*;
use forge_core::merge_queue::MAX_BATCH;

/// `probe` in a transaction that is rolled back (foreign keys off: only
/// the CHECK is asked): the SQLSTATE and constraint of its failure.
async fn refusal(su: &mut sqlx::PgConnection, probe: &str) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("replica");
    let result = sqlx::raw_sql(probe).execute(&mut *tx).await;
    tx.rollback().await.expect("rollback");
    let e = result.err()?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

fn queue(limit: i64) -> String {
    format!(
        "INSERT INTO forge.merge_queues (project_id, target_branch, batch_limit) \
         VALUES (1, 'refs/heads/main', {limit})"
    )
}

fn batch(size: i64) -> String {
    format!(
        "INSERT INTO forge.merge_queue_batches (queue_id, project_id, state, base_sha, \
         train_sha, size, request_id, deadline_at) VALUES (1, 1, 'testing', '{a}', '{a}', \
         {size}, 'req-limit', now())",
        a = "a".repeat(40)
    )
}

#[tokio::test]
async fn batch_limit_accepts_64_rejects_65() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    assert_eq!(MAX_BATCH, 64, "the schema's bound and the code's are one");
    for ok in [1, 4, 32, 64] {
        assert_eq!(refusal(&mut su, &queue(ok)).await, None, "batch_limit {ok}");
        assert_eq!(refusal(&mut su, &batch(ok)).await, None, "size {ok}");
    }
    for bad in [0, 65] {
        assert_eq!(
            refusal(&mut su, &queue(bad)).await,
            Some(("23514".to_owned(), "merge_queues_batch_limit".to_owned())),
            "batch_limit {bad}"
        );
        assert_eq!(
            refusal(&mut su, &batch(bad)).await,
            Some(("23514".to_owned(), "merge_queue_batches_size".to_owned())),
            "size {bad}"
        );
    }
    // A new queue starts at the full mark, 4 ("not lowered": its kind
    // bounds a batch), which a binary before S7 still reads.
    let defaults = t
        .json_rows(
            "SELECT column_default AS d FROM information_schema.columns \
             WHERE table_schema = 'forge' AND table_name = 'merge_queues' \
             AND column_name = 'batch_limit'",
        )
        .await;
    assert_eq!(defaults[0]["d"].as_str(), Some("4"));
}
