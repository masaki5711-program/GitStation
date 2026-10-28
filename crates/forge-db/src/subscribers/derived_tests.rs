//! Derived events: DerivedTx sets the lineage and needs no audit row; the
//! triggers of migration 20261027140000 refuse a forged lineage, the depth
//! limit, the fan-out limit and a derived event without the subscriber's
//! advance (XF002 → FG-DB-0013); plain events still need an audit row.

use std::sync::atomic::Ordering as Atomic;

use forge_errors::ErrorCode;
use sqlx::Connection as _;

use super::tests::{cursor, published, scalar, step, Scripted};
use super::*;
use crate::query_error;
use crate::test_support::*;

const INSERT_DERIVED: &str = "INSERT INTO forge.events (event_id, event_type, schema_version, \
     aggregate_type, aggregate_id, actor_kind, request_id, caused_by_seq, root_seq, \
     causation_depth, payload) VALUES (gen_random_uuid(), 'repository.indexed', 1, \
     'repository', 1, 'system', 'r', $1, $2, $3, '{}')";

/// Insert a derived event (as forge_app) in its own transaction, optionally
/// after advancing subscriber `adv` to `caused_by` there.
async fn derive_by_hand(
    t: &FreshDb,
    advance: Option<&str>,
    caused_by: i64,
    root: i64,
    depth: i16,
) -> Result<(), ForgeError> {
    let mut conn = t.db.write_pool().acquire().await.expect("acquire");
    let mut tx = conn.begin().await.expect("begin");
    if let Some(name) = advance {
        sqlx::query(
            "UPDATE forge.event_subscribers SET last_seq = $2, \
             last_txid = pg_current_xact_id() WHERE name = $1",
        )
        .bind(name)
        .bind(caused_by)
        .execute(&mut *tx)
        .await
        .expect("advance");
    }
    sqlx::query(INSERT_DERIVED)
        .bind(caused_by)
        .bind(root)
        .bind(depth)
        .execute(&mut *tx)
        .await
        .map_err(|e| query_error("test.derived", e))?;
    tx.commit().await.map_err(|e| query_error("test.commit", e))
}

async fn subscriber(
    t: &FreshDb,
    name: &'static str,
    ordering: Ordering,
) -> std::sync::Arc<Scripted> {
    let s = Scripted::new(name, ordering);
    register(&t.db, s.as_ref(), Start::Beginning)
        .await
        .expect("register");
    s
}

#[tokio::test]
async fn a_derived_event_carries_its_lineage_and_needs_no_audit_row() {
    let t = FreshDb::migrated().await;
    let s = subscriber(&t, "indexer", Ordering::Ordered).await;
    published(&t, 1).await;
    s.derive.store(2, Atomic::SeqCst);
    let audit_before = scalar(&t, "SELECT count(*) FROM forge.audit_log").await;
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 1, derived: 2 }
    ));
    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.audit_log").await,
        audit_before
    );
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.events WHERE caused_by_seq = 1 AND root_seq = 1 \
             AND causation_depth = 1 AND actor_kind = 'system' AND request_id = 'r-1'"
        )
        .await,
        2
    );
    // A derived event is published like any other and derives in turn: the
    // root stays, the depth grows.
    crate::sequencer::run_once(
        &t.db,
        crate::sequencer::BatchSize::MAX,
        crate::sequencer::AuditSequencing::Skip,
    )
    .await
    .expect("sequence the derived events");
    s.derive.store(1, Atomic::SeqCst);
    assert!(matches!(
        step(&t, &s).await,
        Progress::Handled { seq: 2, derived: 1 }
    ));
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.events WHERE caused_by_seq = 2 AND root_seq = 1 \
             AND causation_depth = 2"
        )
        .await,
        1
    );
    t.drop_now().await;
}

#[tokio::test]
async fn plain_events_still_need_an_audit_row_and_derived_ones_need_the_advance() {
    let t = FreshDb::migrated().await;
    let _s = subscriber(&t, "manual", Ordering::Ordered).await;
    published(&t, 1).await;
    // A plain event without an audit row: refused at COMMIT.
    let mut conn = t.db.write_pool().acquire().await.expect("acquire");
    let mut tx = conn.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) VALUES (gen_random_uuid(), \
         'repository.created', 1, 'repository', 1, 'system', 'r', '{}')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert");
    let err = tx
        .commit()
        .await
        .map_err(|e| query_error("test.plain", e))
        .expect_err("no audit row");
    assert_reason(&err, ErrorCode::AuditMissing, "no_audit_row");
    drop(conn);
    // A derived event without the subscriber's advance: refused at COMMIT.
    let err = derive_by_hand(&t, None, 1, 1, 1)
        .await
        .expect_err("no advance");
    assert_reason(&err, ErrorCode::AuditMissing, "derived_progress");
    // With it, and without an audit row: accepted.
    derive_by_hand(&t, Some("manual"), 1, 1, 1)
        .await
        .expect("advance + derived event");
    t.drop_now().await;
}

