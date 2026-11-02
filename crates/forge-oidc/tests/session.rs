//! The running session against the mock provider and a migrated database
//! (M2 MR5, ADR-0021 §7, §8): the 5-minute refresh check (rotation, the
//! role confirmed again, `invalid_grant`, a removed role, a changed
//! subject, answers without an ID token, an unreachable provider), the
//! authenticator wrapper, the RP logout and the back-channel logout (every
//! logout token failure, `sid` and `sub`, the replay).
#![allow(clippy::expect_used, clippy::panic)] // tests fail loudly

use std::sync::Arc;
use std::time::Duration;

use forge_core::authenticator::SessionIdentity;
use forge_core::web_keys::KeyPurpose;
use forge_core::{RequestId, SessionAuthenticator, SessionRevokeReason};
use forge_db::sessions::SessionStore;
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::test_support::{web_keys, Endpoint, Fault, MockOp, RefreshMode};
use forge_oidc::{
    CallbackInput, LoginDone, LogoutRequest, Oidc, RefreshVerdict, RefreshingSessions,
};
use serde_json::{json, Value};

const BASE: &str = "http://localhost:8080";
const IDLE: Duration = Duration::from_secs(4 * 3600);

fn rid() -> RequestId {
    RequestId::generate()
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

async fn sign_in(oidc: &Oidc, op: &MockOp) -> LoginDone {
    let start = oidc.start(None).await.expect("start");
    let (code, state) = op.authorize(&start.location, json!({}));
    let request_id = rid();
    oidc.complete(CallbackInput {
        request_id: &request_id,
        code: Some(&code),
        state: Some(&state),
        error: None,
        state_cookie: Some(&start.state_cookie),
        session_cookie: None,
        source_ip: None,
        user_agent: None,
    })
    .await
    .expect("signed in")
}

struct World {
    t: FreshDb,
    op: MockOp,
    oidc: Arc<Oidc>,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = Arc::new(Oidc::new(op.settings(BASE), &web_keys(), t.db.clone()).expect("oidc"));
    World { t, op, oidc }
}

impl World {
    async fn row(&self, done: &LoginDone, columns: &str) -> Value {
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT {columns} FROM forge.web_sessions WHERE public_id = '{}'",
                done.public_id
            ))
            .await;
        rows.into_iter().next().expect("the session row")
    }

    /// Move the last check back past 5 minutes (unaudited) and
    /// authenticate as a request would.
    async fn due(&self, done: &LoginDone) -> SessionIdentity {
        self.t
            .exec_superuser(&format!(
                "UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '301 seconds' \
                 WHERE public_id = '{}'",
                done.public_id
            ))
            .await;
        let identity =
            forge_db::sessions::authenticate(&self.t.db, &rid(), &done.token.reveal(), IDLE)
                .await
                .expect("authenticated");
        assert!(identity.refresh_due);
        identity
    }

    async fn check(&self, done: &LoginDone) -> RefreshVerdict {
        let identity = self.due(done).await;
        self.oidc
            .refresh_check(&rid(), &identity)
            .await
            .expect("the check ran")
    }

    async fn revoked_reason(&self, done: &LoginDone) -> Value {
        self.row(done, "revoked_reason").await["revoked_reason"].clone()
    }
}

fn unhex(v: &Value) -> Vec<u8> {
    let hex = v.as_str().expect("bytea").trim_start_matches("\\x");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

#[tokio::test]
async fn a_refresh_with_an_id_token_rotates_the_tokens_and_continues() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let before = w.row(&done, "absolute_expires_at").await;
    assert_eq!(w.check(&done).await, RefreshVerdict::Continue);
    let row = w
        .row(
            &done,
            "refresh_token_enc, refresh_without_id_token, revoked_at, absolute_expires_at",
        )
        .await;
    let refresh = web_keys().purpose(KeyPurpose::RefreshToken).expect("keys");
    let plain = refresh
        .open(done.public_id.as_bytes(), &unhex(&row["refresh_token_enc"]))
        .expect("opens");
    assert_eq!(
        plain, b"synthetic-refresh-token-1",
        "the rotated token is kept"
    );
    assert_eq!(row["refresh_without_id_token"], 0);
    assert!(row["revoked_at"].is_null());
    assert_eq!(
        row["absolute_expires_at"], before["absolute_expires_at"],
        "the absolute lifetime never moves"
    );
    // Not due again right away: the provider is not called.
    let hits = w.op.hits(Endpoint::Token);
    let identity = forge_db::sessions::authenticate(&w.t.db, &rid(), &done.token.reveal(), IDLE)
        .await
        .expect("authenticated");
    assert!(!identity.refresh_due);
    assert_eq!(w.op.hits(Endpoint::Token), hits);
    w.t.drop_now().await;
}

