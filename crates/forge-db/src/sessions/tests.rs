//! Web sessions against a migrated database (M2 MR3, ADR-0021 §5):
//! issue, authenticate (every reason, the dummy hash), touch, revoke,
//! revoke all, the disable, and the table's audit enforcement and
//! privileges. Inspection uses a superuser connection (the read pool cannot
//! see session hashes).

use std::time::Duration;

use forge_core::ids::UserId;
use forge_core::{RequestId, SessionRevokeReason, UserKind};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::*;
use crate::audited_tests::{audit, commit, sql};
use crate::change::Actor;
use crate::jobs::runner_stop_tests::Captured;
use crate::test_support::*;
use crate::users::{self, AuthSource, NewUser};

const IDLE: Duration = Duration::from_secs(4 * 3600);

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

async fn human(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, "Someone", &format!("{name}@example.test")),
    )
    .await
    .expect("create user")
    .user_id
}

fn new_session(user: UserId) -> NewSession {
    NewSession {
        user,
        idle: IDLE,
        absolute: Duration::from_secs(12 * 3600),
        oidc: None,
    }
}

async fn issue_ok(t: &FreshDb, req: &str, user: UserId) -> Issued {
    issue(&t.db, &ctx(req), &new_session(user))
        .await
        .expect("issue")
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A superuser change of audited columns, with its audit row in the same
/// transaction (a simple-protocol batch is one transaction).
async fn su_change(su: &mut PgConnection, user: UserId, change: &str) {
    let batch = format!("{}; {change}", audit("user", user.get(), "succeeded"));
    sqlx::raw_sql(&batch)
        .execute(su)
        .await
        .unwrap_or_else(|e| panic!("{change}: {e}"));
}

fn secret_hex(issued: &Issued) -> String {
    issued
        .token
        .reveal()
        .rsplit('_')
        .next()
        .expect("secret part")
        .to_owned()
}

#[tokio::test]
async fn an_issued_session_authenticates_to_its_user_with_one_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let issued = issue_ok(&t, "req-sess-issue", alice).await;
    let cookie = issued.token.reveal();
    assert!(cookie.starts_with("gss_"));
    let identity = authenticate(&t.db, &rid("req-sess-auth"), &cookie, IDLE)
        .await
        .expect("authenticate");
    assert_eq!(identity.session_id, issued.session_id);
    assert_eq!(identity.user_id, alice);
    assert_eq!(identity.username, "alice");
    assert_eq!(identity.public_id, issued.token.public_id().as_str());
    assert!(!identity.is_admin);
    assert_eq!(identity.absolute_expires_at, issued.absolute_expires_at);
    let mut su = t.superuser().await;
    let rows: Vec<(String, String, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT action, resource_type, resource_id, details FROM forge.audit_log \
         WHERE request_id = 'req-sess-issue'",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "session.created");
    assert_eq!((rows[0].1.as_str(), rows[0].2), ("user", alice.get()));
    assert_eq!(
        rows[0].3,
        serde_json::json!({
            "session_id": issued.session_id,
            "user_id": alice.get(),
            "public_id": issued.token.public_id().as_str(),
        })
    );
    assert!(!rows[0].3.to_string().contains(&secret_hex(&issued)));
    // Only the hash is stored; no event is published (ADR-0021 §5).
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_sha256 FROM forge.web_sessions")
        .fetch_one(&mut su)
        .await
        .expect("hash");
    assert_eq!(stored, forge_core::token::sha256(issued.token.secret()));
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type LIKE 'session.%'"
        )
        .await,
        0
    );
    // 4 h idle within the 12 h absolute.
    let (idle, absolute): (f64, f64) = sqlx::query_as(
        "SELECT extract(epoch FROM idle_expires_at - created_at)::float8, \
         extract(epoch FROM absolute_expires_at - created_at)::float8 FROM forge.web_sessions",
    )
    .fetch_one(&mut su)
    .await
    .expect("lifetimes");
    assert!((idle - 14_400.0).abs() < 5.0, "{idle}");
    assert!((absolute - 43_200.0).abs() < 5.0, "{absolute}");
    t.drop_now().await;
}

