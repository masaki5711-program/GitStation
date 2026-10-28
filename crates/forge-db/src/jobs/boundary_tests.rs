//! The queue's boundaries: a job enqueued inside a change lives and dies
//! with it, and forge_ro (read pool or role) cannot claim.

use std::time::Duration;

use forge_core::RequestId;
use forge_errors::ErrorCode;
use serde_json::json;

use super::queue::claim_on;
use super::tests::{count_jobs, kind, new_job, queued, state};
use super::*;
use crate::test_support::*;

const T: Duration = Duration::from_secs(5);

#[tokio::test]
async fn a_job_enqueued_in_a_rolled_back_change_does_not_exist() {
    use crate::change::{Actor, AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome};
    let t = FreshDb::migrated().await;
    let ctx = ChangeContext::new(Actor::System, RequestId::parse("req-change").expect("id"));
    let mut tx = ChangeTx::begin(&t.db, &ctx).await.expect("begin");
    let job = new_job("test.a", None, 3, T);
    assert!(matches!(
        tx.enqueue_job(&job).await.expect("enqueue"),
        Enqueued::Queued(_)
    ));
    drop(tx);
    assert_eq!(count_jobs(&t).await, 0, "rolled back with the change");
    let mut tx = ChangeTx::begin(&t.db, &ctx).await.expect("begin");
    tx.enqueue_job(&job).await.expect("enqueue");
    let record = ChangeRecord {
        audit: AuditEntry {
            action: "repository.sealed",
            outcome: Outcome::Succeeded,
            resource_type: "repository",
            resource_id: Some(7),
            project_id: None,
            error_code: None,
            details: json!({}),
        },
        events: Vec::new(),
    };
    tx.commit(record).await.expect("commit");
    assert_eq!(count_jobs(&t).await, 1, "committed with the change");
    t.drop_now().await;
}

#[tokio::test]
async fn forge_ro_cannot_claim_a_job() {
    let t = FreshDb::migrated().await;
    let id = queued(&t, &new_job("test.a", None, 3, T)).await;
    let w = WorkerId::generate();
    // The read pool (forge_ro, read-only sessions).
    let mut ro = t.db.pool().executor().acquire().await.expect("read conn");
    claim_on(&mut ro, &w, &[kind("test.a")])
        .await
        .expect_err("the read pool cannot claim");
    drop(ro);
    // forge_ro in a writable session: no privilege.
    let mut conn = t.superuser().await;
    sqlx::query("SET ROLE forge_ro")
        .execute(&mut conn)
        .await
        .expect("set role");
    let err = claim_on(&mut conn, &w, &[kind("test.a")])
        .await
        .expect_err("forge_ro has no UPDATE");
    assert_reason(&err, ErrorCode::DbQueryFailed, "permission_denied");
    assert_eq!(state(&t, id).await.0, "queued");
    t.drop_now().await;
}