#[tokio::test]
async fn invalid_grant_revokes_the_session_as_idp_revoked() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    w.op.revoke_refresh_tokens();
    assert_eq!(
        w.check(&done).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    assert_eq!(w.revoked_reason(&done).await, "idp_revoked");
    let row = w.row(&done, "refresh_token_enc, id_token_enc").await;
    assert!(row["refresh_token_enc"].is_null() && row["id_token_enc"].is_null());
    let err = forge_db::sessions::authenticate(&w.t.db, &rid(), &done.token.reveal(), IDLE)
        .await
        .expect_err("revoked");
    assert_eq!(reason(&err), "session_revoked");
    w.t.drop_now().await;
}

/// A provider that times out or answers 5xx leaves the session alone:
/// warn, continue, try again only after 5 minutes.
#[tokio::test]
async fn an_unreachable_provider_keeps_the_session_and_retries_after_five_minutes() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    for fault in [Fault::Delay(Duration::from_secs(2)), Fault::Status(503)] {
        w.op.set_fault(Endpoint::Token, Some(fault));
        assert_eq!(w.check(&done).await, RefreshVerdict::Continue, "{fault:?}");
        let row = w.row(&done, "revoked_at, refresh_token_enc").await;
        assert!(row["revoked_at"].is_null(), "{fault:?}");
        let plain = web_keys()
            .purpose(KeyPurpose::RefreshToken)
            .expect("keys")
            .open(done.public_id.as_bytes(), &unhex(&row["refresh_token_enc"]))
            .expect("opens");
        assert_eq!(plain, b"synthetic-refresh-token", "nothing was stored");
        // The claim was taken: the next request does not call again.
        let identity =
            forge_db::sessions::authenticate(&w.t.db, &rid(), &done.token.reveal(), IDLE)
                .await
                .expect("authenticated");
        assert!(!identity.refresh_due, "{fault:?}");
    }
    w.op.set_fault(Endpoint::Token, None);
    assert_eq!(w.check(&done).await, RefreshVerdict::Continue);
    w.t.drop_now().await;
}