/// The dummy hash is compared for an unknown public id (equal work for a
/// miss) and it matches no secret, not even the all-zero one.
#[test]
fn the_dummy_hash_matches_no_secret() {
    use forge_core::token::{verify, TokenSecret};
    for secret in [[0u8; 32], [0xffu8; 32], [1u8; 32]] {
        assert!(!verify(&auth::DUMMY_HASH, &TokenSecret::from_bytes(secret)));
    }
}

#[tokio::test]
async fn every_rejected_session_carries_its_reason_and_never_its_secret() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let valid = issue_ok(&t, "req-rej-1", alice).await;
    let absolute = issue_ok(&t, "req-rej-2", alice).await;
    let idle = issue_ok(&t, "req-rej-3", alice).await;
    let revoked = issue_ok(&t, "req-rej-4", alice).await;
    let mut su = t.superuser().await;
    su_change(
        &mut su,
        alice,
        &format!(
            "UPDATE forge.web_sessions SET created_at = now() - interval '13 hours', \
             idle_expires_at = now() - interval '1 hour', \
             absolute_expires_at = now() - interval '1 hour' WHERE id = {}",
            absolute.session_id
        ),
    )
    .await;
    sqlx::query(
        "UPDATE forge.web_sessions SET idle_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(idle.session_id)
    .execute(&mut su)
    .await
    .expect("idle expired");
    assert_eq!(
        revoke(
            &t.db,
            &ctx("req-rej-5"),
            revoked.session_id,
            SessionRevokeReason::Logout
        )
        .await
        .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let mismatch = format!(
        "gss_{}_{}",
        valid.token.public_id().as_str(),
        "a".repeat(64)
    );
    let unknown = forge_core::session_token::generate()
        .expect("generate")
        .reveal();
    let token_shaped = valid.token.reveal().replacen("gss_", "fgp_", 1);
    let cases = [
        (absolute.token.reveal(), "session_expired"),
        (idle.token.reveal(), "session_expired"),
        (revoked.token.reveal(), "session_revoked"),
        (mismatch, "session_mismatch"),
        (unknown, "session_unknown"),
        (String::new(), "session_malformed"),
        (token_shaped, "session_malformed"),
        (format!("{}x", valid.token.reveal()), "session_malformed"),
    ];
    for (presented, reason) in cases {
        let err = authenticate(&t.db, &rid("req-rej-6"), &presented, IDLE)
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::Unauthenticated, reason);
        assert!(!chain(&err).contains(&secret_hex(&valid)));
        assert!(presented.is_empty() || !chain(&err).contains(&presented));
    }
    let err = authenticate(&t.db, &rid("req-rej-7"), &revoked.token.reveal(), IDLE)
        .await
        .expect_err("revoked");
    assert!(chain(&err).contains("revoked_reason=logout"), "{err}");
    authenticate(&t.db, &rid("req-rej-8"), &valid.token.reveal(), IDLE)
        .await
        .expect("the valid one still works");
    t.drop_now().await;
}

