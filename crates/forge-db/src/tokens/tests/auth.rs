//! tokens::authenticate: identity, every rejection reason, outage, the
//! `last_used_at` throttle and unreadable stored data.

use super::*;
use crate::jobs::runner_stop_tests::Captured;

#[tokio::test]
async fn issued_token_authenticates_to_its_owner_with_its_scopes() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(
        &t,
        "req-auth-1",
        alice,
        &[Scope::ReadApi, Scope::WriteRepository],
    )
    .await;
    let identity = authenticate(&t.db, &rid("req-auth-2"), &issued.token.reveal())
        .await
        .expect("authenticate");
    assert_eq!(
        identity,
        TokenIdentity {
            token_id: issued.token_id,
            user_id: alice,
            is_admin: false,
            scopes: vec![Scope::ReadApi, Scope::WriteRepository],
            is_agent: false,
        }
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_agent_token_authenticates_with_is_agent_set() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let new = NewToken {
        is_agent: true,
        ..request(alice, &[Scope::ReadApi])
    };
    let issued = issue(&t.db, &ctx("req-agent-1"), &new)
        .await
        .expect("issue");
    let identity = authenticate(&t.db, &rid("req-agent-2"), &issued.token.reveal())
        .await
        .expect("authenticate");
    assert!(identity.is_agent);
    let mut su = t.superuser().await;
    let events = events_of(&mut su, "token.created").await;
    assert_eq!(events[0].3["is_agent"], true);
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE action = 'token.created'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(details["is_agent"], true);
    t.drop_now().await;
}

#[tokio::test]
async fn expired_revoked_mismatched_unknown_and_malformed_tokens_are_unauthenticated_with_their_reason(
) {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let valid = issue_ok(&t, "req-rej-1", alice, &[Scope::ReadApi]).await;
    let expired = issue_ok(&t, "req-rej-2", alice, &[Scope::ReadApi]).await;
    let revoked = issue_ok(&t, "req-rej-3", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    su_change(
        &mut su,
        alice,
        &format!(
            "UPDATE forge.tokens SET created_at = now() - interval '10 days', \
             expires_at = now() - interval '1 day' WHERE id = {}",
            expired.token_id
        ),
    )
    .await;
    assert_eq!(
        revoke(&t.db, &ctx("req-rej-4"), revoked.token_id)
            .await
            .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let mismatch = format!(
        "fgp_{}_{}",
        valid.token.public_id().as_str(),
        "a".repeat(64)
    );
    let unknown = forge_core::token::generate().expect("generate").reveal();
    let cases = [
        (expired.token.reveal(), "expired"),
        (revoked.token.reveal(), "revoked"),
        (mismatch, "mismatch"),
        (unknown, "unknown"),
        (String::new(), "malformed"),
        ("not-a-token".to_owned(), "malformed"),
        ("fgp_zz_yy".to_owned(), "malformed"),
        (format!("{}x", valid.token.reveal()), "malformed"),
    ];
    for (presented, reason) in cases {
        let err = authenticate(&t.db, &rid("req-rej-5"), &presented)
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::Unauthenticated, reason);
        assert_eq!(err.code().http_status(), 401);
        assert!(
            !chain(&err).contains(&secret_hex(&valid)),
            "no secret in the error"
        );
        assert!(
            presented.is_empty() || !chain(&err).contains(&presented),
            "the presented token is not echoed: {err}"
        );
    }
    // The valid one still works.
    authenticate(&t.db, &rid("req-rej-6"), &valid.token.reveal())
        .await
        .expect("valid");
    t.drop_now().await;
}

#[tokio::test]
async fn an_active_token_of_a_disabled_owner_is_account_disabled() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-inactive-1", alice, &[Scope::ReadApi]).await;
    // A disabled owner whose token is still active (written outside
    // users::disable, which revokes them): authenticate itself refuses it,
    // so no caller can forget the check.
    let mut su = t.superuser().await;
    sqlx::query("ALTER TABLE forge.users DISABLE TRIGGER USER")
        .execute(&mut su)
        .await
        .expect("disable triggers");
    sqlx::query("UPDATE forge.users SET state = 'disabled' WHERE id = $1")
        .bind(alice.get())
        .execute(&mut su)
        .await
        .expect("disable owner");
    let err = authenticate(&t.db, &rid("req-inactive-2"), &issued.token.reveal())
        .await
        .expect_err("a disabled owner");
    assert_reason(&err, ErrorCode::AccountDisabled, "owner_disabled");
    assert!(
        chain(&err).contains(&format!("user={}", alice.get())),
        "{err}"
    );
    assert!(
        chain(&err).contains(issued.token.public_id().as_str()),
        "{err}"
    );
    let used: Option<String> = sqlx::query_scalar("SELECT last_used_at::text FROM forge.tokens")
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert!(used.is_none(), "a refused use is not recorded");
    t.drop_now().await;
}

