//! Edges of the queue statements: dedup after a dead or a succeeded job,
//! the claim order, the reap limit, and a claimed row that cannot be read.

use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use super::queue::reap_at_most;
use super::tests::{count_jobs, expire, kind, new_job, queued, state};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_dead_job_frees_its_dedup_key_and_a_succeeded_one_keeps_it() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let job = new_job("test.a", Some("repo:7:seal"), 1, T);
    let first = queued(&t, &job).await;
    let claimed = claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .expect("ready");
    let never = JobFailure::permanent(ForgeError::new(ErrorCode::Internal));
    assert!(matches!(
        fail(&t.db, &claimed, &never).await.expect("fail"),
        Failed::Dead
    ));
    // The runbook's recovery: the same work enqueued again is a new job.
    let second = queued(&t, &job).await;
    assert_ne!(first, second);
    assert_eq!(state(&t, second).await.0, "queued");
    // While it is live it dedups, and once it succeeded too.
    assert_eq!(
        enqueue(&t.db, &job).await.expect("enqueue"),
        Enqueued::Deduped
    );
    let claimed = claim(&t.db, &w, &[kind("test.a")])
        .await
        .expect("claim")
        .expect("ready");
    assert!(matches!(
        complete(&t.db, &claimed).await.expect("complete"),
        Completed::Applied
    ));
    assert_eq!(
        enqueue(&t.db, &job).await.expect("enqueue"),
        Enqueued::Deduped
    );
    assert_eq!(count_jobs(&t).await, 2);
    t.drop_now().await;
}

#[tokio::test]
async fn a_claim_takes_the_oldest_ready_job_first() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let newer = queued(&t, &new_job("test.a", None, 3, T)).await;
    let older = queued(&t, &new_job("test.a", None, 3, T)).await;
    t.exec_superuser(&format!(
        "UPDATE forge.jobs SET run_after = now() - interval '1 minute' WHERE id = {}; \
         UPDATE forge.jobs SET run_after = now() - interval '2 minutes' WHERE id = {}",
        newer.get(),
        older.get()
    ))
    .await;
    let order: Vec<JobId> = [
        claim(&t.db, &w, &[kind("test.a")]).await.expect("claim"),
        claim(&t.db, &w, &[kind("test.a")]).await.expect("claim"),
    ]
    .into_iter()
    .flatten()
    .map(|j| j.lease.job_id)
    .collect();
    assert_eq!(order, vec![older, newer]);
    t.drop_now().await;
}

#[tokio::test]
async fn reap_handles_at_most_its_limit_per_call() {
    let t = FreshDb::migrated().await;
    let w = WorkerId::generate();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = queued(&t, &new_job("test.a", None, 3, T)).await;
        claim(&t.db, &w, &[kind("test.a")])
            .await
            .expect("claim")
            .expect("ready");
        ids.push(id);
    }
    for id in &ids {
        expire(&t, *id).await;
    }
    assert_eq!(reap_at_most(&t.db, 2).await.expect("reap").len(), 2);
    assert_eq!(reap_at_most(&t.db, 2).await.expect("reap").len(), 1);
    assert!(reap(&t.db).await.expect("reap").is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_claimed_row_that_cannot_be_read_is_reported_and_reaped_later() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    // Tamper: the claimed row comes back with a kind that is not valid.
    t.exec_superuser(
        "CREATE FUNCTION forge.t_bad_kind() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN NEW.kind := 'Bad Kind'; RETURN NEW; END $$; \
         CREATE TRIGGER t_bad_kind BEFORE UPDATE ON forge.jobs FOR EACH ROW \
         WHEN (NEW.state = 'running') EXECUTE FUNCTION forge.t_bad_kind()",
    )
    .await;
    let err = claim(&t.db, &WorkerId::generate(), &[kind("test.a")])
        .await
        .expect_err("an invalid row");
    assert_reason(&err, ErrorCode::Internal, "invalid_row");
    assert!(err.to_string().contains("column=kind"), "{err}");
    assert!(
        err.to_string().contains(&format!("job_id={}", id.get())),
        "{err}"
    );
    t.exec_superuser("DROP TRIGGER t_bad_kind ON forge.jobs; DROP FUNCTION forge.t_bad_kind()")
        .await;
    // Not lost: it holds a lease, which expires and is reaped.
    assert_eq!(state(&t, id).await.0, "running");
    expire(&t, id).await;
    let reaped = reap(&t.db).await.expect("reap");
    assert_eq!(reaped.len(), 1);
    assert_eq!(reaped[0].job_id, id);
    assert_eq!(state(&t, id).await.0, "queued");
    t.drop_now().await;
}