/// ADR-0019 Amendment M2: the disable revokes the user's sessions in its own
/// change (one `user.disabled` row counting them, no session rows, no
/// events), and the next request with the cookie is `FG-AUTH-0004`.
#[tokio::test]
async fn disabling_a_user_revokes_their_sessions_in_the_same_change() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    let first = issue_ok(&t, "req-dis-1", alice).await;
    let second = issue_ok(&t, "req-dis-2", alice).await;
    let bobs = issue_ok(&t, "req-dis-3", bob).await;
    // An expired session is revoked too (review: no unrevoked row lingers
    // in the partial indexes).
    let mut su = t.superuser().await;
    let expired = issue_ok(&t, "req-dis-4", alice).await;
    su_change(
        &mut su,
        alice,
        &format!(
            "UPDATE forge.web_sessions SET created_at = now() - interval '13 hours', \
             idle_expires_at = now() - interval '1 hour', \
             absolute_expires_at = now() - interval '1 hour' WHERE id = {}",
            expired.session_id
        ),
    )
    .await;
    let outcome = users::disable(&t.db, &ctx("req-dis-5"), alice)
        .await
        .expect("disable");
    assert_eq!(outcome, users::DisableOutcome::Disabled);
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE request_id = 'req-dis-5' \
         AND action = 'user.disabled'",
    )
    .fetch_one(&mut su)
    .await
    .expect("details");
    assert_eq!(details["revoked_sessions"], 3);
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE request_id = 'req-dis-5'"
        )
        .await,
        1,
        "one audit row for the disable"
    );
    let reasons: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, revoked_reason FROM forge.web_sessions ORDER BY id")
            .fetch_all(&mut su)
            .await
            .expect("reasons");
    let reason_of = |id: i64| {
        reasons
            .iter()
            .find(|(i, _)| *i == id)
            .and_then(|(_, r)| r.clone())
    };
    assert_eq!(
        reason_of(first.session_id).as_deref(),
        Some("user_disabled")
    );
    assert_eq!(
        reason_of(second.session_id).as_deref(),
        Some("user_disabled")
    );
    assert_eq!(
        reason_of(expired.session_id).as_deref(),
        Some("user_disabled")
    );
    assert_eq!(reason_of(bobs.session_id), None, "another user's session");
    for issued in [&first, &second] {
        let err = authenticate(&t.db, &rid("req-dis-6"), &issued.token.reveal(), IDLE)
            .await
            .expect_err("disabled");
        assert_reason(&err, ErrorCode::AccountDisabled, "owner_disabled");
    }
    authenticate(&t.db, &rid("req-dis-7"), &bobs.token.reveal(), IDLE)
        .await
        .expect("bob is not affected");
    // A disabled user gets no new session.
    let err = issue(&t.db, &ctx("req-dis-8"), &new_session(alice))
        .await
        .expect_err("disabled");
    assert_reason(&err, ErrorCode::AccountDisabled, "owner_disabled");
    t.drop_now().await;
}