#[tokio::test]
async fn a_stored_hash_that_is_not_32_bytes_is_credential_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-hash-1", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    sqlx::query("ALTER TABLE forge.tokens DROP CONSTRAINT tokens_secret_sha256_len")
        .execute(&mut su)
        .await
        .expect("drop constraint");
    sqlx::query("ALTER TABLE forge.tokens DISABLE TRIGGER USER")
        .execute(&mut su)
        .await
        .expect("disable triggers");
    sqlx::query("UPDATE forge.tokens SET secret_sha256 = substring(secret_sha256 from 1 for 31)")
        .execute(&mut su)
        .await
        .expect("short hash");
    let err = authenticate(&t.db, &rid("req-hash-2"), &issued.token.reveal())
        .await
        .expect_err("bad hash");
    assert_reason(&err, ErrorCode::CredentialDataInvalid, "bad_hash");
    assert!(
        chain(&err).contains(&format!("token_id={}", issued.token_id)),
        "{err}"
    );
    assert!(
        chain(&err).contains(issued.token.public_id().as_str()),
        "{err}"
    );
    assert!(!chain(&err).contains(&secret_hex(&issued)));
    t.drop_now().await;
}

#[tokio::test]
async fn a_database_outage_during_lookup_is_db_unavailable_not_unauthenticated() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-outage-1", alice, &[Scope::ReadApi]).await;
    t.db.close().await;
    let err = authenticate(&t.db, &rid("req-outage-2"), &issued.token.reveal())
        .await
        .expect_err("outage");
    assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
    assert_eq!(err.code().http_status(), 503);
    t.drop_now().await;
}

#[tokio::test]
async fn last_used_at_is_written_at_most_once_per_10_minutes() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-used-1", alice, &[Scope::ReadApi]).await;
    let presented = issued.token.reveal();
    let mut su = t.superuser().await;
    let read = "SELECT last_used_at::text FROM forge.tokens";
    let before: Option<String> = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert!(before.is_none(), "never used yet");

    authenticate(&t.db, &rid("req-used-2"), &presented)
        .await
        .expect("first use");
    let first: Option<String> = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert!(first.is_some(), "the first use is recorded");

    authenticate(&t.db, &rid("req-used-3"), &presented)
        .await
        .expect("second use");
    let second: Option<String> = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert_eq!(first, second, "not written again within 10 minutes");

    // 9 minutes ago: still throttled. 11 minutes ago: written again.
    sqlx::query("UPDATE forge.tokens SET last_used_at = now() - interval '9 minutes'")
        .execute(&mut su)
        .await
        .expect("age 9");
    let nine: Option<String> = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    authenticate(&t.db, &rid("req-used-4"), &presented)
        .await
        .expect("use");
    let after_nine: Option<String> = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert_eq!(nine, after_nine);
    sqlx::query("UPDATE forge.tokens SET last_used_at = now() - interval '11 minutes'")
        .execute(&mut su)
        .await
        .expect("age 11");
    authenticate(&t.db, &rid("req-used-5"), &presented)
        .await
        .expect("use");
    let fresh: bool =
        sqlx::query_scalar("SELECT last_used_at > now() - interval '1 minute' FROM forge.tokens")
            .fetch_one(&mut su)
            .await
            .expect("fresh");
    assert!(fresh, "written again after 10 minutes");
    t.drop_now().await;
}

#[tokio::test]
async fn a_last_used_at_change_needs_no_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-noaudit-1", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    let before = count(&mut su, "SELECT count(*) FROM forge.audit_log").await;
    authenticate(&t.db, &rid("req-noaudit-2"), &issued.token.reveal())
        .await
        .expect("authenticate");
    let used = count(
        &mut su,
        "SELECT count(*) FROM forge.tokens WHERE last_used_at IS NOT NULL",
    )
    .await;
    assert_eq!(used, 1, "last_used_at was written");
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.audit_log").await,
        before,
        "without an audit row"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_last_used_at_failure_is_logged_once_and_authentication_proceeds() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-fail-1", alice, &[Scope::ReadApi]).await;
    t.exec_superuser("REVOKE UPDATE (last_used_at) ON forge.tokens FROM forge_app")
        .await;
    let logs = Captured::default();
    let _guard = logs.install();
    let identity = authenticate(&t.db, &rid("req-fail-2"), &issued.token.reveal())
        .await
        .expect("authentication proceeds");
    assert_eq!(identity.user_id, alice);
    let failures = logs.failures();
    assert_eq!(failures.len(), 1, "logged once: {failures:?}");
    assert_eq!(failures[0]["fields"]["error_code"], "FG-DB-0002");
    assert_eq!(failures[0]["level"], "WARN");
    let text = failures[0].to_string();
    assert!(!text.contains(&issued.token.reveal()) && !text.contains(&secret_hex(&issued)));
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_stored_scope_is_credential_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice", false).await;
    let issued = issue_ok(&t, "req-scope-1", alice, &[Scope::ReadApi]).await;
    let mut su = t.superuser().await;
    sqlx::query("ALTER TABLE forge.tokens DROP CONSTRAINT tokens_scopes_known")
        .execute(&mut su)
        .await
        .expect("drop constraint");
    su_change(
        &mut su,
        alice,
        "UPDATE forge.tokens SET scopes = ARRAY['teleport']",
    )
    .await;
    let err = authenticate(&t.db, &rid("req-scope-2"), &issued.token.reveal())
        .await
        .expect_err("unknown scope");
    assert_reason(&err, ErrorCode::CredentialDataInvalid, "unknown_scope");
    assert_eq!(err.code().http_status(), 500);
    assert!(!chain(&err).contains(&secret_hex(&issued)));
    t.drop_now().await;
}