/// A refreshed ID token without the access role, or naming another
/// subject, ends the session.
#[tokio::test]
async fn a_removed_role_or_a_changed_subject_revokes_the_session() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    w.op.set_refresh(
        RefreshMode::WithIdToken,
        json!({"resource_access": {"gitstation": {"roles": []}}}),
    );
    assert_eq!(
        w.check(&done).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    assert_eq!(w.revoked_reason(&done).await, "idp_revoked");

    let other = sign_in(&w.oidc, &w.op).await;
    w.op.set_refresh(
        RefreshMode::WithIdToken,
        json!({"sub": "synthetic-sub-mallory"}),
    );
    assert_eq!(
        w.check(&other).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    // A forged / broken refreshed ID token (wrong audience) also ends it.
    let third = sign_in(&w.oidc, &w.op).await;
    w.op.set_refresh(RefreshMode::WithIdToken, json!({"aud": "someone-else"}));
    assert_eq!(
        w.check(&third).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    w.t.drop_now().await;
}

/// Without an ID token the last confirmed role stands three times in a
/// row; the fourth revokes (fail closed). One with an ID token resets it.
#[tokio::test]
async fn answers_without_an_id_token_are_accepted_three_times_in_a_row() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    w.op.set_refresh(RefreshMode::WithoutIdToken, json!({}));
    for n in 1..=3 {
        assert_eq!(w.check(&done).await, RefreshVerdict::Continue, "#{n}");
        assert_eq!(
            w.row(&done, "refresh_without_id_token").await["refresh_without_id_token"],
            n
        );
    }
    w.op.set_refresh(RefreshMode::WithIdToken, json!({}));
    assert_eq!(w.check(&done).await, RefreshVerdict::Continue);
    assert_eq!(
        w.row(&done, "refresh_without_id_token").await["refresh_without_id_token"],
        0
    );
    w.op.set_refresh(RefreshMode::WithoutIdToken, json!({}));
    for _ in 1..=3 {
        assert_eq!(w.check(&done).await, RefreshVerdict::Continue);
    }
    assert_eq!(
        w.check(&done).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    w.t.drop_now().await;
}

/// The wrapper: a session revoked by the check is this request's
/// `FG-AUTH-0001 reason=session_revoked`; one that is not due never
/// reaches the provider.
#[tokio::test]
async fn the_refreshing_authenticator_refuses_a_session_the_provider_ended() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let store: Arc<dyn SessionAuthenticator> =
        Arc::new(SessionStore::new(w.t.db.clone(), IDLE).deferring_touch_to_refresh_check());
    let sessions = RefreshingSessions::new(store, Arc::clone(&w.oidc));
    let hits = w.op.hits(Endpoint::Token);
    sessions
        .authenticate_session(&rid(), &done.token.reveal())
        .await
        .expect("not due: fine");
    assert_eq!(w.op.hits(Endpoint::Token), hits, "not due: no call");
    w.op.revoke_refresh_tokens();
    w.t.exec_superuser(&format!(
        "UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '301 seconds' \
         WHERE public_id = '{}'",
        done.public_id
    ))
    .await;
    let err = sessions
        .authenticate_session(&rid(), &done.token.reveal())
        .await
        .expect_err("ended at the provider");
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
    assert_eq!(reason(&err), "session_revoked");
    assert!(
        err.to_string().contains("revoked_reason=idp_revoked"),
        "{err}"
    );
    w.t.drop_now().await;
}

/// The RP logout revokes the session (`logout`) and sends the browser to
/// the provider with the ID token as hint; without a live session, with
/// no hint; without an `end_session_endpoint`, home.
#[tokio::test]
async fn the_rp_logout_revokes_and_sends_the_browser_to_the_provider() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let identity = forge_db::sessions::authenticate(&w.t.db, &rid(), &done.token.reveal(), IDLE)
        .await
        .expect("authenticated");
    let request_id = rid();
    let request = LogoutRequest {
        request_id: &request_id,
        user: done.user,
        session_id: identity.session_id,
        source_ip: None,
        user_agent: Some("synthetic-agent"),
    };
    let location = w.oidc.logout(request).await.expect("logout");
    assert!(
        location.starts_with(&format!("{}/logout?", w.op.issuer)),
        "{location}"
    );
    for part in [
        "id_token_hint=",
        "client_id=gitstation",
        "post_logout_redirect_uri=http%3A%2F%2Flocalhost%3A8080%2F",
    ] {
        assert!(location.contains(part), "{part}: {location}");
    }
    assert_eq!(w.revoked_reason(&done).await, "logout");
    let audit =
        w.t.json_rows(&format!(
            "SELECT actor_kind, details->>'reason' AS reason FROM forge.audit_log \
             WHERE request_id = '{request_id}' AND action = 'session.revoked'"
        ))
        .await;
    assert_eq!(audit.len(), 1, "one audit row: {audit:?}");
    assert_eq!(audit[0]["reason"], "logout");
    let again = w.oidc.logout(request).await.expect("logout again");
    assert!(!again.contains("id_token_hint"), "{again}");

    let no_endpoint = MockOp::start().await;
    let mut doc = json!({
        "issuer": no_endpoint.issuer,
        "authorization_endpoint": format!("{}/auth", no_endpoint.issuer),
        "token_endpoint": format!("{}/token", no_endpoint.issuer),
        "jwks_uri": format!("{}/certs", no_endpoint.issuer),
    });
    no_endpoint.set_discovery(Some(doc.take()));
    let bare = Oidc::new(no_endpoint.settings(BASE), &web_keys(), w.t.db.clone()).expect("oidc");
    assert_eq!(
        bare.logout(request).await.expect("home"),
        "http://localhost:8080/"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_backchannel_logout_revokes_by_sid_or_subject_and_refuses_a_replay() {
    let w = world().await;
    let first = sign_in(&w.oidc, &w.op).await;
    let token = w.op.logout_token(json!({}));
    assert_eq!(
        w.oidc
            .backchannel_logout(&rid(), &token, None)
            .await
            .expect("logout"),
        1
    );
    assert_eq!(w.revoked_reason(&first).await, "backchannel");
    let err = w
        .oidc
        .backchannel_logout(&rid(), &token, None)
        .await
        .expect_err("replay");
    assert_eq!(err.code(), ErrorCode::LogoutTokenInvalid);
    assert_eq!(reason(&err), "replayed");
    // An unknown sid revokes nothing and is still a success.
    let unknown = w.op.logout_token(json!({"sid": "synthetic-sid-unknown"}));
    assert_eq!(
        w.oidc
            .backchannel_logout(&rid(), &unknown, None)
            .await
            .expect("ok"),
        0
    );
    // Without sid: every session of the subject.
    let a = sign_in(&w.oidc, &w.op).await;
    let b = sign_in(&w.oidc, &w.op).await;
    let by_sub =
        w.op.logout_token(json!({"sid": null, "sub": "synthetic-sub-alice"}));
    assert_eq!(
        w.oidc
            .backchannel_logout(&rid(), &by_sub, None)
            .await
            .expect("sub"),
        2
    );
    assert_eq!(w.revoked_reason(&a).await, "backchannel");
    assert_eq!(w.revoked_reason(&b).await, "backchannel");
    w.t.drop_now().await;
}

#[tokio::test]
async fn logout_token_failures_are_logout_token_invalid() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let mut cases: Vec<(String, &str)> = vec![
        (w.op.logout_token(json!({"nonce": "n"})), "nonce_present"),
        (w.op.logout_token(json!({"events": null})), "missing_claim"),
        (w.op.logout_token(json!({"events": {"x": {}}})), "events"),
        (w.op.logout_token(json!({"jti": null})), "missing_claim"),
        (w.op.logout_token(json!({"sid": null})), "missing_claim"),
        (
            w.op.logout_token(json!({"aud": "someone-else"})),
            "audience",
        ),
        (
            w.op.logout_token(json!({"iss": "http://127.0.0.1:1/realms/other"})),
            "issuer",
        ),
        (w.op.logout_token(json!({"iat": now - 600})), "too_old"),
        (w.op.logout_token(json!({"exp": now - 600})), "expired"),
        ("not.a.jwt".to_owned(), "malformed"),
        ("only-one-part".to_owned(), "malformed"),
    ];
    // A changed payload breaks the signature.
    let good = w.op.logout_token(json!({}));
    let parts: Vec<&str> = good.split('.').collect();
    let forged_payload = base64_url(&json!({
        "iss": w.op.issuer, "aud": "gitstation", "iat": now, "jti": "synthetic-jti-forged",
        "sub": "synthetic-sub-alice",
        "events": {"http://schemas.openid.net/event/backchannel-logout": {}},
    }));
    cases.push((
        format!("{}.{forged_payload}.{}", parts[0], parts[2]),
        "signature",
    ));
    // alg none: refused before any key is looked at.
    let none_header = base64_url(&json!({"alg": "none", "kid": "synthetic-rsa-1"}));
    cases.push((
        format!("{none_header}.{forged_payload}."),
        "alg_not_allowed",
    ));
    for (token, want) in &cases {
        let err = w
            .oidc
            .backchannel_logout(&rid(), token, None)
            .await
            .expect_err(want);
        assert_eq!(err.code(), ErrorCode::LogoutTokenInvalid, "{want}: {err}");
        assert_eq!(&reason(&err), want, "{err}");
    }
    assert!(
        w.row(&done, "revoked_at").await["revoked_at"].is_null(),
        "no refused token revoked anything"
    );
    w.t.drop_now().await;
}

fn base64_url(v: &Value) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
}

