//! ChangeTx against a migrated database: one transaction for state, audit
//! and events; the deferred audit-presence trigger; dedup; limits.

use std::time::Duration;

use forge_core::ids::{ProjectId, UserId};
use forge_core::{DedupKey, EventType, RequestId};
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
        event_type: EventType::RepositoryCreated,
        schema_version: 1,
        aggregate_type: "repository",
        aggregate_id: n,
        project_id: Some(ProjectId::new(3)),
        payload: json!({ "n": n }),
        // Well-formed keys in these tests; the writers' own check is
        // exercised with `unchecked` malformed ones below.
        dedup_key: dedup.map(DedupKey::unchecked),
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
    let mut other_project = event(1, None);
    other_project.project_id = Some(ProjectId::new(99));
    let mut huge_number = event(1, None);
    huge_number.payload = json!({ "n": u64::MAX });
    // Every part within 64 characters, 201 in all: only the total bound
    // refuses it (`exact_key` below, 200 characters, is accepted).
    let long_key = format!(
        "kk:{}",
        vec!["1".repeat(64); 3].join(":") + ":" + &"1".repeat(3)
    );
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
            record(vec![event(1, Some(&long_key))]),
            "invalid_dedup_key",
            None,
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
    // A key of exactly 200 characters is stored.
    let exact_key = format!(
        "k:{}",
        vec!["1".repeat(64); 3].join(":") + ":" + &"1".repeat(3)
    );
    assert_eq!(exact_key.len(), 200);
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    tx.commit(record(vec![event(1, Some(&exact_key))]))
        .await
        .expect("200 characters");
    t.drop_now().await;
}

/// The event scope rule (ADR-0016 Amendment D4): an event carries the
/// project of its change's audit row, in both directions.
#[tokio::test]
async fn a_change_whose_audit_row_names_no_project_cannot_publish_a_project_event() {
    let t = FreshDb::migrated().await;
    // No project on the audit row, a project event: refused.
    let mut instance_audit = record(vec![event(1, None)]);
    instance_audit.audit.project_id = None;
    // A project on the audit row, an instance-wide event: refused.
    let mut instance_event = event(2, None);
    instance_event.project_id = None;
    for rec in [instance_audit, record(vec![instance_event])] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        let err = tx.commit(rec).await.expect_err("scope");
        assert_reason(&err, ErrorCode::ChangeRejected, "event_scope_mismatch");
    }
    assert_eq!(count(&t, "forge.events").await, 0);
    assert_eq!(
        count(&t, "forge.audit_log WHERE request_id = 'req-test-1'").await,
        0
    );
    // Both sides instance-wide, and both on the same project, are fine.
    let mut instance = record(vec![DomainEvent {
        project_id: None,
        ..event(3, None)
    }]);
    instance.audit.project_id = None;
    for rec in [instance, record(vec![event(4, None)])] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        tx.commit(rec).await.expect("same scope");
    }
    assert_eq!(count(&t, "forge.events").await, 2);
    t.drop_now().await;
}

/// An audit row on a project resource names that project as its
/// `project_id`, so the event scope rule cannot be steered by the caller.
#[tokio::test]
async fn a_project_audit_row_must_name_its_own_project() {
    let t = FreshDb::migrated().await;
    let on_project = |project_id: Option<i64>| {
        let mut rec = record(vec![]);
        rec.audit.resource_type = "project";
        rec.audit.resource_id = Some(3);
        rec.audit.project_id = project_id.map(ProjectId::new);
        rec
    };
    for rec in [on_project(Some(4)), on_project(None)] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        let err = tx.commit(rec).await.expect_err("audit scope");
        assert_reason(&err, ErrorCode::ChangeRejected, "audit_scope_mismatch");
    }
    assert_eq!(
        count(&t, "forge.audit_log WHERE request_id = 'req-test-1'").await,
        0
    );
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    tx.commit(on_project(Some(3)))
        .await
        .expect("its own project");
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

/// The columns `client_values_are_cut_so_a_refusal_is_still_audited` reads.
type CutRow = (
    String,
    String,
    String,
    Option<String>,
    Vec<String>,
    serde_json::Value,
);

