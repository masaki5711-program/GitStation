//! The seal job and verification against a migrated database: chains from
//! genesis, stops at gaps, never seals twice under concurrency, and the
//! backlog check sees old unsealed rows.

use std::collections::BTreeSet;

use forge_errors::ErrorCode;

use super::golden::golden_keys;
use super::*;
use crate::sequencer::{run_once, AuditSequencing, BatchSize};
use crate::test_support::*;

/// `n` audit rows (with personal columns) from forge_app's point of view.
pub(crate) async fn insert_audit(t: &FreshDb, n: i64) {
    t.exec_superuser(&format!(
        "INSERT INTO forge.audit_log (actor_kind, actor_user_id, actor_label, action, outcome, \
         resource_type, resource_id, source_ip, user_agent, request_id, details) \
         SELECT 'user', 42, 'Alice', 'repository.viewed', 'succeeded', 'repository', g, \
         '10.0.0.1', 'git/2.47', 'r-' || g, jsonb_build_object('n', g) \
         FROM generate_series(1, {n}) AS g"
    ))
    .await;
}

/// Turn audit sequencing on (as the superuser) and number every waiting row.
pub(crate) async fn number_all(t: &FreshDb) {
    // Creates the feed-state row first.
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("feed state");
    t.exec_superuser("UPDATE forge.event_feed_state SET audit_sequencing_enabled = true")
        .await;
    while run_once(&t.db, BatchSize::MAX, AuditSequencing::IfEnabled)
        .await
        .expect("sequence")
        .audit
        .is_some()
    {}
}

pub(crate) async fn seal_all(t: &FreshDb) -> Vec<Sealed> {
    let mut out = Vec::new();
    loop {
        match seal_pending(&t.db, &golden_keys()).await.expect("seal") {
            SealOutcome::Sealed(s) => out.push(s),
            SealOutcome::UpToDate => return out,
            SealOutcome::Busy => panic!("nobody else seals"),
        }
    }
}

pub(crate) async fn verify_all(t: &FreshDb) -> Result<Verified, forge_errors::ForgeError> {
    verify_range(
        &t.db,
        Some(&golden_keys()),
        VerifyScope::SealedWithin(DOCTOR_WINDOW),
    )
    .await
}

async fn scalar(t: &FreshDb, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(&mut t.superuser().await)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[tokio::test]
async fn nothing_numbered_is_up_to_date() {
    let t = FreshDb::migrated().await;
    // No feed state yet, then a feed state without numbered audit rows.
    assert_eq!(
        seal_pending(&t.db, &golden_keys()).await.expect("seal"),
        SealOutcome::UpToDate
    );
    run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("feed state");
    assert_eq!(
        seal_pending(&t.db, &golden_keys()).await.expect("seal"),
        SealOutcome::UpToDate
    );
    t.drop_now().await;
}

#[tokio::test]
async fn three_seals_chain_from_genesis_and_verify() {
    let t = FreshDb::migrated().await;
    let mut ranges = Vec::new();
    for n in [2, 3, 1] {
        insert_audit(&t, n).await;
        number_all(&t).await;
        let sealed = seal_all(&t).await;
        assert_eq!(sealed.len(), 1, "{sealed:?}");
        ranges.push(sealed[0].range);
    }
    // Contiguous from 1 (the migrations' own rows come first).
    assert_eq!(ranges[0].first(), 1);
    assert_eq!(ranges[1].first(), ranges[0].last() + 1);
    assert_eq!(ranges[2].first(), ranges[1].last() + 1);
    assert_eq!(ranges[2].count(), 1);
    let chained: i64 = scalar(
        &t,
        "SELECT count(*) FROM forge.audit_seal s \
         JOIN forge.audit_seal p ON p.last_seq = s.first_seq - 1 \
         WHERE s.prev_seal_sha256 = p.seal_sha256",
    )
    .await;
    assert_eq!(chained, 2);
    let genesis: i64 = scalar(
        &t,
        "SELECT count(*) FROM forge.audit_seal \
         WHERE first_seq = 1 AND prev_seal_sha256 = sha256('forge-audit-genesis'::bytea)",
    )
    .await;
    assert_eq!(genesis, 1);
    let verified = verify_all(&t).await.expect("verifies");
    assert_eq!(verified.seals, 3);
    assert_eq!(verified.rows, ranges[2].last());
    assert_eq!(verified.hmac, HmacCheck::Checked);
    // Without keys: values and chain only.
    let unkeyed = verify_range(&t.db, None, VerifyScope::Seqs(ranges[1]))
        .await
        .expect("verifies");
    assert_eq!((unkeyed.seals, unkeyed.hmac), (1, HmacCheck::NoKeys));
    t.drop_now().await;
}

#[tokio::test]
async fn a_large_backlog_is_sealed_in_bounded_steps() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, MAX_SEAL_ROWS + 10).await;
    number_all(&t).await;
    let sealed = seal_all(&t).await;
    assert_eq!(sealed.len(), 2, "{sealed:?}");
    assert_eq!(sealed[0].range.count(), MAX_SEAL_ROWS);
    assert!(sealed[0].more && !sealed[1].more);
    verify_all(&t).await.expect("verifies");
    t.drop_now().await;
}

