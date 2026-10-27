//! ChangeTx against a migrated database: one transaction for state, audit
//! and events; the deferred audit-presence trigger; dedup; limits.

use std::time::Duration;

use forge_core::ids::{ProjectId, UserId};
use forge_core::RequestId;
use forge_errors::ErrorCode;
use serde_json::json;

use super::*;
use crate::query_error;
use crate::test_support::*;

fn ctx(actor: Actor) -> ChangeContext {
    let mut ctx = ChangeContext::new(actor, RequestId::parse("req-test-1").expect("id"));
    ctx.actor_label = Some("someone".to_owned());
    ctx.source_ip = Some("192.0.2.7".parse().expect("ip"));
    ctx
}

fn event(n: i64, dedup: Option<&str>) -> DomainEvent {
    DomainEvent {
        event_type: "repository.created",
        schema_version: 1,
        aggregate_type: "repository",
        aggregate_id: n,
        project_id: Some(ProjectId::new(3)),
        payload: json!({ "n": n }),
        dedup_key: dedup.map(str::to_owned),
    }
}

fn record(events: Vec<DomainEvent>) -> ChangeRecord {
    ChangeRecord {
        audit: AuditEntry {
            action: "repository.created",
            outcome: Outcome::Succeeded,
            resource_type: "repository",
            resource_id: Some(1),
            project_id: Some(ProjectId::new(3)),
            error_code: None,
            details: json!({ "name": "r1" }),
        },
        events,
    }
}

async fn count(t: &FreshDb, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("count")
}

#[tokio::test]
async fn a_change_records_its_audit_row_and_events_in_one_transaction() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::User(UserId::new(9))))
        .await
        .expect("begin");
    let Committed::Recorded { event_ids } = tx
        .commit(record(vec![event(1, None), event(2, None)]))
        .await
        .expect("commit")
    else {
        panic!("not deduped");
    };
    assert_eq!(event_ids.len(), 2);
    let rows: Vec<(String, String, i64, String)> = sqlx::query_as(
        "SELECT e.txid::text, e.request_id, e.actor_user_id, a.actor_kind \
         FROM forge.events e JOIN forge.audit_log a ON a.txid = e.txid \
         WHERE a.action = 'repository.created' ORDER BY e.id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("rows");
    assert_eq!(
        rows.len(),
        2,
        "both events share the audit row's transaction"
    );
    assert!(rows
        .iter()
        .all(|r| r.1 == "req-test-1" && r.2 == 9 && r.3 == "user"));
    t.drop_now().await;
}

#[tokio::test]
async fn an_event_without_an_audit_row_cannot_commit() {
    let t = FreshDb::migrated().await;
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES \
         (gen_random_uuid(), 'repository.created', 1, 'repository', 1, 'system', 'r', '{}')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert (checked at commit)");
    let err = tx
        .commit()
        .await
        .map_err(|e| query_error("test.bypass", e))
        .expect_err("no audit row");
    assert_eq!(err.code(), ErrorCode::AuditMissing, "{err}");
    assert!(err.to_string().contains("sqlstate=XF002"), "{err}");
    assert_eq!(count(&t, "forge.events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_change_with_too_many_events_is_rejected_before_writing() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    let events = (0..=MAX_EVENTS as i64).map(|n| event(n, None)).collect();
    let err = tx.commit(record(events)).await.expect_err("101 events");
    assert_reason(&err, ErrorCode::ChangeRejected, "too_many_events");
    assert_eq!(count(&t, "forge.events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn invalid_changes_are_rejected_with_their_reason() {
    let t = FreshDb::migrated().await;
    let mut bad_type = event(1, None);
    bad_type.event_type = "Repository Created";
    let long_key = "k".repeat(201);
    for (actor, events, reason) in [
        (Actor::Anonymous, vec![event(1, None)], "anonymous_event"),
        (Actor::System, vec![bad_type], "invalid_event_type"),
        (
            Actor::System,
            vec![event(1, Some(&long_key))],
            "field_too_long",
        ),
    ] {
        let tx = ChangeTx::begin(&t.db, &ctx(actor)).await.expect("begin");
        let err = tx.commit(record(events)).await.expect_err(reason);
        assert_reason(&err, ErrorCode::ChangeRejected, reason);
    }
    // An anonymous change without events (a failed login) is fine.
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::Anonymous))
        .await
        .expect("begin");
    tx.commit(record(vec![])).await.expect("audit only");
    t.drop_now().await;
}

#[tokio::test]
async fn a_repeated_dedup_key_rolls_back_the_whole_change() {
    let t = FreshDb::migrated().await;
    let first = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    first
        .commit(record(vec![event(1, Some("push:1:abc"))]))
        .await
        .expect("first");
    let mut again = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    // A state write of the same change must be undone with it.
    sqlx::query(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
         origin_request_id) VALUES ('t', '{}', 'queued', 1, 1000, 'r')",
    )
    .execute(again.conn())
    .await
    .expect("state write");
    let outcome = again
        .commit(record(vec![event(2, None), event(1, Some("push:1:abc"))]))
        .await
        .expect("second");
    assert_eq!(outcome, Committed::Deduped);
    assert_eq!(count(&t, "forge.events").await, 1);
    assert_eq!(count(&t, "forge.jobs").await, 0);
    assert_eq!(
        count(&t, "forge.audit_log WHERE action = 'repository.created'").await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_change_that_exceeds_its_deadline_is_rolled_back() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin_within(&t.db, &ctx(Actor::System), Duration::from_millis(300))
        .await
        .expect("begin");
    let err = tx
        .within(async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(())
        })
        .await
        .expect_err("deadline");
    assert_reason(&err, ErrorCode::DbQueryFailed, "change_timeout");
    drop(tx);
    assert_eq!(
        count(&t, "forge.audit_log WHERE action <> 'db.migrate'").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_act_as_change_records_both_people() {
    let t = FreshDb::migrated().await;
    let actor = Actor::UserViaServiceAccount {
        user: UserId::new(5),
        via: UserId::new(77),
        credential_id: 12,
        act_as_claimed: "hmac-abc".to_owned(),
        act_as_key_id: 1,
        via_assertion_id: Some("asr-1".to_owned()),
    };
    let tx = ChangeTx::begin(&t.db, &ctx(actor)).await.expect("begin");
    tx.commit(record(vec![event(1, None)]))
        .await
        .expect("commit");
    let row: (String, i64, i64, i64, String) = sqlx::query_as(
        "SELECT actor_kind, actor_user_id, actor_via_id, credential_id, via_assertion_id \
         FROM forge.audit_log WHERE action = 'repository.created'",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("row");
    assert_eq!(
        row,
        (
            "user_via_service_account".to_owned(),
            5,
            77,
            12,
            "asr-1".to_owned()
        )
    );
    t.drop_now().await;
}
