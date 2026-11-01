//! The whole sign-in against the mock provider and a migrated database
//! (ADR-0021 §3, §11): start → authorize → callback, the state cookie
//! rules (missing, tampered, expired, mismatch, replay), the ID token's
//! nonce / aud / azp / expiry checks through the flow, the JWKS refetch
//! limit, provider failures (`FG-AUTH-0015`), the admission (`0013`), the
//! revocation of the previous session and the sealed IdP tokens.
#![allow(clippy::expect_used, clippy::panic)] // tests fail loudly

use std::time::Duration;

use forge_core::web_keys::KeyPurpose;
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::state::LoginState;
use forge_oidc::test_support::{web_keys, Endpoint, Fault, MockOp, EC_KID, RSA_KID};
use forge_oidc::{CallbackInput, LoginDone, Oidc, OidcSettings};
use serde_json::{json, Value};

const BASE: &str = "http://localhost:8080";

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

async fn oidc_with(t: &FreshDb, op: &MockOp, edit: impl FnOnce(&mut OidcSettings)) -> Oidc {
    let mut settings = op.settings(BASE);
    edit(&mut settings);
    Oidc::new(settings, &web_keys(), t.db.clone()).expect("oidc")
}

/// Start, have the mock authorize with `overrides`, and call back.
async fn sign_in(
    oidc: &Oidc,
    op: &MockOp,
    overrides: Value,
    session_cookie: Option<&str>,
) -> Result<LoginDone, ForgeError> {
    let start = oidc.start(Some("/acme/forge")).await?;
    let (code, state) = op.authorize(&start.location, overrides);
    let request_id = rid();
    oidc.complete(CallbackInput {
        request_id: &request_id,
        code: Some(&code),
        state: Some(&state),
        error: None,
        state_cookie: Some(&start.state_cookie),
        session_cookie,
        source_ip: None,
        user_agent: Some("synthetic-agent"),
    })
    .await
}

async fn rows(t: &FreshDb, sql: &str) -> Vec<Value> {
    t.json_rows(sql).await
}

#[tokio::test]
async fn a_first_sign_in_creates_the_user_issues_a_session_and_seals_the_idp_tokens() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    let start = oidc
        .start(Some("https://evil.example.invalid/"))
        .await
        .expect("start");
    assert!(start.next_rejected);
    assert!(start.location.starts_with(&format!("{}/auth?", op.issuer)));
    for part in [
        "response_type=code",
        "scope=openid",
        "response_mode=query",
        "code_challenge_method=S256",
        "redirect_uri=http%3A%2F%2Flocalhost%3A8080%2F-%2Fauth%2Fcallback",
    ] {
        assert!(start.location.contains(part), "{part}: {}", start.location);
    }
    let done = sign_in(&oidc, &op, json!({}), None)
        .await
        .expect("signed in");
    assert_eq!(done.outcome, "created");
    assert_eq!(done.next, "/acme/forge");
    assert_eq!(done.max_age, Duration::from_secs(12 * 3600));
    let session = rows(
        &t,
        &format!(
            "SELECT oidc_issuer, oidc_sub, oidc_sid, refresh_token_enc, id_token_enc \
             FROM forge.web_sessions WHERE public_id = '{}'",
            done.public_id
        ),
    )
    .await;
    assert_eq!(session[0]["oidc_issuer"], op.issuer.as_str());
    assert_eq!(session[0]["oidc_sub"], "synthetic-sub-alice");
    assert_eq!(session[0]["oidc_sid"], "synthetic-sid-1");
    // Sealed with the session's public id: opens only with that AAD.
    let sealed = |v: &Value| -> Vec<u8> {
        let hex = v.as_str().expect("bytea").trim_start_matches("\\x");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
            .collect()
    };
    let refresh = web_keys().purpose(KeyPurpose::RefreshToken).expect("keys");
    let plain = refresh
        .open(
            done.public_id.as_bytes(),
            &sealed(&session[0]["refresh_token_enc"]),
        )
        .expect("opens with its public id");
    assert_eq!(plain, b"synthetic-refresh-token");
    assert!(refresh
        .open(
            b"0000000000000000",
            &sealed(&session[0]["refresh_token_enc"])
        )
        .is_err());
    let id = web_keys().purpose(KeyPurpose::IdToken).expect("keys");
    assert!(id
        .open(
            done.public_id.as_bytes(),
            &sealed(&session[0]["id_token_enc"])
        )
        .is_ok());

    // A second sign-in finds the identity and revokes the previous session.
    let previous = done.token.reveal();
    let again = sign_in(&oidc, &op, json!({}), Some(&previous))
        .await
        .expect("again");
    assert_eq!(again.outcome, "existing");
    assert_eq!(again.user, done.user);
    let old = rows(
        &t,
        &format!(
            "SELECT revoked_reason FROM forge.web_sessions WHERE public_id = '{}'",
            done.public_id
        ),
    )
    .await;
    assert_eq!(old[0]["revoked_reason"], "logout");
    let audit = rows(
        &t,
        "SELECT action FROM forge.audit_log WHERE action LIKE 'session.%' ORDER BY id",
    )
    .await;
    let actions: Vec<&str> = audit.iter().filter_map(|r| r["action"].as_str()).collect();
    assert_eq!(
        actions,
        ["session.created", "session.revoked", "session.created"]
    );
    t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0011.
