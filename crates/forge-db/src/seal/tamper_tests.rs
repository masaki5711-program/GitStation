//! What the seals catch and what the guard of forge.audit_seal refuses:
//! tampering past the guards (the superuser disables a trigger) is found by
//! verification with the right reason; the seal rows themselves only ever
//! gain shipped_at, once.

use forge_core::config::SealKeys;
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::tests::{insert_audit, number_all, seal_all, verify_all};
use super::*;
use crate::test_support::*;

/// Two seals over migration rows plus 3 + 2 audit rows.
pub(super) async fn sealed_twice() -> FreshDb {
    let t = FreshDb::migrated().await;
    for n in [3, 2] {
        insert_audit(&t, n).await;
        number_all(&t).await;
        seal_all(&t).await;
    }
    t
}

pub(super) fn assert_mismatch(err: &ForgeError, reason: &str, detail: &str) {
    assert_reason(err, ErrorCode::AuditSealMismatch, reason);
    assert!(
        err.to_string().contains(&format!("detail={detail}")),
        "{err}"
    );
}

/// The SQLSTATE of a failed statement.
async fn state_of(conn: &mut PgConnection, sql: &str) -> Option<String> {
    match sqlx::raw_sql(sql).execute(conn).await {
        Ok(_) => None,
        Err(e) => e
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned()),
    }
}

pub(super) const SEAL_GUARD_OFF: &str =
    "ALTER TABLE forge.audit_seal DISABLE TRIGGER audit_seal_append_only";
pub(super) const LOG_GUARD_OFF: &str =
    "ALTER TABLE forge.audit_log DISABLE TRIGGER audit_log_append_only";

