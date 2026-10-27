//! The sequencer against a migrated database: forge.assign_seq numbers
//! gap-free in id order, honours the batch size, never hands a seq out
//! twice, numbers audit rows only when both switches are on, beats the
//! heartbeat and notifies once per non-empty batch.

use std::collections::BTreeSet;
use std::time::Duration;

use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::ConnectOptions as _;

use super::*;
use crate::test_support::*;

/// `n` events (and their audit row) in one transaction.
pub(crate) async fn insert_events(t: &FreshDb, n: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'repository.created', 'succeeded', 'repository', 'r-1'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
         aggregate_id, actor_kind, request_id, payload) \
         SELECT gen_random_uuid(), 'repository.created', 1, 'repository', g, 'system', 'r-1', '{{}}' \
         FROM generate_series(1, {n}) AS g"
    ))
    .await;
}

async fn scalar(t: &FreshDb, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut t.superuser().await)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

async fn run(t: &FreshDb, batch: u16, audit: AuditSequencing) -> Assigned {
    run_once(&t.db, BatchSize::new(batch).expect("batch"), audit)
        .await
        .expect("run_once")
}

fn range(first: i64, last: i64) -> Option<SeqRange> {
    Some(SeqRange::new(first, last).expect("range"))
}

#[test]
fn a_batch_size_is_between_one_and_a_thousand() {
    assert!(BatchSize::new(0).is_none());
    assert!(BatchSize::new(1001).is_none());
    assert_eq!(BatchSize::new(1000), Some(BatchSize::MAX));
    assert!(SeqRange::new(5, 4).is_none());
    assert_eq!(SeqRange::new(3, 5).map(|r| r.count()), Some(3));
}

#[tokio::test]
async fn numbering_is_gap_free_and_follows_the_id_order() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 3).await;
    insert_events(&t, 2).await;
    let first = run(&t, 1000, AuditSequencing::Skip).await;
    assert_eq!(first.events, range(1, 5));
    assert_eq!(first.audit, None);
    let rows: Vec<(i64, i64, bool)> =
        sqlx::query_as("SELECT id, seq, sequenced_at IS NOT NULL FROM forge.events ORDER BY id")
            .fetch_all(&mut t.superuser().await)
            .await
            .expect("rows");
    let seqs: Vec<i64> = rows.iter().map(|(_, s, _)| *s).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4, 5]);
    assert!(rows.iter().all(|(.., at)| *at));
    assert_eq!(
        scalar(&t, "SELECT high_seq FROM forge.event_feed_state").await,
        5
    );
    // Nothing new: nothing assigned.
    assert!(run(&t, 1000, AuditSequencing::Skip).await.is_empty());
    insert_events(&t, 2).await;
    assert_eq!(
        run(&t, 1000, AuditSequencing::Skip).await.events,
        range(6, 7)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_run_takes_at_most_one_batch() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 7).await;
    for expected in [range(1, 3), range(4, 6), range(7, 7), None] {
        assert_eq!(run(&t, 3, AuditSequencing::Skip).await.events, expected);
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_full_batch_is_a_thousand_rows() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 1001).await;
    let assigned = run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("run");
    assert_eq!(assigned.events, range(1, 1000));
    assert_eq!(
        run(&t, 1000, AuditSequencing::Skip).await.events,
        range(1001, 1001)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn concurrent_sequencers_never_hand_out_a_seq_twice() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 200).await;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let db = t.db.clone(); // clone: each task holds the pool handles
        set.spawn(async move {
            let mut got = Vec::new();
            loop {
                let a = run_once(&db, BatchSize::new(7).expect("7"), AuditSequencing::Skip)
                    .await
                    .expect("run");
                match a.events {
                    Some(r) => got.push(r),
                    None => return got,
                }
            }
        });
    }
    let mut seen = BTreeSet::new();
    while let Some(ranges) = set.join_next().await {
        for r in ranges.expect("task") {
            for seq in r.first()..=r.last() {
                assert!(seen.insert(seq), "seq {seq} handed out twice");
            }
        }
    }
    assert_eq!(seen, (1..=200).collect::<BTreeSet<i64>>());
    assert_eq!(
        scalar(&t, "SELECT count(DISTINCT seq) FROM forge.events").await,
        200
    );
    t.drop_now().await;
}

