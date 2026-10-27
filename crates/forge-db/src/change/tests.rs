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
    assert!(err.to_string().contains("reason=no_audit_row"), "{err}");
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
    assert!(err.to_string().contains("count=101"), "{err}");
    assert_eq!(count(&t, "forge.events").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn invalid_changes_are_rejected_with_their_reason() {
    let t = FreshDb::migrated().await;
    let mut bad_type = event(1, None);
    bad_type.event_type = "Repository Created";
    let mut other_project = event(1, None);
    other_project.project_id = Some(ProjectId::new(99));
    let mut huge_number = event(1, None);
    huge_number.payload = json!({ "n": u64::MAX });
    let long_key = "k".repeat(201);
    let act_as = |credential_id: i64| Actor::UserViaServiceAccount {
        user: UserId::new(5),
        via: UserId::new(77),
        credential_id,
        act_as_claimed: "hmac".to_owned(),
        act_as_key_id: 1,
        via_assertion_id: None,
    };
    let mut mismatched = ctx(act_as(12));
    mismatched.credential_id = Some(13);
    let mut wide_scope = ctx(Actor::System);
    wide_scope.effective_scope = vec!["s".to_owned(); 33];
    let mut denied = record(vec![event(1, None)]);
    denied.audit.outcome = Outcome::Denied;
    let mut big_details = record(vec![]);
    big_details.audit.details = json!({ "id": 9_007_199_254_740_993_u64 });
    for (change_ctx, rec, reason, field) in [
        (
            ctx(Actor::Anonymous),
            record(vec![event(1, None)]),
            "anonymous_event",
            None,
        ),
        (
            ctx(Actor::System),
            record(vec![bad_type]),
            "invalid_event_type",
            None,
        ),
        (
            ctx(Actor::System),
            record(vec![event(1, Some(&long_key))]),
            "field_too_long",
            Some("dedup_key"),
        ),
        (ctx(Actor::System), denied, "events_on_unsuccessful", None),
        (
            ctx(Actor::System),
            record(vec![other_project]),
            "event_scope_mismatch",
            None,
        ),
        (
            ctx(Actor::System),
            record(vec![huge_number]),
            "unsafe_number",
            Some("payload"),
        ),
        (
            ctx(Actor::System),
            big_details,
            "unsafe_number",
            Some("details"),
        ),
        (
            mismatched,
            record(vec![event(1, None)]),
            "credential_mismatch",
            None,
        ),
        (
            wide_scope,
            record(vec![]),
            "field_too_long",
            Some("effective_scope"),
        ),
    ] {
        let tx = ChangeTx::begin(&t.db, &change_ctx).await.expect("begin");
        let err = tx.commit(rec).await.expect_err(reason);
        assert_reason(&err, ErrorCode::ChangeRejected, reason);
        if let Some(field) = field {
            assert!(err.to_string().contains(&format!("field={field}")), "{err}");
        }
    }
    assert_eq!(count(&t, "forge.events").await, 0);
    // An anonymous change without events (a failed login) is fine.
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::Anonymous))
        .await
        .expect("begin");
    let mut failed_login = record(vec![]);
    failed_login.audit.outcome = Outcome::Failed;
    tx.commit(failed_login).await.expect("audit only");
    t.drop_now().await;
}

#[tokio::test]
async fn exactly_max_events_is_accepted() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    let events = (0..MAX_EVENTS as i64).map(|n| event(n, None)).collect();
    tx.commit(record(events)).await.expect("100 events");
    assert_eq!(count(&t, "forge.events").await, MAX_EVENTS as i64);
    t.drop_now().await;
}