#[tokio::test]
async fn a_tampered_sealed_column_is_audit_seal_mismatch_recompute() {
    let t = sealed_twice().await;
    verify_all(&t).await.expect("intact");
    t.exec_superuser(&format!(
        "{LOG_GUARD_OFF}; UPDATE forge.audit_log SET outcome = 'denied' WHERE request_id = 'r-2'"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("tampered");
    assert_mismatch(&err, "recompute", "rows_sha256");
    assert!(err.to_string().contains("seal_id="), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn redacting_the_personal_columns_keeps_the_seal_valid() {
    let t = sealed_twice().await;
    t.exec_superuser(
        "SET ROLE forge_purge; \
         UPDATE forge.audit_log SET actor_label = NULL, source_ip = NULL, user_agent = NULL \
         WHERE actor_label IS NOT NULL; RESET ROLE;",
    )
    .await;
    let redacted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE request_id LIKE 'r-%' AND actor_label IS NULL",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("count");
    assert_eq!(redacted, 5);
    verify_all(&t).await.expect("still valid");
    t.drop_now().await;
}

#[tokio::test]
async fn a_deleted_sealed_row_is_a_gap() {
    let t = sealed_twice().await;
    t.exec_superuser(&format!(
        "{LOG_GUARD_OFF}; DELETE FROM forge.audit_log WHERE request_id = 'r-1' AND seq = \
         (SELECT max(seq) FROM forge.audit_log WHERE request_id = 'r-1')"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("gap");
    assert_mismatch(&err, "gap", "missing_rows");
    assert!(err.to_string().contains("missing_seq="), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_broken_prev_seal_sha256_is_a_chain_mismatch() {
    let t = sealed_twice().await;
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET prev_seal_sha256 = sha256('x'::bytea) \
         WHERE id = (SELECT max(id) FROM forge.audit_seal)"
    ))
    .await;
    let err = verify_all(&t).await.expect_err("chain");
    assert_mismatch(&err, "chain", "prev_seal_sha256");
    t.drop_now().await;
}

#[tokio::test]
async fn a_rewritten_seal_value_is_a_recompute_mismatch_without_the_key() {
    let t = sealed_twice().await;
    // rows_sha256 rewritten, seal_sha256 and hmac left: the seal's own
    // fields no longer give its seal_sha256 (even without keys).
    t.exec_superuser(&format!(
        "{SEAL_GUARD_OFF}; UPDATE forge.audit_seal SET rows_sha256 = sha256('x'::bytea) \
         WHERE id = (SELECT max(id) FROM forge.audit_seal)"
    ))
    .await;
    let err = verify_range(&t.db, None, VerifyScope::SealedWithin(DOCTOR_WINDOW))
        .await
        .expect_err("recompute");
    assert_mismatch(&err, "recompute", "seal_sha256");
    t.drop_now().await;
}

#[tokio::test]
async fn a_wrong_key_is_an_hmac_mismatch() {
    let t = sealed_twice().await;
    let wrong = SealKeys::parse(&format!("1 {}", "43".repeat(32))).expect("keys");
    let err = verify_range(
        &t.db,
        Some(&wrong),
        VerifyScope::SealedWithin(DOCTOR_WINDOW),
    )
    .await
    .expect_err("hmac");
    assert_mismatch(&err, "hmac", "mismatch");
    assert!(err.to_string().contains("key_id=1"), "{err}");
    // A key file that lacks the seal's key_id: the HMAC is checked first.
    let rotated = SealKeys::parse(&format!("2 {}", "42".repeat(32))).expect("keys");
    let err = verify_range(
        &t.db,
        Some(&rotated),
        VerifyScope::SealedWithin(DOCTOR_WINDOW),
    )
    .await
    .expect_err("unknown key");
    assert_mismatch(&err, "hmac", "unknown_key_id");
    t.drop_now().await;
}

#[tokio::test]
async fn only_shipped_at_may_be_set_and_only_once() {
    let t = sealed_twice().await;
    let mut app = t.db.write_pool().acquire().await.expect("app");
    for (sql, expected) in [
        (
            "UPDATE forge.audit_seal SET shipped_at = now() WHERE first_seq = 1",
            None,
        ),
        // Once.
        (
            "UPDATE forge.audit_seal SET shipped_at = now() + interval '1 s' WHERE first_seq = 1",
            Some("XF001"),
        ),
        (
            "UPDATE forge.audit_seal SET shipped_at = NULL WHERE first_seq = 1",
            Some("XF001"),
        ),
        // forge_app has no privilege on the other columns, nor DELETE.
        (
            "UPDATE forge.audit_seal SET hmac = sha256('x'::bytea)",
            Some("42501"),
        ),
        ("DELETE FROM forge.audit_seal", Some("42501")),
        ("TRUNCATE forge.audit_seal", Some("42501")),
    ] {
        assert_eq!(state_of(&mut app, sql).await.as_deref(), expected, "{sql}");
    }
    drop(app);
    let mut su = t.superuser().await;
    for sql in [
        // Even the owner: one column besides shipped_at, deletes, truncate.
        "UPDATE forge.audit_seal SET shipped_at = now(), key_id = 2 \
         WHERE shipped_at IS NULL",
        "UPDATE forge.audit_seal SET kind = 'restore'",
        "DELETE FROM forge.audit_seal",
        "TRUNCATE forge.audit_seal",
    ] {
        assert_eq!(
            state_of(&mut su, sql).await.as_deref(),
            Some("XF001"),
            "{sql}"
        );
    }
    t.drop_now().await;
}

#[tokio::test]
async fn the_guard_accepts_only_seals_that_continue_the_chain() {
    let t = sealed_twice().await;
    let (last, hash): (i64, Vec<u8>) = sqlx::query_as(
        "SELECT last_seq, seal_sha256 FROM forge.audit_seal ORDER BY last_seq DESC LIMIT 1",
    )
    .fetch_one(t.db.pool().executor())
    .await
    .expect("last seal");
    insert_audit(&t, 2).await;
    number_all(&t).await;
    let insert = |kind: &str, first: i64, prev: &str, shipped: &str| {
        format!(
            "INSERT INTO forge.audit_seal (kind, hash_version, seal_epoch, key_id, first_seq, \
             last_seq, row_count, events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, \
             hmac{shipped_col}) VALUES ('{kind}', 1, 1, 1, {first}, {first}, 1, 0, \
             sha256('r'::bytea), {prev}, sha256('s'::bytea), sha256('h'::bytea){shipped})",
            shipped_col = if shipped.is_empty() {
                ""
            } else {
                ", shipped_at"
            },
        )
    };
    let prev = format!("'\\x{}'::bytea", super::golden::hex(&hash));
    let mut app = t.db.write_pool().acquire().await.expect("app");
    for (sql, why) in [
        (insert("normal", last + 2, &prev, ""), "skips a number"),
        (
            insert("normal", last + 1, "sha256('x'::bytea)", ""),
            "wrong prev",
        ),
        (
            insert("imported", last + 1, &prev, ""),
            "imported by forge_app",
        ),
        (
            insert("normal", last + 50, &prev, ""),
            "past audit_high_seq",
        ),
    ] {
        assert_eq!(
            state_of(&mut app, &sql).await.as_deref(),
            Some("XF001"),
            "{why}: {sql}"
        );
    }
    // forge_app cannot set shipped_at on insert at all (no column privilege).
    let shipped = insert("normal", last + 1, &prev, ", now()");
    assert_eq!(state_of(&mut app, &shipped).await.as_deref(), Some("42501"));
    let mut su = t.superuser().await;
    assert_eq!(state_of(&mut su, &shipped).await.as_deref(), Some("XF001"));
    // The continuing one is accepted (its values are nonsense: the guard
    // cannot recompute them, verification does).
    assert_eq!(
        state_of(&mut app, &insert("normal", last + 1, &prev, "")).await,
        None
    );
    drop(app);
    let err = verify_all(&t).await.expect_err("nonsense values");
    assert_reason(&err, ErrorCode::AuditSealMismatch, "hmac");
    t.drop_now().await;
}
