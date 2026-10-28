//! What verification selects and recomputes beyond the basic tampering
//! cases: the overlap of `VerifyScope::Seqs`, the doctor's 24-hour window
//! (and its unauthenticated `sealed_at`), a range that does not follow a
//! seal, an unknown hash version, and `kind` / `key_id` being covered by
//! `seal_sha256`.

use forge_core::config::SealKeys;

use super::golden::golden_keys;
use super::tamper_tests::{assert_mismatch, sealed_twice, LOG_GUARD_OFF, SEAL_GUARD_OFF};
use super::tests::{insert_audit, number_all, seal_all, verify_all};
use super::*;
use crate::sequencer::SeqRange;
use crate::test_support::*;

fn seqs(first: i64, last: i64) -> VerifyScope {
    VerifyScope::Seqs(SeqRange::new(first, last).expect("range"))
}

/// The range of the latest seal.
async fn latest(t: &FreshDb) -> (i64, i64) {
    sqlx::query_as(
        "SELECT first_seq, last_seq FROM forge.audit_seal ORDER BY last_seq DESC LIMIT 1",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("latest seal")
}

#[tokio::test]
async fn verify_seqs_checks_exactly_the_seals_overlapping_the_range() {
    let t = FreshDb::migrated().await;
    let mut ranges = Vec::new();
    for n in [2, 3, 1] {
        insert_audit(&t, n).await;
        number_all(&t).await;
        ranges.extend(seal_all(&t).await.into_iter().map(|s| s.range));
    }
    let keys = golden_keys();
    let check = |scope| verify_range(&t.db, Some(&keys), scope);
    let (r0, r1, r2) = (ranges[0], ranges[1], ranges[2]);
    // Across the boundary of the first two seals.
    let two = check(seqs(r0.last(), r1.first())).await.expect("ok");
    assert_eq!((two.seals, two.rows), (2, r0.count() + r1.count()));
    // One number inside the second seal.
    let one = check(seqs(r1.first() + 1, r1.first() + 1))
        .await
        .expect("ok");
    assert_eq!((one.seals, one.rows), (1, r1.count()));
    // Past the last seal: nothing.
    let none = check(seqs(r2.last() + 1, r2.last() + 10))
        .await
        .expect("ok");
    assert_eq!((none.seals, none.truncated), (0, false));
    let all = check(seqs(1, r2.last())).await.expect("ok");
    assert_eq!((all.seals, all.rows), (3, r2.last()));
    t.drop_now().await;
}

#[tokio::test]
async fn a_seal_older_than_the_window_is_left_to_the_full_walk() {
    let t = sealed_twice().await;
    // sealed_at is not authenticated: backdating the first seal (past the
    // guard) and tampering its rows hides it from the doctor's window...
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; \
         UPDATE forge.audit_seal SET sealed_at = now() - interval '25 hours' WHERE first_seq = 1; \
         {LOG_GUARD_OFF}; UPDATE forge.audit_log SET outcome = 'denied' \
         WHERE seq = (SELECT last_seq FROM forge.audit_seal WHERE first_seq = 1)"
    ))
    .await;
    let recent = verify_all(&t)
        .await
        .expect("only the second seal is recent");
    assert_eq!(recent.seals, 1);
    // ...but not from the walk over every seal (C9 audit.seal_reverify).
    let (_, last) = latest(&t).await;
    let err = verify_range(&t.db, Some(&golden_keys()), seqs(1, last))
        .await
        .expect_err("tampered");
    assert_mismatch(&err, "recompute", "rows_sha256");
    t.drop_now().await;
}

#[tokio::test]
async fn a_seal_that_does_not_follow_a_seal_is_chain_first_seq() {
    let t = sealed_twice().await;
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET first_seq = first_seq + 1, \
         row_count = row_count - 1 WHERE last_seq = (SELECT max(last_seq) FROM forge.audit_seal)"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("chain");
    assert_mismatch(&err, "chain", "first_seq");
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_hash_version_is_recompute_hash_version() {
    let t = sealed_twice().await;
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET hash_version = 2 \
         WHERE last_seq = (SELECT max(last_seq) FROM forge.audit_seal)"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("version");
    assert_mismatch(&err, "recompute", "hash_version");
    assert!(err.to_string().contains("hash_version=2"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn kind_and_key_id_are_covered_by_seal_sha256() {
    let t = sealed_twice().await;
    let newest = "WHERE last_seq = (SELECT max(last_seq) FROM forge.audit_seal)";
    // kind: C9 relaxes the gap check for imported seals.
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET kind = 'imported' {newest}"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("kind");
    assert_mismatch(&err, "recompute", "seal_sha256");
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET kind = 'normal', key_id = 2 {newest}"
    ))
    .await;
    // With both keys the HMAC no longer matches; without keys the value
    // itself does not (key_id is part of seal_sha256).
    let both =
        SealKeys::parse(&format!("1 {}\n2 {}", "42".repeat(32), "43".repeat(32))).expect("keys");
    let err = verify_range(&t.db, Some(&both), VerifyScope::SealedWithin(DOCTOR_WINDOW))
        .await
        .expect_err("key_id");
    assert_mismatch(&err, "hmac", "mismatch");
    let err = verify_range(&t.db, None, VerifyScope::SealedWithin(DOCTOR_WINDOW))
        .await
        .expect_err("key_id");
    assert_mismatch(&err, "recompute", "seal_sha256");
    t.drop_now().await;
}