// --- review of MR5: cancellation, the touch order, sid with sub ---

/// The provider's request goes away (it disconnects, the request times
/// out) while the back-channel logout runs: the detached task still
/// finishes, so the session is revoked and the provider's retry finds the
/// `jti` remembered for a logout that happened (HIGH of the review).
#[tokio::test]
async fn a_backchannel_logout_finishes_when_its_request_goes_away() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    // A cold instance: its discovery is slow, so the call is still inside
    // the task when the caller gives up.
    let cold = Arc::new(Oidc::new(w.op.settings(BASE), &web_keys(), w.t.db.clone()).expect("oidc"));
    w.op.set_fault(
        Endpoint::Discovery,
        Some(Fault::Delay(Duration::from_millis(300))),
    );
    let token = w.op.logout_token(json!({}));
    let request_id = rid();
    let gave_up = tokio::time::timeout(
        Duration::from_millis(50),
        cold.backchannel_logout(&request_id, &token, None),
    )
    .await;
    assert!(gave_up.is_err(), "the caller went away first");
    w.op.set_fault(Endpoint::Discovery, None);
    let mut revoked = Value::Null;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        revoked = w.revoked_reason(&done).await;
        if !revoked.is_null() {
            break;
        }
    }
    assert_eq!(revoked, "backchannel", "the task finished the logout");
    let err = cold
        .backchannel_logout(&rid(), &token, None)
        .await
        .expect_err("the retry is a replay of a done logout");
    assert_eq!(reason(&err), "replayed");
    w.t.drop_now().await;
}