#[tokio::test]
async fn activity_moves_the_idle_expiry_at_most_once_a_minute_without_an_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let issued = issue_ok(&t, "req-touch-1", alice).await;
    let cookie = issued.token.reveal();
    let mut su = t.superuser().await;
    let audit_before = count(&mut su, "SELECT count(*) FROM forge.audit_log").await;
    let read = "SELECT last_seen_at::text || ' ' || idle_expires_at::text FROM forge.web_sessions";
    let fresh: String = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    authenticate(&t.db, &rid("req-touch-2"), &cookie, IDLE)
        .await
        .expect("use");
    let after: String = sqlx::query_scalar(read)
        .fetch_one(&mut su)
        .await
        .expect("read");
    assert_eq!(fresh, after, "not written again within a minute");
    sqlx::query(
        "UPDATE forge.web_sessions SET last_seen_at = now() - interval '2 minutes', \
         idle_expires_at = now() + interval '10 minutes'",
    )
    .execute(&mut su)
    .await
    .expect("age");
    let identity = authenticate(&t.db, &rid("req-touch-3"), &cookie, IDLE)
        .await
        .expect("use");
    let (seen_now, idle_left): (bool, f64) = sqlx::query_as(
        "SELECT last_seen_at > now() - interval '30 seconds', \
         extract(epoch FROM idle_expires_at - now())::float8 FROM forge.web_sessions",
    )
    .fetch_one(&mut su)
    .await
    .expect("read");
    assert!(seen_now);
    assert!(idle_left > 14_000.0, "moved to now + 4 h: {idle_left}");
    assert_ne!(identity.idle_expires_at, issued.absolute_expires_at);
    // Never past the absolute expiry.
    su_change(
        &mut su,
        alice,
        "UPDATE forge.web_sessions SET last_seen_at = now() - interval '2 minutes', \
         idle_expires_at = now() + interval '1 minute', \
         absolute_expires_at = now() + interval '30 minutes'",
    )
    .await;
    authenticate(&t.db, &rid("req-touch-4"), &cookie, IDLE)
        .await
        .expect("use");
    let capped: bool =
        sqlx::query_scalar("SELECT idle_expires_at = absolute_expires_at FROM forge.web_sessions")
            .fetch_one(&mut su)
            .await
            .expect("read");
    assert!(capped, "the idle expiry stops at the absolute one");
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.audit_log").await,
        audit_before + 1,
        "only the su_change's own audit row"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_failed_touch_is_logged_once_and_authentication_proceeds() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let issued = issue_ok(&t, "req-tfail-1", alice).await;
    let mut su = t.superuser().await;
    sqlx::query("UPDATE forge.web_sessions SET last_seen_at = now() - interval '2 minutes'")
        .execute(&mut su)
        .await
        .expect("age");
    t.exec_superuser("REVOKE UPDATE (last_seen_at) ON forge.web_sessions FROM forge_app")
        .await;
    let logs = Captured::default();
    let _guard = logs.install();
    let identity = authenticate(&t.db, &rid("req-tfail-2"), &issued.token.reveal(), IDLE)
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
async fn a_database_outage_is_db_unavailable_and_a_bad_hash_is_credential_data_invalid() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let issued = issue_ok(&t, "req-out-1", alice).await;
    let mut su = t.superuser().await;
    sqlx::query("ALTER TABLE forge.web_sessions DROP CONSTRAINT web_sessions_secret_sha256_len")
        .execute(&mut su)
        .await
        .expect("drop constraint");
    sqlx::query("ALTER TABLE forge.web_sessions DISABLE TRIGGER USER")
        .execute(&mut su)
        .await
        .expect("disable triggers");
    sqlx::query(
        "UPDATE forge.web_sessions SET secret_sha256 = substring(secret_sha256 from 1 for 31)",
    )
    .execute(&mut su)
    .await
    .expect("short hash");
    let err = authenticate(&t.db, &rid("req-out-2"), &issued.token.reveal(), IDLE)
        .await
        .expect_err("bad hash");
    assert_reason(&err, ErrorCode::CredentialDataInvalid, "bad_hash");
    assert!(chain(&err).contains(&format!("session_id={}", issued.session_id)));
    t.db.close().await;
    let err = authenticate(&t.db, &rid("req-out-3"), &issued.token.reveal(), IDLE)
        .await
        .expect_err("outage");
    assert_eq!(err.code(), ErrorCode::DbUnavailable, "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn revoke_and_revoke_all_write_one_audit_row_each_and_are_idempotent() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let one = issue_ok(&t, "req-rev-1", alice).await;
    let two = issue_ok(&t, "req-rev-2", alice).await;
    let three = issue_ok(&t, "req-rev-3", alice).await;
    assert_eq!(
        revoke(
            &t.db,
            &ctx("req-rev-4"),
            one.session_id,
            SessionRevokeReason::Backchannel
        )
        .await
        .expect("revoke"),
        RevokeOutcome::Revoked
    );
    assert_eq!(
        revoke(
            &t.db,
            &ctx("req-rev-5"),
            one.session_id,
            SessionRevokeReason::Logout
        )
        .await
        .expect("again"),
        RevokeOutcome::AlreadyEnded
    );
    let err = revoke(
        &t.db,
        &ctx("req-rev-6"),
        999_999,
        SessionRevokeReason::Admin,
    )
    .await
    .expect_err("unknown");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let revoked = revoke_all(&t.db, &ctx("req-rev-7"), alice, SessionRevokeReason::Admin)
        .await
        .expect("all");
    assert_eq!(revoked, 2);
    assert_eq!(
        revoke_all(&t.db, &ctx("req-rev-8"), alice, SessionRevokeReason::Admin)
            .await
            .expect("none left"),
        0
    );
    let mut su = t.superuser().await;
    let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT request_id, action, details FROM forge.audit_log \
         WHERE action LIKE 'session.revoked%' ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "req-rev-4");
    assert_eq!(rows[0].2["reason"], "backchannel");
    assert_eq!(rows[0].2["public_id"], one.token.public_id().as_str());
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1.as_str()),
        ("req-rev-7", "session.revoked_all")
    );
    assert_eq!(rows[1].2["revoked_sessions"], 2);
    for (issued, reason) in [(&one, "backchannel"), (&two, "admin"), (&three, "admin")] {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT revoked_reason FROM forge.web_sessions WHERE id = $1")
                .bind(issued.session_id)
                .fetch_one(&mut su)
                .await
                .expect("reason");
        assert_eq!(stored.as_deref(), Some(reason));
    }
    t.drop_now().await;
}