#[tokio::test]
async fn long_or_odd_personal_fields_are_cut_not_refused() {
    let t = FreshDb::migrated().await;
    let mut change_ctx = ctx(Actor::Anonymous);
    change_ctx.actor_label = Some(format!("evil\n\u{7}{}", "x".repeat(100)));
    change_ctx.user_agent = Some("ü".repeat(300));
    let tx = ChangeTx::begin(&t.db, &change_ctx).await.expect("begin");
    let mut denied = record(vec![]);
    denied.audit.outcome = Outcome::Denied;
    tx.commit(denied).await.expect("still audited");
    let (label, agent): (String, String) = sqlx::query_as(
        "SELECT actor_label, user_agent FROM forge.audit_log WHERE outcome = 'denied'",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("row");
    assert_eq!(label.chars().count(), 64);
    assert!(
        label.starts_with("evilx") && !label.contains('\n'),
        "{label:?}"
    );
    assert_eq!(agent.chars().count(), 256);
    t.drop_now().await;
}

#[tokio::test]
async fn a_dedup_hit_on_the_first_event_writes_nothing() {
    let t = FreshDb::migrated().await;
    let first = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    first
        .commit(record(vec![event(1, Some("push:1:abc"))]))
        .await
        .expect("first");
    for events in [
        vec![event(1, Some("push:1:abc"))],
        vec![event(1, Some("push:1:abc")), event(2, None)],
    ] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        assert_eq!(
            tx.commit(record(events)).await.expect("dedup"),
            Committed::Deduped
        );
    }
    assert_eq!(count(&t, "forge.events").await, 1);
    assert_eq!(
        count(&t, "forge.audit_log WHERE action = 'repository.created'").await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_deadline_that_expires_inside_commit_rolls_back_and_frees_the_connection() {
    let t = FreshDb::migrated().await;
    // Another transaction holds the same dedup_key, uncommitted: the
    // change's insert waits on it.
    let mut holder = t.db.write_pool().begin().await.expect("holder");
    sqlx::raw_sql(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'h'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, dedup_key, payload) VALUES (gen_random_uuid(), \
         'repository.created', 1, 'repository', 1, 'system', 'h', 'push:9:x', '{}')",
    )
    .execute(&mut *holder)
    .await
    .expect("holder row");
    let tx = ChangeTx::begin_within(&t.db, &ctx(Actor::System), Duration::from_millis(500))
        .await
        .expect("begin");
    let mut waiting = event(1, Some("push:9:x"));
    waiting.project_id = Some(ProjectId::new(3));
    let err = tx
        .commit(record(vec![waiting]))
        .await
        .expect_err("deadline");
    assert_reason(&err, ErrorCode::DbQueryFailed, "change_timeout");
    assert!(err.to_string().contains("timeout_ms=500"), "{err}");
    holder.rollback().await.expect("release");
    // The pool is usable and nothing of the timed-out change remains.
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin again");
    tx.commit(record(vec![event(2, None)]))
        .await
        .expect("next change");
    assert_eq!(count(&t, "forge.events").await, 1);
    t.drop_now().await;
}

/// actor, actor_kind, actor_user_id, actor_via_id, credential_id.
type ActorCase = (Actor, &'static str, Option<i64>, Option<i64>, Option<i64>);
/// The audit_log columns `every_actor_variant_records_its_columns` reads.
type AuditActorRow = (
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Vec<String>,
    String,
    String,
);

#[tokio::test]
async fn every_actor_variant_records_its_columns() {
    let t = FreshDb::migrated().await;
    let cases: Vec<ActorCase> = vec![
        (Actor::User(UserId::new(9)), "user", Some(9), None, Some(41)),
        (
            Actor::ServiceAccount(UserId::new(8)),
            "service_account",
            Some(8),
            None,
            Some(41),
        ),
        (Actor::System, "system", None, None, Some(41)),
        (
            Actor::UserViaServiceAccount {
                user: UserId::new(5),
                via: UserId::new(77),
                credential_id: 12,
                act_as_claimed: "hmac-abc".to_owned(),
                act_as_key_id: 1,
                via_assertion_id: Some("asr-1".to_owned()),
            },
            "user_via_service_account",
            Some(5),
            Some(77),
            Some(12),
        ),
    ];
    for (n, (actor, kind, user, via, credential)) in cases.into_iter().enumerate() {
        let mut change_ctx = ctx(actor);
        change_ctx.credential_id = if kind == "user_via_service_account" {
            None
        } else {
            Some(41)
        };
        change_ctx.effective_scope = vec!["api".to_owned()];
        let tx = ChangeTx::begin(&t.db, &change_ctx).await.expect("begin");
        let mut rec = record(vec![event(n as i64, None)]);
        rec.audit.resource_id = Some(n as i64);
        tx.commit(rec).await.expect("commit");
        let audit: AuditActorRow = sqlx::query_as(
            "SELECT actor_kind, actor_user_id, actor_via_id, credential_id, effective_scope, \
                 host(source_ip), request_id FROM forge.audit_log WHERE resource_id = $1 \
                 AND action = 'repository.created'",
        )
        .bind(n as i64)
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("audit");
        assert_eq!(
            (audit.0.as_str(), audit.1, audit.2, audit.3),
            (kind, user, via, credential),
            "{kind}"
        );
        assert_eq!(audit.4, ["api"]);
        assert_eq!(
            (audit.5.as_str(), audit.6.as_str()),
            ("192.0.2.7", "req-test-1")
        );
        let event_row: (String, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT actor_kind, actor_user_id, actor_via_id FROM forge.events \
             WHERE aggregate_id = $1",
        )
        .bind(n as i64)
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("event");
        assert_eq!(
            (event_row.0.as_str(), event_row.1, event_row.2),
            (kind, user, via)
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_derived_event_without_an_audit_row_cannot_commit_either() {
    let t = FreshDb::migrated().await;
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, caused_by_seq, root_seq, causation_depth, \
         payload) VALUES (gen_random_uuid(), 'repository.deleted', 1, 'repository', 1, \
         'system', 'r', 999999, 999999, 1, '{}')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert (checked at commit)");
    let err = tx
        .commit()
        .await
        .map_err(|e| query_error("test.derived", e))
        .expect_err("no audit row");
    assert_reason(&err, ErrorCode::AuditMissing, "no_audit_row");
    t.drop_now().await;
}

#[tokio::test]
async fn set_constraints_immediate_checks_at_the_insert() {
    let t = FreshDb::migrated().await;
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let err = sqlx::raw_sql(
        "SET CONSTRAINTS ALL IMMEDIATE; \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         'repository.created', 1, 'repository', 1, 'system', 'r', '{}')",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("test.immediate", e))
    .expect_err("checked now");
    assert_reason(&err, ErrorCode::AuditMissing, "no_audit_row");
    drop(tx);
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
