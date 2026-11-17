//! The PFS Keycloak's shape against the mock provider and a migrated
//! database (ADR-0021 amendment 2026-11-17, `auth.oidc.claims_source =
//! access_token`): a bare ID token, the identity claims and the client
//! roles in the access token only, `staff_id` as the employee number. The
//! sign-in, the admission, every access token refusal (`FG-AUTH-0025`),
//! the link by the employee number alone (no `email_verified`, the case
//! ignored) and the 5-minute refresh check. Synthetic data only.
#![allow(clippy::expect_used, clippy::panic)] // tests fail loudly

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_core::config::OidcClaimsSource;
use forge_core::{RequestId, SessionRevokeReason};
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::test_support::{web_keys, MockOp};
use forge_oidc::{CallbackInput, LoginDone, Oidc, RefreshVerdict};
use serde_json::{json, Value};

const BASE: &str = "http://localhost:8080";
const IDLE: Duration = Duration::from_secs(4 * 3600);

fn rid() -> RequestId {
    RequestId::generate()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// The configuration of this PC's PFS Keycloak, against the mock.
fn pfs(t: &FreshDb, op: &MockOp) -> Arc<Oidc> {
    let mut settings = op.settings(BASE);
    settings.claims_source = OidcClaimsSource::AccessToken;
    settings.employee_number_claim = "staff_id".to_owned();
    settings.access_role = "access".to_owned();
    Arc::new(Oidc::new(settings, &web_keys(), t.db.clone()).expect("oidc"))
}

async fn sign_in(
    oidc: &Oidc,
    op: &MockOp,
    id_overrides: Value,
    access_overrides: Value,
) -> Result<LoginDone, ForgeError> {
    let start = oidc.start(None).await?;
    let (code, state) = op.authorize_pfs(&start.location, id_overrides, access_overrides);
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
}

async fn nothing_written(t: &FreshDb) {
    assert!(t.json_rows("SELECT id FROM forge.users").await.is_empty());
    assert!(t
        .json_rows("SELECT id FROM forge.web_sessions")
        .await
        .is_empty());
}

#[tokio::test]
async fn a_pfs_sign_in_reads_the_access_token_and_names_the_user_by_staff_id() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("signed in");
    assert_eq!(done.outcome, "created");
    let user = t
        .json_rows(&format!(
            "SELECT username, display_name, email, employee_number FROM forge.users WHERE id = {}",
            done.user.get()
        ))
        .await;
    assert_eq!(user[0]["username"], "syn9001", "the staff_id in lower case");
    assert_eq!(user[0]["display_name"], "PFS Synthetic", "the name claim");
    assert_eq!(user[0]["email"], "pfs.person@example.test");
    assert_eq!(user[0]["employee_number"], "SYN9001", "stored as sent");
    let session = t
        .json_rows(&format!(
            "SELECT oidc_sub, oidc_sid FROM forge.web_sessions WHERE public_id = '{}'",
            done.public_id
        ))
        .await;
    assert_eq!(session[0]["oidc_sub"], "synthetic-sub-pfs-1");
    assert_eq!(session[0]["oidc_sid"], "synthetic-sid-1", "the ID token's");
    // The next sign-in finds the identity.
    let again = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("again");
    assert_eq!((again.outcome, again.user), ("existing", done.user));
    t.drop_now().await;
}

/// The existing link by the employee number: a user an administrator made
/// with the number in lower case and another e-mail is linked by the
/// upper-case `staff_id` alone (PFS sends no `email_verified`).
#[tokio::test]
async fn a_pfs_staff_id_links_the_existing_user_without_the_case_or_the_email() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let mut new = forge_db::users::NewUser::human("alice", "Alice", "alice@example.test");
    new.employee_number = Some("syn9001".to_owned());
    let alice = forge_db::users::create(&t.db, &ChangeContext::new(Actor::System, rid()), &new)
        .await
        .expect("alice")
        .user_id;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("linked");
    assert_eq!((done.outcome, done.user), ("linked", alice));
    t.drop_now().await;
}

/// The admission and the link read the access token: no role there is
/// refused even when the ID token carries one; no `staff_id` is the
/// existing `employee_number_missing` refusal.
#[tokio::test]
async fn a_pfs_sign_in_without_the_access_role_or_staff_id_is_refused() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let err = sign_in(
        &oidc,
        &op,
        json!({"resource_access": {"gitstation": {"roles": ["access"]}}}),
        json!({"resource_access": {"gitstation": {"roles": ["viewer"]}}}),
    )
    .await
    .expect_err("no role in the access token");
    assert_eq!(err.code(), ErrorCode::LoginNotPermitted);
    assert_eq!(reason(&err), "no_access_role");
    let err = sign_in(
        &oidc,
        &op,
        json!({}),
        json!({"resource_access": {"other-app": {"roles": ["access"]}}}),
    )
    .await
    .expect_err("another client's role");
    assert_eq!(reason(&err), "no_access_role");
    let err = sign_in(
        &oidc,
        &op,
        json!({"staff_id": "SYN9001"}),
        json!({"staff_id": null}),
    )
    .await
    .expect_err("no staff_id in the access token");
    assert_eq!(err.code(), ErrorCode::IdentityLinkConflict);
    assert_eq!(reason(&err), "employee_number_missing");
    nothing_written(&t).await;
    t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0025: every access token refusal