#[tokio::test]
async fn issue_refuses_unknown_users_bots_and_out_of_range_lifetimes() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let err = issue(&t.db, &ctx("req-ref-1"), &new_session(UserId::new(999_999)))
        .await
        .expect_err("unknown");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let bot = users::create(
        &t.db,
        &ctx("req-ref-bot"),
        &NewUser {
            kind: UserKind::Bot,
            username: "builder".to_owned(),
            display_name: "Bot".to_owned(),
            email: None,
            employee_number: None,
            external_id: None,
            auth_source: AuthSource::Local,
            is_admin: false,
        },
    )
    .await
    .expect("bot")
    .user_id;
    let err = issue(&t.db, &ctx("req-ref-2"), &new_session(bot))
        .await
        .expect_err("bot");
    assert_reason(&err, ErrorCode::Internal, "session_for_non_human");
    for (idle, absolute) in [(30, 3600), (7200, 3600), (3600, 25 * 3600)] {
        let new = NewSession {
            idle: Duration::from_secs(idle),
            absolute: Duration::from_secs(absolute),
            ..new_session(alice)
        };
        let err = issue(&t.db, &ctx("req-ref-3"), &new)
            .await
            .expect_err("lifetime");
        assert_reason(&err, ErrorCode::Internal, "session_lifetime_out_of_range");
    }
    let mut su = t.superuser().await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.web_sessions").await,
        0,
        "nothing was written"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_oidc_session_stores_its_issuer_sid_and_subject() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let new = NewSession {
        oidc: Some(OidcSession {
            issuer: "https://sso.example.test/realms/iot".to_owned(),
            subject: "synthetic-sub-1".to_owned(),
            sid: Some("synthetic-sid-1".to_owned()),
            refresh_token_enc: Some(vec![1, 2, 3]),
            id_token_enc: Some(vec![4, 5]),
        }),
        ..new_session(alice)
    };
    let issued = issue(&t.db, &ctx("req-oidc-1"), &new).await.expect("issue");
    let mut su = t.superuser().await;
    let row: (String, String, String, Vec<u8>, bool) = sqlx::query_as(
        "SELECT oidc_issuer, oidc_sid, oidc_sub, refresh_token_enc, \
         refresh_checked_at IS NOT NULL AND access_role_seen_at IS NOT NULL \
         FROM forge.web_sessions WHERE id = $1",
    )
    .bind(issued.session_id)
    .fetch_one(&mut su)
    .await
    .expect("row");
    assert_eq!(row.0, "https://sso.example.test/realms/iot");
    assert_eq!(
        (row.1.as_str(), row.2.as_str()),
        ("synthetic-sid-1", "synthetic-sub-1")
    );
    assert_eq!(row.3, vec![1, 2, 3]);
    assert!(row.4, "the login is the first check");
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-oidc-1'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert!(
        !details.to_string().contains("synthetic-sub-1"),
        "the subject is not audited"
    );
    let too_long = NewSession {
        oidc: Some(OidcSession {
            issuer: "i".repeat(256),
            subject: "s".to_owned(),
            sid: None,
            refresh_token_enc: None,
            id_token_enc: None,
        }),
        ..new_session(alice)
    };
    let err = issue(&t.db, &ctx("req-oidc-2"), &too_long)
        .await
        .expect_err("too long");
    assert_reason(&err, ErrorCode::Internal, "session_oidc_out_of_range");
    // Revoking drops the sealed IdP tokens (review MEDIUM).
    assert_eq!(
        revoke(
            &t.db,
            &ctx("req-oidc-3"),
            issued.session_id,
            SessionRevokeReason::Logout
        )
        .await
        .expect("revoke"),
        RevokeOutcome::Revoked
    );
    let (refresh, id): (Option<Vec<u8>>, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT refresh_token_enc, id_token_enc FROM forge.web_sessions WHERE id = $1",
    )
    .bind(issued.session_id)
    .fetch_one(&mut su)
    .await
    .expect("row");
    assert_eq!((refresh, id), (None, None));
    t.drop_now().await;
}