#[tokio::test]
async fn a_gap_in_the_numbers_is_not_sealed() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 2).await;
    number_all(&t).await;
    seal_all(&t).await;
    let high = scalar(&t, "SELECT audit_high_seq FROM forge.event_feed_state").await;
    // forge_seq skips a number within the guards' window.
    insert_audit(&t, 1).await;
    t.exec_superuser(&format!(
        "SET ROLE forge_seq; \
         UPDATE forge.audit_log SET seq = {skip} WHERE seq IS NULL AND request_id = 'r-1'; \
         UPDATE forge.event_feed_state SET audit_high_seq = {skip}; \
         RESET ROLE;",
        skip = high + 2
    ))
    .await;
    let err = seal_pending(&t.db, &golden_keys()).await.expect_err("gap");
    assert_reason(&err, ErrorCode::AuditSealMismatch, "gap");
    let text = err.to_string();
    assert!(
        text.contains(&format!("missing_seq={}", high + 1)),
        "{text}"
    );
    assert!(text.contains(&format!("first_seq={}", high + 1)), "{text}");
    assert_eq!(
        scalar(&t, "SELECT count(*) FROM forge.audit_seal").await,
        1,
        "nothing more sealed"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn concurrent_seal_pending_callers_do_not_double_seal() {
    let t = FreshDb::migrated().await;
    insert_audit(&t, 50).await;
    number_all(&t).await;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let db = t.db.clone(); // clone: each task holds the pool handles
        set.spawn(async move {
            let keys = golden_keys();
            let mut got = Vec::new();
            for _ in 0..20 {
                match seal_pending(&db, &keys).await.expect("seal") {
                    SealOutcome::Sealed(s) => got.push(s.range),
                    SealOutcome::Busy => tokio::task::yield_now().await,
                    SealOutcome::UpToDate => break,
                }
            }
            got
        });
    }
    let mut seen = BTreeSet::new();
    while let Some(ranges) = set.join_next().await {
        for r in ranges.expect("task") {
            for seq in r.first()..=r.last() {
                assert!(seen.insert(seq), "seq {seq} sealed twice");
            }
        }
    }
    // Every numbered row is sealed exactly once (a final call finishes a
    // range a Busy loser left).
    seal_all(&t).await;
    let high = scalar(&t, "SELECT audit_high_seq FROM forge.event_feed_state").await;
    assert_eq!(
        scalar(
            &t,
            "SELECT coalesce(sum(row_count), 0)::bigint FROM forge.audit_seal"
        )
        .await,
        high
    );
    verify_all(&t).await.expect("verifies");
    t.drop_now().await;
}

#[tokio::test]
async fn an_old_unsealed_row_is_audit_seal_backlog() {
    let t = FreshDb::migrated().await;
    let b = backlog(&t.db).await.expect("backlog").expect("migrated");
    // The migrations' rows wait for their number (sequencing off) but are new.
    assert!(!b.audit_sequencing && b.unsequenced > 0, "{b:?}");
    b.check().expect("young rows are no backlog");
    insert_audit(&t, 3).await;
    number_all(&t).await;
    seal_all(&t).await;
    let sealed = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!((sealed.oldest_age_ms, sealed.unsealed_numbered), (None, 0));
    // A numbered, unsealed row from 6 minutes ago.
    insert_audit(&t, 1).await;
    t.exec_superuser(
        "ALTER TABLE forge.audit_log DISABLE TRIGGER audit_log_append_only; \
         UPDATE forge.audit_log SET occurred_at = now() - interval '6 minutes' WHERE seq IS NULL; \
         ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_append_only;",
    )
    .await;
    let waiting = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!(waiting.unsequenced, 1);
    let err = waiting.check().expect_err("backlog");
    assert_eq!(err.code(), ErrorCode::AuditSealBacklog);
    assert!(err.to_string().contains("audit_sequencing=on"), "{err}");
    number_all(&t).await;
    let numbered = backlog(&t.db).await.expect("backlog").expect("migrated");
    assert_eq!((numbered.unsequenced, numbered.unsealed_numbered), (0, 1));
    assert!(numbered.check().is_err());
    seal_all(&t).await;
    backlog(&t.db)
        .await
        .expect("backlog")
        .expect("migrated")
        .check()
        .expect("sealed again");
    t.drop_now().await;
}