#[tokio::test]
async fn login_state_failures_are_login_state_invalid() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    let start = oidc.start(None).await.expect("start");
    let (code, state) = op.authorize(&start.location, json!({}));
    let request_id = rid();
    let input = CallbackInput {
        request_id: &request_id,
        code: Some(&code),
        state: Some(&state),
        error: None,
        state_cookie: Some(&start.state_cookie),
        session_cookie: None,
        source_ip: None,
        user_agent: None,
    };
    let mut tampered = start.state_cookie.clone();
    tampered.replace_range(10..11, if &tampered[10..11] == "A" { "B" } else { "A" });
    let keys = web_keys().purpose(KeyPurpose::OidcState).expect("keys");
    let mut old = LoginState::generate("/".to_owned(), 1_000).expect("state");
    old.state = state.clone();
    let expired = old.seal(&keys).expect("seal");
    let cases: Vec<(CallbackInput<'_>, &str)> = vec![
        (
            CallbackInput {
                state_cookie: None,
                ..input
            },
            "state_cookie_missing",
        ),
        (
            CallbackInput {
                state_cookie: Some(&tampered),
                ..input
            },
            "state_cookie_invalid",
        ),
        (
            CallbackInput {
                state_cookie: Some("%%%"),
                ..input
            },
            "state_cookie_invalid",
        ),
        (
            CallbackInput {
                state_cookie: Some(&expired),
                ..input
            },
            "state_expired",
        ),
        (
            CallbackInput {
                state: Some("another-state"),
                ..input
            },
            "state_mismatch",
        ),
        (
            CallbackInput {
                state: None,
                ..input
            },
            "query_missing",
        ),
        (
            CallbackInput {
                code: None,
                ..input
            },
            "query_missing",
        ),
        (
            CallbackInput {
                error: Some("access_denied"),
                ..input
            },
            "idp_error",
        ),
    ];
    for (bad, expected) in cases {
        let err = oidc.complete(bad).await.expect_err(expected);
        assert_eq!(
            err.code(),
            ErrorCode::LoginStateInvalid,
            "{expected}: {err}"
        );
        assert_eq!(reason(&err), expected, "{err}");
    }
    // The real one works once; the replayed callback is refused (single-use code).
    oidc.complete(input).await.expect("first callback");
    let err = oidc.complete(input).await.expect_err("replay");
    assert_eq!(reason(&err), "code_rejected", "{err}");
    assert!(
        err.to_string().contains("oauth_error=invalid_grant"),
        "{err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn the_flow_refuses_a_bad_nonce_audience_azp_or_expiry() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    for (overrides, expected) in [
        (json!({"nonce": "replayed-nonce"}), "nonce"),
        (json!({"aud": "other-client"}), "audience"),
        (
            json!({"aud": ["gitstation", "other"], "azp": "other"}),
            "azp",
        ),
        (json!({"exp": now - 120}), "expired"),
        (json!({"iat": now + 600}), "not_yet_valid"),
    ] {
        let err = sign_in(&oidc, &op, overrides.clone(), None)
            .await
            .expect_err(expected);
        assert_eq!(err.code(), ErrorCode::IdTokenInvalid, "{overrides}");
        assert_eq!(reason(&err), expected, "{err}");
    }
    // Clock skew within 60 s is accepted.
    sign_in(&oidc, &op, json!({"iat": now + 30, "exp": now + 40}), None)
        .await
        .expect("within skew");
    assert_eq!(
        t.json_rows("SELECT id FROM forge.users").await.len(),
        1,
        "only the accepted sign-in wrote a user"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_kid_refetches_the_jwks_at_most_once_a_minute() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    // The provider publishes only its EC key but signs with the RSA one.
    let ec_only = json!({"keys": [op.jwks_json()["keys"][1].clone()]});
    op.set_jwks(Some(ec_only.to_string()));
    op.sign_with(RSA_KID);
    let mut hits = Vec::new();
    for _ in 0..3 {
        let err = sign_in(&oidc, &op, json!({}), None)
            .await
            .expect_err("unknown kid");
        assert_eq!(reason(&err), "unknown_kid", "{err}");
        hits.push(op.hits(Endpoint::Jwks));
    }
    // 1: the first fetch; 2: one refetch for the unknown kid; then none.
    assert_eq!(hits, [1, 2, 2]);
    // Publishing the key does not help within the minute (no fetch)…
    op.set_jwks(None);
    sign_in(&oidc, &op, json!({}), None)
        .await
        .expect_err("still unknown");
    assert_eq!(op.hits(Endpoint::Jwks), 2);
    // …but the known EC key verifies from the cache.
    op.sign_with(EC_KID);
    sign_in(&oidc, &op, json!({}), None)
        .await
        .expect("EC key cached");
    assert_eq!(op.hits(Endpoint::Jwks), 2);
    t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0015: a slow or broken provider.
#[tokio::test]
async fn idp_timeout_is_identity_provider_unavailable() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    oidc.start(None).await.expect("discovery cached");
    op.set_fault(Endpoint::Token, Some(Fault::Delay(Duration::from_secs(3))));
    let err = sign_in(&oidc, &op, json!({}), None)
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::IdentityProviderUnavailable);
    assert_eq!(reason(&err), "timeout");
    assert!(err.to_string().contains("call=token"), "{err}");
    op.set_fault(Endpoint::Token, Some(Fault::Status(502)));
    let err = sign_in(&oidc, &op, json!({}), None).await.expect_err("5xx");
    assert_eq!(reason(&err), "status");
    op.set_fault(Endpoint::Token, None);
    op.set_fault(Endpoint::Jwks, Some(Fault::InvalidJson));
    let err = sign_in(&oidc, &op, json!({}), None)
        .await
        .expect_err("jwks json");
    assert_eq!(reason(&err), "invalid_json");
    assert!(err.to_string().contains("call=jwks"), "{err}");

    // Discovery: not cached on failure; 503, then a document pointing elsewhere.
    let fresh = oidc_with(&t, &op, |_| {}).await;
    op.set_fault(Endpoint::Discovery, Some(Fault::Status(503)));
    let err = fresh.start(None).await.expect_err("discovery 503");
    assert_eq!(reason(&err), "status");
    op.set_fault(Endpoint::Discovery, None);
    let i = op.issuer.clone();
    op.set_discovery(Some(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/auth"),
        "token_endpoint": "http://127.0.0.2:9/token",
        "jwks_uri": format!("{i}/certs"),
    })));
    let err = fresh.start(None).await.expect_err("origin");
    assert_eq!(reason(&err), "endpoint_origin");
    op.set_discovery(Some(json!({
        "issuer": "http://127.0.0.1:1/realms/other",
        "authorization_endpoint": format!("{i}/auth"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/certs"),
    })));
    let err = fresh.start(None).await.expect_err("issuer");
    assert_eq!(reason(&err), "issuer_mismatch");
    op.set_discovery(None);
    fresh.start(None).await.expect("recovers");

    // Nothing listening: connect fails fast.
    let mut settings = op.settings(BASE);
    settings.issuer = "http://127.0.0.1:9/realms/none".to_owned();
    let down = Oidc::new(settings, &web_keys(), t.db.clone()).expect("oidc");
    let err = down.start(None).await.expect_err("down");
    assert_eq!(err.code(), ErrorCode::IdentityProviderUnavailable);
    assert!(
        ["connect", "connect_timeout"].contains(&reason(&err).as_str()),
        "{err}"
    );
    t.drop_now().await;
}