/// `revoke_all` (and the disable) drop the sealed IdP tokens of every
/// session they revoke.
#[tokio::test]
async fn revoke_all_drops_the_sealed_idp_tokens() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let oidc = || OidcSession {
        issuer: "https://sso.example.test/realms/iot".to_owned(),
        subject: "synthetic-sub-2".to_owned(),
        sid: None,
        refresh_token_enc: Some(vec![9]),
        id_token_enc: Some(vec![8]),
    };
    for req in ["req-drop-1", "req-drop-2"] {
        let new = NewSession {
            oidc: Some(oidc()),
            ..new_session(alice)
        };
        issue(&t.db, &ctx(req), &new).await.expect("issue");
    }
    let revoked = revoke_all(&t.db, &ctx("req-drop-3"), alice, SessionRevokeReason::Admin)
        .await
        .expect("all");
    assert_eq!(revoked, 2);
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.web_sessions \
             WHERE refresh_token_enc IS NOT NULL OR id_token_enc IS NOT NULL"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// A touch that races a revocation still open in another transaction waits
/// for its row lock, then finds the row revoked and changes nothing; the
/// request that read the session before the revocation completes (the
/// accepted in-flight risk, ADR-0019), the next one is `session_revoked`.
#[tokio::test]
async fn a_touch_that_races_an_open_revoke_changes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let issued = issue_ok(&t, "req-race-1", alice).await;
    let mut su = t.superuser().await;
    sqlx::query("UPDATE forge.web_sessions SET last_seen_at = now() - interval '5 minutes'")
        .execute(&mut su)
        .await
        .expect("age");
    let before: String = sqlx::query_scalar("SELECT last_seen_at::text FROM forge.web_sessions")
        .fetch_one(&mut su)
        .await
        .expect("read");
    let held = t
        .hold_audited(
            ("user", alice.get()),
            &[format!(
                "UPDATE forge.web_sessions SET revoked_at = now(), revoked_reason = 'logout' \
                 WHERE id = {}",
                issued.session_id
            )],
        )
        .await;
    let db = t.db.clone();
    let cookie = issued.token.reveal();
    let racing =
        tokio::spawn(async move { authenticate(&db, &rid("req-race-2"), &cookie, IDLE).await });
    t.until_waiting("SET last_seen_at = now()").await;
    held.commit().await;
    let identity = racing
        .await
        .expect("join")
        .expect("read before the revocation");
    assert_eq!(identity.session_id, issued.session_id);
    let (after, reason): (String, Option<String>) =
        sqlx::query_as("SELECT last_seen_at::text, revoked_reason FROM forge.web_sessions")
            .fetch_one(&mut su)
            .await
            .expect("read");
    assert_eq!(after, before, "the touch found the row revoked");
    assert_eq!(reason.as_deref(), Some("logout"));
    let err = authenticate(&t.db, &rid("req-race-3"), &issued.token.reveal(), IDLE)
        .await
        .expect_err("revoked");
    assert_reason(&err, ErrorCode::Unauthenticated, "session_revoked");
    t.drop_now().await;
}

// ── The tables themselves (migration 20261103100000) ──

fn insert_session(public_id: &str) -> String {
    format!(
        "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
         absolute_expires_at, oidc_issuer, oidc_sub, refresh_token_enc, id_token_enc) \
         VALUES ('{public_id}', decode(repeat('ab', 32), 'hex'), 1, now() + interval '1 hour', \
         now() + interval '2 hours', 'https://sso.example.test', 'synthetic-sub', '\\x01', '\\x02')"
    )
}

async fn seed_user(t: &FreshDb) {
    commit(
        t,
        &[
            audit("user", 1, "succeeded"),
            sql(
                "INSERT INTO forge.users (id, kind, username, display_name, email) \
                 OVERRIDING SYSTEM VALUE VALUES (1, 'human', 'alice', 'A', 'alice@example.test')",
            ),
        ],
    )
    .await
    .expect("seed user");
}

