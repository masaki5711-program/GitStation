//! Waking the merge queue's turns (plan 18 S3, ADR-0037 (a)).
//!
//! **Senders.** A writer that makes a queue actionable does two things in
//! its own transaction: it makes the queue due (`next_run_at = now()`, the
//! fact a turn reads) and it sends `forge_merge_queue` with the queue's id
//! (the hint that ends an idle turn's wait). The notification is delivered
//! at the commit and never on a rollback; a lost one costs the safety-net
//! period, never a landing — the queue is due either way.
//!
//! - an entry enqueued (`merges::enqueue_mr_decided`), dequeued
//!   (`dequeue_mr_decided`), a queue resumed (`set_paused_decided`):
//!   [`notify_queue_on`] next to the statement that makes the queue due;
//! - a train's pipeline that reached a terminal state — the runner's last
//!   job report, a reaped job, a cancel, a pipeline born terminal (an
//!   invalid definition): [`train_pipeline_ended_on`];
//! - the job that creates the train's pipeline ended without one (the
//!   train has no CI definition after all, or the job is dead):
//!   `jobs::queue`'s `complete` and `fail`, in their one statement.
//!
//! **The idle wait.** [`next_due`] is when the next queue wants a look by
//! itself: the deadline a waiting queue was released with, or the end of a
//! lease nobody released (a turn that died). An idle turn waits for a
//! notification, that time or its safety-net period — whichever is first.

use std::time::Duration;

use forge_errors::ForgeError;
use sqlx::PgConnection;

use crate::wake::{notify_on, Channel};
use crate::{query_error, Db};

/// Send `forge_merge_queue` with `queue`'s id on `c` (module docs): the
/// caller made the queue due in the same transaction.
pub(crate) async fn notify_queue_on(c: &mut PgConnection, queue: i64) -> Result<(), ForgeError> {
    notify_on(c, Channel::MergeQueue, &queue.to_string()).await
}

/// `pipeline` may have reached a terminal state in this transaction: when
/// it is the pipeline of a train a batch is testing, that batch's queue is
/// due now and its turns are told. Any other pipeline (not a train's, not
/// terminal, a batch no longer testing) writes and sends nothing.
pub(crate) async fn train_pipeline_ended_on(
    c: &mut PgConnection,
    pipeline: i64,
) -> Result<(), ForgeError> {
    const QUERY: &str = "merge_queue.train_pipeline_ended";
    let queues: Vec<i64> = sqlx::query_scalar(
        "UPDATE forge.merge_queues q SET next_run_at = now() \
          WHERE q.id IN (SELECT b.queue_id FROM forge.ci_pipelines p \
                           JOIN forge.merge_queue_batches b \
                             ON b.project_id = p.project_id AND b.train_sha = p.sha \
                          WHERE p.id = $1 AND p.source = 'merge_queue' \
                            AND p.status NOT IN ('pending', 'running') \
                            AND b.state = 'testing') \
          RETURNING q.id",
    )
    .bind(pipeline)
    .fetch_all(&mut *c)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    for queue in queues {
        notify_queue_on(c, queue).await?;
    }
    Ok(())
}

/// How long until a queue with work wants a look by itself (module docs):
/// zero when one is due now, `None` when no queue has work. A queue whose
/// lease is held counts from the end of that lease (the turn that holds it
/// looks again by itself when it releases). Read on the write pool: the
/// answer is about rows written a moment ago.
pub async fn next_due(db: &Db) -> Result<Option<Duration>, ForgeError> {
    const QUERY: &str = "merge_queue.next_due";
    let due_in_ms: Option<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT (EXTRACT(epoch FROM min(GREATEST(q.next_run_at, \
                           COALESCE(q.lease_until, q.next_run_at))) - now()) * 1000)::bigint \
                   FROM forge.merge_queues q \
                  WHERE EXISTS (SELECT 1 FROM forge.merge_queue_entries e \
                                 WHERE e.queue_id = q.id AND e.state IN ('queued', 'testing')) \
                     OR EXISTS (SELECT 1 FROM forge.merge_queue_batches b \
                                 WHERE b.queue_id = q.id AND b.state IN ('testing', 'landing'))",
            )
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(due_in_ms.map(|ms| Duration::from_millis(u64::try_from(ms).unwrap_or(0))))
}