#[tokio::test]
async fn audit_rows_are_numbered_only_when_enabled_and_requested() {
    let t = FreshDb::migrated().await;
    // Creates the feed state (audit sequencing off by default).
    run(&t, 1000, AuditSequencing::Skip).await;
    let pending = scalar(&t, "SELECT count(*) FROM forge.audit_log WHERE seq IS NULL").await;
    assert!(pending > 0, "the migrations' own rows");
    assert_eq!(run(&t, 1000, AuditSequencing::IfEnabled).await.audit, None);
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    assert_eq!(run(&t, 1000, AuditSequencing::Skip).await.audit, None);
    let assigned = run(&t, 1000, AuditSequencing::IfEnabled).await;
    assert_eq!(assigned.audit, range(1, pending));
    assert_eq!(
        scalar(&t, "SELECT audit_high_seq FROM forge.event_feed_state").await,
        pending
    );
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.audit_log WHERE seq IS NULL OR sequenced_at IS NULL"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_first_run_creates_the_feed_state_and_every_run_beats_the_heartbeat() {
    let t = FreshDb::migrated().await;
    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.event_feed_state").await,
        0
    );
    run(&t, 1000, AuditSequencing::Skip).await;
    let identity_matches = scalar(
        &t,
        "SELECT count(*) FROM forge.event_feed_state s, forge_audit.cluster_identity() c \
         WHERE s.system_identifier = c.system_identifier AND s.timeline_id = c.timeline_id \
           AND s.high_seq = 0 AND s.audit_high_seq = 0 AND NOT s.audit_sequencing_enabled",
    )
    .await;
    assert_eq!(identity_matches, 1);
    t.exec_superuser(
        "UPDATE forge.event_feed_state SET sequencer_heartbeat = now() - interval '1 hour'",
    )
    .await;
    // An empty run still beats it.
    run(&t, 1000, AuditSequencing::Skip).await;
    assert_eq!(
        scalar(
            &t,
            "SELECT count(*) FROM forge.event_feed_state \
             WHERE sequencer_heartbeat > now() - interval '1 minute'"
        )
        .await,
        1
    );
    t.drop_now().await;
}

/// The next notification's payload (the batch's last seq).
async fn next(listener: &mut PgListener) -> String {
    tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .expect("a notification in time")
        .expect("recv")
        .payload()
        .to_owned()
}

#[tokio::test]
async fn a_notification_follows_each_batch_that_numbered_events() {
    let t = FreshDb::migrated().await;
    let options: sqlx::postgres::PgConnectOptions = url_with_db(&t.name).parse().expect("dev url");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.password("forge_dev").disable_statement_logging())
        .await
        .expect("listen pool");
    let mut listener = PgListener::connect_with(&pool).await.expect("listener");
    listener.listen(CHANNEL).await.expect("listen");
    insert_events(&t, 3).await;
    run(&t, 2, AuditSequencing::Skip).await;
    assert_eq!(next(&mut listener).await, "2");
    run(&t, 2, AuditSequencing::Skip).await;
    assert_eq!(next(&mut listener).await, "3");
    // Empty runs, and a run that numbers audit rows only, stay silent: the
    // next notification is the next event batch.
    run(&t, 2, AuditSequencing::Skip).await;
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    assert!(run(&t, 2, AuditSequencing::IfEnabled).await.audit.is_some());
    insert_events(&t, 1).await;
    run(&t, 2, AuditSequencing::Skip).await;
    assert_eq!(next(&mut listener).await, "4");
    drop(listener);
    pool.close().await;
    t.drop_now().await;
}

#[tokio::test]
async fn the_application_may_execute_assign_seq_but_never_write_seq_itself() {
    let t = FreshDb::migrated().await;
    insert_events(&t, 1).await;
    let mut app = t.db.write_pool().acquire().await.expect("app connection");
    for sql in [
        "UPDATE forge.events SET seq = 1",
        "UPDATE forge.audit_log SET seq = 1",
        "UPDATE forge.event_feed_state SET high_seq = 1",
        "INSERT INTO forge.event_feed_state (epoch, system_identifier, timeline_id, \
         sequencer_heartbeat) VALUES (gen_random_uuid(), 1, 1, now())",
    ] {
        let err = sqlx::query(sql).execute(&mut *app).await.expect_err(sql);
        let state = err.as_database_error().and_then(|d| d.code());
        assert_eq!(state.as_deref(), Some("42501"), "{sql}");
    }
    let (from, to): (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT from_seq, to_seq FROM forge.assign_seq(10, false)")
            .fetch_one(&mut *app)
            .await
            .expect("forge_app executes assign_seq");
    assert_eq!((from, to), (Some(1), Some(1)));
    drop(app);
    // Only forge_app (and the owner) may run it.
    let privileges: Vec<(String, bool)> = sqlx::query_as(
        "SELECT r, has_function_privilege(r, 'forge.assign_seq(integer,boolean)', 'EXECUTE') \
         FROM unnest(ARRAY['forge_app', 'forge_ro', 'forge_maint', 'forge_purge', \
                           'forge_migrator']) AS r",
    )
    .fetch_all(&mut t.superuser().await)
    .await
    .expect("privileges");
    for (role, granted) in privileges {
        assert_eq!(granted, role == "forge_app", "{role}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_function_refuses_a_batch_outside_one_to_a_thousand() {
    let t = FreshDb::migrated().await;
    let mut app = t.db.write_pool().acquire().await.expect("app connection");
    for sql in [
        "SELECT * FROM forge.assign_seq(0, false)",
        "SELECT * FROM forge.assign_seq(1001, false)",
        "SELECT * FROM forge.assign_seq(10, NULL)",
    ] {
        let err = sqlx::query(sql).execute(&mut *app).await.expect_err(sql);
        let state = err.as_database_error().and_then(|d| d.code());
        assert_eq!(state.as_deref(), Some("22023"), "{sql}");
    }
    drop(app);
    t.drop_now().await;
}