#[tokio::test]
async fn client_values_are_cut_so_a_refusal_is_still_audited() {
    let t = FreshDb::migrated().await;
    let mut change_ctx = ctx(Actor::UserViaServiceAccount {
        user: UserId::new(5),
        via: UserId::new(77),
        credential_id: 12,
        act_as_claimed: format!("\u{202E}{}", "c".repeat(200)),
        act_as_key_id: 1,
        via_assertion_id: Some("a".repeat(300)),
    });
    change_ctx.actor_label = Some(format!("evil\n\u{7}\u{202E}\u{200B}{}", "x".repeat(100)));
    change_ctx.user_agent = Some("ü".repeat(300));
    change_ctx.effective_scope = vec!["s".repeat(80); 40];
    for (n, details) in [
        json!({ "id": 9_007_199_254_740_993_u64 }),
        json!({ "echo": "d".repeat(9000) }),
    ]
    .into_iter()
    .enumerate()
    {
        let tx = ChangeTx::begin(&t.db, &change_ctx).await.expect("begin");
        let mut denied = record(vec![]);
        denied.audit.outcome = Outcome::Denied;
        denied.audit.resource_id = Some(n as i64);
        denied.audit.details = details;
        tx.commit(denied).await.expect("still audited");
    }
    let rows: Vec<CutRow> = sqlx::query_as(
        "SELECT actor_label, user_agent, act_as_claimed, via_assertion_id, effective_scope, \
         details FROM forge.audit_log WHERE outcome = 'denied' ORDER BY resource_id",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("rows");
    assert_eq!(rows.len(), 2);
    let (label, agent, claimed, assertion, scope, details) = &rows[0];
    assert_eq!(label.chars().count(), 64);
    assert!(label.starts_with("evilx"), "{label:?}");
    assert_eq!(agent.chars().count(), 256);
    assert_eq!(claimed.chars().count(), 128);
    assert!(claimed.starts_with('c'), "{claimed:?}");
    assert_eq!(assertion.as_deref().map(|a| a.chars().count()), Some(128));
    assert_eq!(scope.len(), 32);
    assert!(scope.iter().all(|s| s.chars().count() == 64));
    assert_eq!(details, &json!({ "details_dropped": "unsafe_number" }));
    assert_eq!(rows[1].5, json!({ "details_dropped": "too_long" }));
    t.drop_now().await;
}

#[tokio::test]
async fn a_succeeded_change_over_the_stored_size_is_rejected() {
    let t = FreshDb::migrated().await;
    let mut big_details = record(vec![]);
    big_details.audit.details = json!({ "echo": "d".repeat(9000) });
    let mut big_payload = event(1, None);
    big_payload.payload = json!({ "blob": "p".repeat(70_000) });
    for (rec, field) in [
        (big_details, "details"),
        (record(vec![big_payload]), "payload"),
    ] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        let err = tx.commit(rec).await.expect_err(field);
        assert_reason(&err, ErrorCode::ChangeRejected, "field_too_long");
        assert!(err.to_string().contains(&format!("field={field}")), "{err}");
    }
    assert_eq!(
        count(&t, "forge.audit_log WHERE action = 'repository.created'").await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_commit_that_does_not_answer_in_time_has_an_unknown_outcome() {
    let t = FreshDb::migrated().await;
    // A deferred check that runs at COMMIT and takes longer than its bound.
    t.exec_superuser(
        "CREATE FUNCTION public.slow_commit() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM pg_catalog.pg_sleep(2); RETURN NULL; END $$; \
         CREATE CONSTRAINT TRIGGER slow_commit AFTER INSERT ON forge.audit_log \
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.slow_commit()",
    )
    .await;
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin")
        .with_commit_timeout(Duration::from_millis(200));
    let err = tx
        .commit(record(vec![event(1, None)]))
        .await
        .expect_err("no answer in time");
    assert_reason(&err, ErrorCode::DbQueryFailed, "commit_outcome_unknown");
    assert!(err.to_string().contains("timeout_ms=200"), "{err}");
    assert!(
        err.to_string().contains("action=repository.created"),
        "{err}"
    );
    // The pool still serves the next change.
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin again");
    tx.commit(record(vec![event(2, None)]))
        .await
        .expect("next change");
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
async fn a_deadline_hit_while_waiting_on_a_lock_rolls_back_and_frees_the_connection() {
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

/// C5: a derived event needs no audit row, but an origin that does not
/// exist is refused at the insert (the lineage check; subscribers' own
/// tests cover the rest).
#[tokio::test]
async fn a_derived_event_of_an_unknown_origin_is_refused() {
    let t = FreshDb::migrated().await;
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let err = sqlx::query(
        "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, caused_by_seq, root_seq, causation_depth, \
         payload) VALUES (gen_random_uuid(), 'repository.deleted', 1, 'repository', 1, \
         'system', 'r', 999999, 999999, 1, '{}')",
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| query_error("test.derived", e))
    .expect_err("unknown origin");
    assert_reason(&err, ErrorCode::AuditMissing, "derived_lineage");
    drop(tx);
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

/// The budget leaves room for BEGIN on a loaded test cluster (300 ms flaked
/// with the whole suite running).
#[tokio::test]
async fn a_change_that_exceeds_its_deadline_is_rolled_back() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin_within(&t.db, &ctx(Actor::System), Duration::from_millis(2000))
        .await
        .expect("begin");
    let err = tx
        .within(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
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

#[tokio::test]
async fn every_event_type_passes_the_db_check() {
    let t = FreshDb::migrated().await;
    let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
        .await
        .expect("begin");
    let events: Vec<DomainEvent> = EventType::ALL
        .into_iter()
        .zip(1..)
        .map(|(event_type, n)| DomainEvent {
            event_type,
            project_id: None,
            ..event(n, None)
        })
        .collect();
    let mut rec = record(events);
    rec.audit.project_id = None;
    let committed = tx.commit(rec).await.expect("every type is stored");
    let Committed::Recorded { event_ids } = committed else {
        panic!("not deduplicated: {committed:?}");
    };
    assert_eq!(event_ids.len(), EventType::ALL.len());
    let mut stored: Vec<String> =
        sqlx::query_scalar("SELECT event_type FROM forge.events ORDER BY id")
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("types");
    stored.sort();
    let mut expected: Vec<&str> = EventType::ALL.iter().map(|e| e.as_str()).collect();
    expected.sort_unstable();
    assert_eq!(stored, expected);
    t.drop_now().await;
}

#[tokio::test]
async fn a_malformed_dedup_key_is_change_rejected() {
    let t = FreshDb::migrated().await;
    for raw in ["Push:1", "push", "push:a b", "push:1:2:3:4:5", "push:é"] {
        let tx = ChangeTx::begin(&t.db, &ctx(Actor::System))
            .await
            .expect("begin");
        let err = tx
            .commit(record(vec![event(1, Some(raw))]))
            .await
            .expect_err(raw);
        assert_reason(&err, ErrorCode::ChangeRejected, "invalid_dedup_key");
    }
    assert_eq!(count(&t, "forge.events").await, 0);
    t.drop_now().await;
}