#[tokio::test]
async fn admission_and_link_refusals_write_nothing() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = oidc_with(&t, &op, |_| {}).await;
    let err = sign_in(
        &oidc,
        &op,
        json!({"resource_access": {"gitstation": {"roles": []}}}),
        None,
    )
    .await
    .expect_err("no role");
    assert_eq!(err.code(), ErrorCode::LoginNotPermitted);
    assert_eq!(reason(&err), "no_access_role");
    let err = sign_in(&oidc, &op, json!({"employee_number": null}), None)
        .await
        .expect_err("no employee number");
    assert_eq!(err.code(), ErrorCode::IdentityLinkConflict);
    assert_eq!(reason(&err), "employee_number_missing");
    assert!(t.json_rows("SELECT id FROM forge.users").await.is_empty());
    assert!(t
        .json_rows("SELECT id FROM forge.web_sessions")
        .await
        .is_empty());

    // auto_link = false: an existing employee number is not linked.
    let mut new = forge_db::users::NewUser::human("alice", "Alice", "alice@example.test");
    new.employee_number = Some("SYN0001".to_owned());
    let ctx = ChangeContext::new(Actor::System, rid());
    forge_db::users::create(&t.db, &ctx, &new)
        .await
        .expect("alice");
    let strict = oidc_with(&t, &op, |s| s.auto_link = false).await;
    let err = sign_in(&strict, &op, json!({}), None)
        .await
        .expect_err("auto link off");
    assert_eq!(reason(&err), "auto_link_disabled");
    let linked = sign_in(&oidc, &op, json!({}), None)
        .await
        .expect("auto link on");
    assert_eq!(linked.outcome, "linked");
    t.drop_now().await;
}

#[tokio::test]
async fn the_operator_link_needs_the_system_actor() {
    let t = FreshDb::migrated().await;
    let mut new = forge_db::users::NewUser::human("root", "Root", "root@example.test");
    new.is_admin = true;
    let created = forge_db::users::create(&t.db, &ChangeContext::new(Actor::System, rid()), &new)
        .await
        .expect("root");
    let user = created.user_id;
    let as_user = ChangeContext::new(Actor::User(user), rid());
    let err =
        forge_oidc::operator_link_identity(&t.db, &as_user, user, "http://127.0.0.1:1/r", "s")
            .await
            .expect_err("not system");
    assert_eq!(err.code(), ErrorCode::Internal);
    assert_eq!(reason(&err), "operator_not_system");
    let system = ChangeContext::new(Actor::System, rid());
    let linked =
        forge_oidc::operator_link_identity(&t.db, &system, user, "http://127.0.0.1:1/r", "s")
            .await
            .expect("linked");
    assert_eq!(linked, forge_db::identities::OperatorLink::Linked);
    t.drop_now().await;
}