/// The request goes away after the provider rotated the refresh token:
/// the detached check still stores the rotated token, so the next refresh
/// is not `invalid_grant` (MEDIUM of the review).
#[tokio::test]
async fn a_refresh_whose_request_goes_away_still_stores_the_rotated_token() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let identity = w.due(&done).await;
    w.op.set_fault(
        Endpoint::Token,
        Some(Fault::Delay(Duration::from_millis(300))),
    );
    let gave_up = tokio::time::timeout(
        Duration::from_millis(50),
        w.oidc.refresh_check(&rid(), &identity),
    )
    .await;
    assert!(gave_up.is_err(), "the caller went away first");
    w.op.set_fault(Endpoint::Token, None);
    let refresh = web_keys().purpose(KeyPurpose::RefreshToken).expect("keys");
    let mut stored = Vec::new();
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let row = w.row(&done, "refresh_token_enc").await;
        stored = refresh
            .open(done.public_id.as_bytes(), &unhex(&row["refresh_token_enc"]))
            .expect("opens");
        if stored != b"synthetic-refresh-token" {
            break;
        }
    }
    assert_eq!(
        stored, b"synthetic-refresh-token-1",
        "the rotated token was stored"
    );
    assert_eq!(
        w.check(&done).await,
        RefreshVerdict::Continue,
        "no invalid_grant"
    );
    w.t.drop_now().await;
}

/// The store defers the activity of a due session to the check: a session
/// the check revokes keeps its idle expiry; one it lets go on is touched.
#[tokio::test]
async fn a_due_session_is_touched_only_after_the_check_lets_it_go_on() {
    let w = world().await;
    let store: Arc<dyn SessionAuthenticator> =
        Arc::new(SessionStore::new(w.t.db.clone(), IDLE).deferring_touch_to_refresh_check());
    let sessions = RefreshingSessions::new(store, Arc::clone(&w.oidc));
    let stale_and_due = |public_id: &str| {
        format!(
            "UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '301 seconds', \
             last_seen_at = now() - interval '2 minutes', \
             idle_expires_at = now() + interval '1 hour' WHERE public_id = '{public_id}'"
        )
    };
    let kept = sign_in(&w.oidc, &w.op).await;
    w.t.exec_superuser(&stale_and_due(&kept.public_id)).await;
    let before = w.row(&kept, "idle_expires_at").await;
    sessions
        .authenticate_session(&rid(), &kept.token.reveal())
        .await
        .expect("goes on");
    let after = w.row(&kept, "idle_expires_at").await;
    assert_ne!(before, after, "touched after the Continue");

    let ended = sign_in(&w.oidc, &w.op).await;
    w.t.exec_superuser(&stale_and_due(&ended.public_id)).await;
    let before = w.row(&ended, "idle_expires_at, last_seen_at").await;
    w.op.revoke_refresh_tokens();
    sessions
        .authenticate_session(&rid(), &ended.token.reveal())
        .await
        .expect_err("revoked");
    let after = w.row(&ended, "idle_expires_at, last_seen_at").await;
    assert_eq!(before, after, "a revoked session was never extended");
    w.t.drop_now().await;
}

/// A logout token with `sid` and another `sub` revokes nothing (the
/// mismatch is logged).
#[tokio::test]
async fn a_logout_token_whose_sub_does_not_match_its_sid_revokes_nothing() {
    let w = world().await;
    let done = sign_in(&w.oidc, &w.op).await;
    let token = w.op.logout_token(json!({"sub": "synthetic-sub-mallory"}));
    assert_eq!(
        w.oidc
            .backchannel_logout(&rid(), &token, None)
            .await
            .expect("valid token"),
        0
    );
    assert!(w.revoked_reason(&done).await.is_null());
    let token = w.op.logout_token(json!({"sub": "synthetic-sub-alice"}));
    assert_eq!(
        w.oidc
            .backchannel_logout(&rid(), &token, None)
            .await
            .expect("valid token"),
        1
    );
    w.t.drop_now().await;
}
