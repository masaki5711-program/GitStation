//! Migration `merge_queue_speculation` (plan 18 S9): one live batch per
//! (queue, depth) instead of one per queue, depth 1..=8, a batch behind
//! the front names its parent, and the old one-live index is gone.

use sqlx::Connection as _;

use crate::test_support::*;
use forge_core::merge_queue::MAX_SPECULATION;

/// `probes` in a transaction that is rolled back (foreign keys off: only
/// the CHECKs and the unique index are asked): the SQLSTATE and
/// constraint (or index) of the first failure.
async fn refusal(su: &mut sqlx::PgConnection, probes: &[String]) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .expect("replica");
    let mut failed = None;
    for probe in probes {
        if let Err(e) = sqlx::raw_sql(probe).execute(&mut *tx).await {
            failed = Some(e);
            break;
        }
    }
    tx.rollback().await.expect("rollback");
    let e = failed?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

fn batch(state: &str, depth: i64, parent: Option<i64>) -> String {
    let finished = if matches!(state, "testing" | "landing") {
        "NULL"
    } else {
        "now()"
    };
    let parent = parent.map_or_else(|| "NULL".to_owned(), |p| p.to_string());
    format!(
        "INSERT INTO forge.merge_queue_batches (queue_id, project_id, state, base_sha, \
         train_sha, size, request_id, deadline_at, finished_at, depth, parent_batch_id) \
         VALUES (1, 1, '{state}', '{a}', '{a}', 1, 'req-spec', now(), {finished}, {depth}, \
         {parent})",
        a = "a".repeat(40)
    )
}

#[tokio::test]
async fn speculation_allows_one_live_batch_per_depth() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    assert_eq!(
        MAX_SPECULATION, 8,
        "the schema's bound and the code's are one"
    );
    // A chain of live batches, one per depth, and finished ones beside it.
    let chain = vec![
        batch("testing", 1, None),
        batch("testing", 2, Some(1)),
        batch("testing", 8, Some(1)),
        batch("landed", 1, None),
        batch("abandoned", 2, Some(1)),
    ];
    assert_eq!(refusal(&mut su, &chain).await, None);
    // Two live batches at one depth: the new index refuses the second
    // (a `landing` front is live too).
    let unique = (
        "23505".to_owned(),
        "merge_queue_batches_live_depth".to_owned(),
    );
    for second in ["testing", "landing"] {
        let twice = vec![batch("testing", 1, None), batch(second, 1, None)];
        assert_eq!(refusal(&mut su, &twice).await, Some(unique.clone()));
    }
    for bad in [0, 9] {
        assert_eq!(
            refusal(&mut su, &[batch("testing", bad, Some(1))]).await,
            Some(("23514".to_owned(), "merge_queue_batches_depth".to_owned())),
            "depth {bad}"
        );
    }
    assert_eq!(
        refusal(&mut su, &[batch("testing", 2, None)]).await,
        Some((
            "23514".to_owned(),
            "merge_queue_batches_speculative".to_owned()
        ))
    );
    let old = t
        .json_rows(
            "SELECT count(*) AS n FROM pg_indexes WHERE schemaname = 'forge' \
             AND indexname = 'merge_queue_batches_one_live'",
        )
        .await;
    assert_eq!(old[0]["n"].as_i64(), Some(0), "the one-live index is gone");
    let defaults = t
        .json_rows(
            "SELECT column_default AS d FROM information_schema.columns \
             WHERE table_schema = 'forge' AND table_name = 'merge_queue_batches' \
             AND column_name = 'depth'",
        )
        .await;
    assert_eq!(defaults[0]["d"].as_str(), Some("1"));
}