/// through the flow, the reason logged, nothing written, never a claim.
#[tokio::test]
async fn access_token_failures_are_access_token_invalid() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let cases: Vec<(Value, Value, &str)> = vec![
        (json!({}), json!({"azp": "other-app"}), "azp"),
        (json!({}), json!({"azp": null}), "missing_claim"),
        (
            json!({}),
            json!({"sub": "synthetic-sub-mallory"}),
            "subject_mismatch",
        ),
        (
            json!({"sub": "synthetic-sub-mallory"}),
            json!({}),
            "subject_mismatch",
        ),
        (json!({}), json!({"exp": now() - 120}), "expired"),
        (json!({}), json!({"nbf": now() + 600}), "not_yet_valid"),
        (json!({}), json!({"iat": now() + 600}), "not_yet_valid"),
        (json!({}), json!({"typ": "ID"}), "typ"),
        (json!({}), json!({"typ": "Refresh"}), "typ"),
        (
            json!({}),
            json!({"iss": "http://127.0.0.1:1/realms/other"}),
            "issuer",
        ),
        (json!({}), json!({"aud": 7}), "audience"),
        (json!({}), json!({"sub": null}), "missing_claim"),
    ];
    for (id, access, want) in cases {
        let err = sign_in(&oidc, &op, id.clone(), access.clone())
            .await
            .expect_err(want);
        assert_eq!(
            err.code(),
            ErrorCode::AccessTokenInvalid,
            "{id} {access}: {err}"
        );
        assert_eq!(reason(&err), want, "{id} {access}: {err}");
        let text = err.to_string();
        assert!(
            !text.contains("synthetic-sub") && !text.contains("example.test"),
            "{text}"
        );
    }
    // Keycloak's audiences pass: none, `account`, a list without the client.
    for aud in [json!("account"), json!(["account", "other-app"])] {
        let _done = sign_in(&oidc, &op, json!({}), json!({"aud": aud}))
            .await
            .expect("Keycloak's aud");
    }
    // An ID token failure stays FG-AUTH-0012 (checked first).
    let err = sign_in(&oidc, &op, json!({"aud": "other-app"}), json!({}))
        .await
        .expect_err("id token audience");
    assert_eq!(err.code(), ErrorCode::IdTokenInvalid);
    t.drop_now().await;
}

/// The default (`id_token`) ignores the access token: an opaque one is
/// fine there, but `access_token` needs a JWT.
#[tokio::test]
async fn the_access_token_is_read_only_with_claims_source_access_token() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let start = oidc.start(None).await.expect("start");
    // `authorize` issues an opaque access token (`synthetic-access-token`).
    let (code, state) = op.authorize(&start.location, json!({"staff_id": "SYN9001"}));
    let request_id = rid();
    let err = oidc
        .complete(CallbackInput {
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
        .expect_err("opaque access token");
    assert_eq!(err.code(), ErrorCode::AccessTokenInvalid);
    assert_eq!(reason(&err), "malformed");
    nothing_written(&t).await;
    t.drop_now().await;
}

async fn due_check(t: &FreshDb, oidc: &Arc<Oidc>, done: &LoginDone) -> RefreshVerdict {
    t.exec_superuser(&format!(
        "UPDATE forge.web_sessions SET refresh_checked_at = now() - interval '301 seconds' \
         WHERE public_id = '{}'",
        done.public_id
    ))
    .await;
    let identity = forge_db::sessions::authenticate(&t.db, &rid(), &done.token.reveal(), IDLE)
        .await
        .expect("authenticated");
    assert!(identity.refresh_due);
    oidc.refresh_check(&rid(), &identity)
        .await
        .expect("the check ran")
}

/// The refresh check reads the role from the refreshed access token: a
/// good one continues (the ID token has no roles), a removed role, another
/// `sub` or an invalid access token revokes the session.
#[tokio::test]
async fn a_pfs_refresh_confirms_the_role_from_the_access_token() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("signed in");
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    for overrides in [
        json!({"resource_access": {"gitstation": {"roles": []}}}),
        json!({"sub": "synthetic-sub-mallory"}),
        json!({"azp": "other-app"}),
    ] {
        op.set_refresh_access(json!({}));
        let session = sign_in(&oidc, &op, json!({}), json!({}))
            .await
            .expect("signed in");
        op.set_refresh_access(overrides.clone());
        assert_eq!(
            due_check(&t, &oidc, &session).await,
            RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked),
            "{overrides}"
        );
    }
    t.drop_now().await;
}