#[tokio::test]
async fn a_session_change_without_its_audit_row_cannot_commit_but_activity_can() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    let add = insert_session("00000000000000aa");
    let err = commit(&t, std::slice::from_ref(&sql(&add)))
        .await
        .expect_err("no audit row");
    crate::audited_tests::assert_refused(&err, "web_sessions");
    commit(&t, &[audit("user", 1, "succeeded"), sql(&add)])
        .await
        .expect("with its audit row");
    let revoke_sql = "UPDATE forge.web_sessions SET revoked_at = now(), revoked_reason = 'logout'";
    let err = commit(&t, &[sql(revoke_sql)]).await.expect_err("revoke");
    crate::audited_tests::assert_refused(&err, "web_sessions");
    for unaudited in [
        "UPDATE forge.web_sessions SET last_seen_at = now(), idle_expires_at = now() + interval '5 minutes'",
        "UPDATE forge.web_sessions SET refresh_token_enc = '\\x03', id_token_enc = '\\x04', \
         refresh_checked_at = now(), access_role_seen_at = now()",
    ] {
        commit(&t, &[sql(unaudited)]).await.expect(unaudited);
    }
    commit(&t, &[audit("user", 1, "succeeded"), sql(revoke_sql)])
        .await
        .expect("revoke with its audit row");
    t.drop_now().await;
}

/// ADR-0021 §4–§5: the read pool never sees session hashes, the sealed IdP
/// tokens, the session's subject or an identity's subject.
#[tokio::test]
async fn the_read_pool_cannot_read_session_secrets_or_subjects() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(&insert_session("00000000000000bb")),
            sql(
                "INSERT INTO forge.user_identities (user_id, issuer, subject) \
                 VALUES (1, 'https://sso.example.test', 'synthetic-sub')",
            ),
        ],
    )
    .await
    .expect("seed");
    let ro = t.db.pool().executor();
    for query in [
        "SELECT secret_sha256 FROM forge.web_sessions",
        "SELECT refresh_token_enc FROM forge.web_sessions",
        "SELECT id_token_enc FROM forge.web_sessions",
        "SELECT oidc_sub FROM forge.web_sessions",
        "SELECT * FROM forge.web_sessions",
        "SELECT subject FROM forge.user_identities",
        "SELECT * FROM forge.user_identities",
    ] {
        let err = sqlx::query(query).fetch_all(ro).await.expect_err(query);
        assert!(
            err.to_string().contains("permission denied"),
            "{query}: {err}"
        );
    }
    let sessions = sqlx::query(
        "SELECT id, public_id, user_id, created_at, last_seen_at, idle_expires_at, \
         absolute_expires_at, revoked_at, revoked_reason, oidc_issuer, oidc_sid, \
         refresh_checked_at, access_role_seen_at FROM forge.web_sessions",
    )
    .fetch_all(ro)
    .await
    .expect("the other columns are readable");
    assert_eq!(sessions.len(), 1);
    let identities = sqlx::query(
        "SELECT id, user_id, issuer, linked_at, last_login_at FROM forge.user_identities",
    )
    .fetch_all(ro)
    .await
    .expect("the other columns are readable");
    assert_eq!(identities.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn the_app_role_cannot_delete_sessions_or_rewrite_their_audited_columns() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(&insert_session("00000000000000cc")),
            sql(
                "INSERT INTO forge.user_identities (user_id, issuer, subject) \
                 VALUES (1, 'https://sso.example.test', 'synthetic-sub')",
            ),
        ],
    )
    .await
    .expect("seed");
    // M2 MR6 (migration 20261104100000): forge_app may DELETE, but only a
    // session 30 days past its end (the purge); a live one is refused.
    let err = commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql("DELETE FROM forge.web_sessions"),
        ],
    )
    .await
    .expect_err("a live session");
    assert!(chain(&err).contains("30 days after it ended"), "{err}");
    for change in [
        "UPDATE forge.web_sessions SET secret_sha256 = decode(repeat('cd', 32), 'hex')",
        "UPDATE forge.web_sessions SET user_id = 2",
        "UPDATE forge.web_sessions SET absolute_expires_at = absolute_expires_at + interval '1 hour'",
        "UPDATE forge.web_sessions SET oidc_sub = 'other'",
        "DELETE FROM forge.user_identities",
        "UPDATE forge.user_identities SET subject = 'other'",
        "UPDATE forge.user_identities SET user_id = 2",
    ] {
        let err = commit(&t, &[audit("user", 1, "succeeded"), sql(change)])
            .await
            .expect_err(change);
        assert!(chain(&err).contains("permission denied"), "{change}: {err}");
    }
    commit(
        &t,
        &[sql(
            "UPDATE forge.user_identities SET last_login_at = now()",
        )],
    )
    .await
    .expect("last_login_at is unaudited");
    t.drop_now().await;
}