#[tokio::test]
async fn a_forged_lineage_is_refused_at_the_insert() {
    let t = FreshDb::migrated().await;
    let _s = subscriber(&t, "forger", Ordering::Ordered).await;
    published(&t, 2).await;
    for (caused_by, root, depth) in [(999, 999, 1), (1, 2, 1), (1, 1, 2)] {
        let err = derive_by_hand(&t, Some("forger"), caused_by, root, depth)
            .await
            .expect_err("forged");
        assert_reason(&err, ErrorCode::AuditMissing, "derived_lineage");
        assert!(
            err.to_string().contains("constraint=derived_lineage"),
            "{err}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_depth_limit_stops_derivation() {
    let t = FreshDb::migrated().await;
    let s = subscriber(&t, "deep", Ordering::Independent).await;
    published(&t, 1).await;
    // Build a chain 1 → … at depth 8 by hand (each link with its advance).
    for depth in 1..=8_i16 {
        let origin = scalar(&t, "SELECT max(seq) FROM forge.events").await;
        derive_by_hand(&t, Some("deep"), origin, 1, depth)
            .await
            .unwrap_or_else(|e| panic!("depth {depth}: {e}"));
        crate::sequencer::run_once(
            &t.db,
            crate::sequencer::BatchSize::MAX,
            crate::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequence");
    }
    let deepest = scalar(&t, "SELECT max(seq) FROM forge.events").await;
    assert_eq!(
        scalar(
            &t,
            &format!("SELECT causation_depth::bigint FROM forge.events WHERE seq = {deepest}")
        )
        .await,
        8
    );
    // The trigger refuses a ninth level…
    let err = derive_by_hand(&t, Some("deep"), deepest, 1, 9)
        .await
        .expect_err("depth 9");
    assert_reason(&err, ErrorCode::AuditMissing, "derived_depth");
    // …and DerivedTx never tries: the independent subscriber quarantines it.
    t.exec_superuser(&format!(
        "UPDATE forge.event_subscribers SET last_seq = {} WHERE name = 'deep'",
        deepest - 1
    ))
    .await;
    s.derive.store(1, Atomic::SeqCst);
    let Progress::Quarantined(incident) = step(&t, &s).await else {
        panic!("fan-out limit quarantines at once");
    };
    assert_reason(&incident.error, ErrorCode::SubscriberLag, "fanout_limit");
    assert!(
        incident.error.to_string().contains("limit=depth"),
        "{}",
        incident.error
    );
    assert_eq!(cursor(&t, "deep").await, deepest);
    t.drop_now().await;
}

#[tokio::test]
async fn the_fanout_limit_is_one_hundred_derived_events_per_root() {
    let t = FreshDb::migrated().await;
    let ordered = subscriber(&t, "fan-ordered", Ordering::Ordered).await;
    let independent = subscriber(&t, "fan-independent", Ordering::Independent).await;
    let _writer = subscriber(&t, "fan-writer", Ordering::Independent).await;
    published(&t, 1).await;
    // 100 derived events of root 1, written by hand in one transaction
    // together with the writer's advance.
    t.exec_superuser(
        "BEGIN; \
         UPDATE forge.event_subscribers SET last_txid = pg_current_xact_id(), last_seq = 1 \
         WHERE name = 'fan-writer'; \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, caused_by_seq, root_seq, causation_depth, \
         payload) SELECT gen_random_uuid(), 'repository.indexed', 1, 'repository', g, \
         'system', 'r', 1, 1, 1, '{}' FROM generate_series(1, 100) AS g; \
         COMMIT",
    )
    .await;
    // The trigger refuses the 101st…
    let err = derive_by_hand(&t, Some("fan-independent"), 1, 1, 1)
        .await
        .expect_err("101st");
    assert_reason(&err, ErrorCode::AuditMissing, "derived_fanout");
    // …and DerivedTx stops before it: ordered stops, independent quarantines.
    ordered.derive.store(1, Atomic::SeqCst);
    independent.derive.store(1, Atomic::SeqCst);
    let Progress::Stuck(stuck) = step(&t, &ordered).await else {
        panic!("ordered stops at the fan-out limit");
    };
    assert_reason(&stuck.error, ErrorCode::SubscriberLag, "fanout_limit");
    assert!(
        stuck.error.to_string().contains("limit=per_root"),
        "{}",
        stuck.error
    );
    assert_eq!(cursor(&t, "fan-ordered").await, 0);
    let Progress::Quarantined(quarantined) = step(&t, &independent).await else {
        panic!("independent quarantines at the fan-out limit");
    };
    assert_reason(&quarantined.error, ErrorCode::SubscriberLag, "fanout_limit");
    assert_eq!(cursor(&t, "fan-independent").await, 1);
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.subscriber_dead_letters \
             WHERE subscriber = 'fan-independent' AND seq = 1 AND error_code = 'FG-DB-0008'"
        )
        .await,
        1
    );
    t.drop_now().await;
}