#[tokio::test]
async fn the_session_checks_refuse_what_the_contract_refuses() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    let cases = [
        (
            "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
             absolute_expires_at) VALUES ('00000000000000dd', decode(repeat('ab', 32), 'hex'), 1, \
             now() + interval '1 hour', now() + interval '25 hours')",
            "web_sessions_expiry_range",
        ),
        (
            "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
             absolute_expires_at) VALUES ('00000000000000dd', decode(repeat('ab', 32), 'hex'), 1, \
             now() + interval '3 hours', now() + interval '2 hours')",
            "web_sessions_expiry_range",
        ),
        (
            "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
             absolute_expires_at, revoked_at, revoked_reason) VALUES ('00000000000000dd', \
             decode(repeat('ab', 32), 'hex'), 1, now() + interval '1 hour', \
             now() + interval '2 hours', now(), 'revoked')",
            "web_sessions_revoked_reason",
        ),
        (
            "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
             absolute_expires_at, oidc_issuer) VALUES ('00000000000000dd', \
             decode(repeat('ab', 32), 'hex'), 1, now() + interval '1 hour', \
             now() + interval '2 hours', 'https://sso.example.test')",
            "web_sessions_oidc_pair",
        ),
        (
            "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, idle_expires_at, \
             absolute_expires_at) VALUES ('NOT-HEX', decode(repeat('ab', 32), 'hex'), 1, \
             now() + interval '1 hour', now() + interval '2 hours')",
            "web_sessions_public_id_hex",
        ),
    ];
    for (insert, constraint) in cases {
        let err = commit(&t, &[audit("user", 1, "succeeded"), sql(insert)])
            .await
            .expect_err(constraint);
        assert!(chain(&err).contains(constraint), "{constraint}: {err}");
    }
    t.drop_now().await;
}

/// Migration 20261103100100: a revocation is final for every role (the
/// guard is enabled ALWAYS), and an IdP session id needs its issuer.
#[tokio::test]
async fn a_revoked_session_stays_revoked_with_its_reason() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(&insert_session("00000000000000ee")),
        ],
    )
    .await
    .expect("seed");
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql("UPDATE forge.web_sessions SET revoked_at = now(), revoked_reason = 'logout'"),
        ],
    )
    .await
    .expect("revoke");
    for change in [
        "UPDATE forge.web_sessions SET revoked_at = NULL, revoked_reason = NULL",
        "UPDATE forge.web_sessions SET revoked_reason = 'admin'",
        "UPDATE forge.web_sessions SET revoked_at = now() + interval '1 hour'",
    ] {
        let err = commit(&t, &[audit("user", 1, "succeeded"), sql(change)])
            .await
            .expect_err(change);
        assert!(chain(&err).contains("stays revoked"), "{change}: {err}");
    }
    // Not even a superuser un-revokes it.
    let mut su = t.superuser().await;
    let err = sqlx::raw_sql(&format!(
        "{}; UPDATE forge.web_sessions SET revoked_at = NULL, revoked_reason = NULL",
        audit("user", 1, "succeeded")
    ))
    .execute(&mut su)
    .await
    .expect_err("superuser");
    assert!(err.to_string().contains("stays revoked"), "{err}");
    // The unaudited columns still change (a racing touch is refused by its
    // own WHERE, not by the guard).
    commit(
        &t,
        &[sql(
            "UPDATE forge.web_sessions SET refresh_checked_at = now()",
        )],
    )
    .await
    .expect("unaudited column");
    let err = commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(
                "INSERT INTO forge.web_sessions (public_id, secret_sha256, user_id, \
                 idle_expires_at, absolute_expires_at, oidc_sid) VALUES ('00000000000000ef', \
                 decode(repeat('ab', 32), 'hex'), 1, now() + interval '1 hour', \
                 now() + interval '2 hours', 'synthetic-sid')",
            ),
        ],
    )
    .await
    .expect_err("sid without issuer");
    assert!(
        chain(&err).contains("web_sessions_oidc_sid_needs_issuer"),
        "{err}"
    );
    t.drop_now().await;
}
