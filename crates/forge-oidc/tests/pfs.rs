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
use forge_oidc::test_support::{web_keys, Endpoint, Fault, MockOp};
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
        // Review MEDIUM: Keycloak always sets typ; a token without one is
        // refused.
        (json!({}), json!({"typ": null}), "missing_claim"),
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

/// Review MEDIUM: the ID token itself presented as the access token (same
/// key, same client, same `sub`) is refused by its `typ` — `ID`, or none.
#[tokio::test]
async fn an_id_token_presented_as_the_access_token_is_refused() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    op.access_is_id_token(true);
    for (id_overrides, want) in [(json!({}), "typ"), (json!({"typ": null}), "missing_claim")] {
        let err = sign_in(&oidc, &op, id_overrides.clone(), json!({}))
            .await
            .expect_err("an ID token is not an access token");
        assert_eq!(err.code(), ErrorCode::AccessTokenInvalid, "{err}");
        assert_eq!(reason(&err), want, "{id_overrides}: {err}");
    }
    nothing_written(&t).await;
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

/// Review LOW: "the provider cannot be reached, go on" lasts at most 30
/// minutes since the role was last confirmed; then the check revokes the
/// session (FG-AUTH-0015 `reason=unconfirmed_too_long`). A good refresh
/// confirms the role again.
#[tokio::test]
async fn an_unreachable_provider_keeps_a_session_at_most_thirty_minutes() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("signed in");
    let age = |minutes: i64| {
        format!(
            "UPDATE forge.web_sessions SET access_role_seen_at = now() - interval '{minutes} minutes' \
             WHERE public_id = '{}'",
            done.public_id
        )
    };
    // A good refresh after 20 minutes confirms the role again.
    t.exec_superuser(&age(20)).await;
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    let fresh = t
        .json_rows(&format!(
            "SELECT access_role_seen_at > now() - interval '1 minute' AS fresh \
             FROM forge.web_sessions WHERE public_id = '{}'",
            done.public_id
        ))
        .await;
    assert_eq!(
        fresh[0]["fresh"], true,
        "the access token confirmed the role"
    );

    op.set_fault(Endpoint::Token, Some(Fault::Status(503)));
    t.exec_superuser(&age(29)).await;
    assert_eq!(
        due_check(&t, &oidc, &done).await,
        RefreshVerdict::Continue,
        "within 30 minutes the session goes on"
    );
    t.exec_superuser(&age(31)).await;
    assert_eq!(
        due_check(&t, &oidc, &done).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    assert_eq!(forge_oidc::MAX_UNCONFIRMED_SECS, 1800);
    t.drop_now().await;
}

async fn org_row(t: &FreshDb, done: &LoginDone) -> Value {
    let rows = t
        .json_rows(&format!(
            "SELECT org_units, positions, position_rank, plants, plant_code, \
             invalid_since IS NOT NULL AS invalid FROM forge.user_org_profiles \
             WHERE user_id = {}",
            done.user.get()
        ))
        .await;
    rows.into_iter().next().expect("a snapshot")
}

async fn org_audits(t: &FreshDb, done: &LoginDone) -> usize {
    t.json_rows(&format!(
        "SELECT id FROM forge.audit_log WHERE action = 'user.org_profile_changed' \
         AND resource_id = {}",
        done.user.get()
    ))
    .await
    .len()
}

/// ADR-0021 amendment 2026-11-20: the sign-in stores the access token's org
/// claims (the live realm's JSON-string `org_units`), and a refresh check
/// with changed claims changes the snapshot once.
#[tokio::test]
async fn a_pfs_sign_in_and_refresh_snapshot_the_org_claims() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("signed in");
    let row = org_row(&t, &done).await;
    assert_eq!(
        row["org_units"],
        json!([{"code": "SYN-ORG", "primary": true}])
    );
    assert_eq!(row["positions"], json!(["member"]));
    assert_eq!(row["position_rank"], 3);
    assert_eq!(row["plant_code"], "P001");
    assert_eq!(row["invalid"], false);
    assert_eq!(org_audits(&t, &done).await, 1);

    // The same claims again: no new audit row.
    let again = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("again");
    assert_eq!(again.user, done.user);
    assert_eq!(org_audits(&t, &done).await, 1);

    // A refresh with another department: one change.
    op.set_refresh_access(json!({
        "org_units": [
            "{\"code\":\"SYN-NEW\",\"primary\":true}",
            "{\"code\":\"SYN-ORG\",\"primary\":false}",
        ],
    }));
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    let row = org_row(&t, &done).await;
    assert_eq!(row["org_units"][0]["code"], "SYN-NEW");
    assert_eq!(org_audits(&t, &done).await, 2);
    t.drop_now().await;
}

/// The errors.toml test of FG-AUTH-0028: invalid org claims never block the
/// sign-in or the refresh; the last valid snapshot stays and is marked.
#[tokio::test]
async fn invalid_org_claims_keep_the_snapshot_and_are_logged_once() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("signed in");
    for bad in [
        json!({"org_units": [
            "{\"code\":\"SYN-A\",\"primary\":true}",
            "{\"code\":\"SYN-B\",\"primary\":true}",
        ]}),
        json!({"positions": ["Not-A-Code"]}),
        json!({"position_rank": 1000}),
    ] {
        let again = sign_in(&oidc, &op, json!({}), bad.clone())
            .await
            .expect("an invalid org claim never blocks the sign-in");
        assert_eq!(again.user, done.user, "{bad}");
        let row = org_row(&t, &done).await;
        assert_eq!(row["invalid"], true, "{bad}");
        assert_eq!(row["org_units"][0]["code"], "SYN-ORG", "kept: {bad}");
    }
    op.set_refresh_access(json!({"plants": ["TOOLONG"]}));
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    assert_eq!(org_audits(&t, &done).await, 1, "invalid is not audited");
    // Valid again: the mark clears.
    op.set_refresh_access(json!({}));
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    assert_eq!(org_row(&t, &done).await["invalid"], false);
    t.drop_now().await;
}

/// A first sign-in without any org claim is an empty, valid snapshot.
#[tokio::test]
async fn a_pfs_sign_in_without_org_claims_stores_an_empty_snapshot() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs(&t, &op);
    let done = sign_in(
        &oidc,
        &op,
        json!({}),
        json!({"org_units": null, "positions": null, "position_rank": null,
               "plants": null, "plant_code": null}),
    )
    .await
    .expect("signed in");
    let row = org_row(&t, &done).await;
    assert_eq!(row["org_units"], json!([]));
    assert_eq!(row["invalid"], false);
    t.drop_now().await;
}

/// The PFS configuration with `auth.oidc.admin_role = "admin"`.
fn pfs_admin(t: &FreshDb, op: &MockOp) -> Arc<Oidc> {
    let mut settings = op.settings(BASE);
    settings.claims_source = OidcClaimsSource::AccessToken;
    settings.employee_number_claim = "staff_id".to_owned();
    settings.access_role = "access".to_owned();
    settings.admin_role = Some("admin".to_owned());
    Arc::new(Oidc::new(settings, &web_keys(), t.db.clone()).expect("oidc"))
}

async fn admin_flags(t: &FreshDb, user: forge_core::UserId) -> (bool, String) {
    let rows = t
        .json_rows(&format!(
            "SELECT is_admin, admin_source FROM forge.users WHERE id = {}",
            user.get()
        ))
        .await;
    (
        rows[0]["is_admin"].as_bool().expect("is_admin"),
        rows[0]["admin_source"]
            .as_str()
            .expect("admin_source")
            .to_owned(),
    )
}

async fn admin_changes(t: &FreshDb, user: forge_core::UserId) -> Vec<Value> {
    t.json_rows(&format!(
        "SELECT details FROM forge.audit_log WHERE action = 'user.admin_changed' \
         AND resource_id = {} ORDER BY id",
        user.get()
    ))
    .await
    .into_iter()
    .map(|r| r["details"].clone())
    .collect()
}

/// ADR-0021 amendment 2026-11-24: the client role `admin` grants the
/// administrator flag at the sign-in (before the session exists), the
/// refresh check without it revokes it, and each change is one audit row.
#[tokio::test]
async fn the_admin_role_grants_at_sign_in_and_a_refresh_without_it_revokes() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs_admin(&t, &op);
    let with_admin = json!({"resource_access": {"gitstation": {"roles": ["access", "admin"]}}});
    let done = sign_in(&oidc, &op, json!({}), with_admin.clone())
        .await
        .expect("signed in");
    assert_eq!(admin_flags(&t, done.user).await, (true, "oidc".to_owned()));
    let identity = forge_db::sessions::authenticate(&t.db, &rid(), &done.token.reveal(), IDLE)
        .await
        .expect("authenticated");
    assert!(
        identity.is_admin && identity.admin_from_idp,
        "the first page sees it"
    );
    // A refresh that still carries the role changes nothing.
    op.set_refresh_access(with_admin);
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    assert_eq!(admin_changes(&t, done.user).await.len(), 1);
    // The role was removed at the provider: the next check revokes it, the
    // session goes on.
    op.set_refresh_access(json!({"resource_access": {"gitstation": {"roles": ["access"]}}}));
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    assert_eq!(admin_flags(&t, done.user).await, (false, "cli".to_owned()));
    assert_eq!(
        admin_changes(&t, done.user).await,
        vec![
            json!({"from": false, "to": true, "source": "oidc_role"}),
            json!({"from": true, "to": false, "source": "oidc_role"}),
        ]
    );
    // Signing in again without it: nothing more.
    sign_in(&oidc, &op, json!({}), json!({}))
        .await
        .expect("again");
    assert_eq!(admin_changes(&t, done.user).await.len(), 2);
    t.drop_now().await;
}

/// Break-glass: an administrator made by the CLI and linked keeps the flag
/// when the token has no admin role; without `auth.oidc.admin_role` the
/// role is never read.
#[tokio::test]
async fn a_cli_admin_keeps_the_flag_and_the_role_is_ignored_when_off() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let mut new = forge_db::users::NewUser::human("root", "Root", "root@example.test");
    new.employee_number = Some("SYN9001".to_owned());
    new.is_admin = true;
    let ctx = ChangeContext::new(Actor::System, rid());
    let root = forge_db::users::create(&t.db, &ctx, &new)
        .await
        .expect("root")
        .user_id;
    // An administrator is never linked automatically (privileged_account);
    // the operator links them (runbook oidc-identity-link).
    let linked =
        forge_oidc::operator_link_identity(&t.db, &ctx, root, &op.issuer, "synthetic-sub-pfs-1")
            .await
            .expect("linked");
    assert_eq!(linked, forge_db::identities::OperatorLink::Linked);
    let done = sign_in(&pfs_admin(&t, &op), &op, json!({}), json!({}))
        .await
        .expect("signed in");
    assert_eq!(done.user, root);
    assert_eq!(admin_flags(&t, root).await, (true, "cli".to_owned()));
    assert!(admin_changes(&t, root).await.is_empty());
    // Off: a new person with the role in the token stays a member.
    let off = pfs(&t, &op);
    let other = sign_in(
        &off,
        &op,
        json!({"sub": "synthetic-sub-pfs-2"}),
        json!({"sub": "synthetic-sub-pfs-2", "staff_id": "SYN9002",
               "preferred_username": "syn9002", "email": "pfs.two@example.test",
               "resource_access": {"gitstation": {"roles": ["access", "admin"]}}}),
    )
    .await
    .expect("signed in");
    assert_eq!(admin_flags(&t, other.user).await, (false, "cli".to_owned()));
    t.drop_now().await;
}

/// Review HIGH / MEDIUM 1 (ADR-0021 amendment 2026-11-24): while the
/// provider cannot be reached the session goes on without the role's
/// administrator rights (the flag stays); a session the provider ends
/// (`invalid_grant`) takes the role's flag with it.
#[tokio::test]
async fn an_unconfirmed_role_drops_the_rights_and_an_ended_session_the_flag() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let oidc = pfs_admin(&t, &op);
    let with_admin = json!({"resource_access": {"gitstation": {"roles": ["access", "admin"]}}});
    let done = sign_in(&oidc, &op, json!({}), with_admin)
        .await
        .expect("signed in");
    let identity = |t: &FreshDb| {
        let db = t.db.clone();
        let token = done.token.reveal();
        async move {
            forge_db::sessions::authenticate(&db, &rid(), &token, IDLE)
                .await
                .expect("authenticated")
        }
    };
    assert!(identity(&t).await.is_admin);
    // Unreachable for 10 minutes: the session goes on, the rights do not.
    op.set_fault(Endpoint::Token, Some(Fault::Status(503)));
    t.exec_superuser(&format!(
        "UPDATE forge.web_sessions SET access_role_seen_at = now() - interval '10 minutes' \
         WHERE public_id = '{}'",
        done.public_id
    ))
    .await;
    assert_eq!(due_check(&t, &oidc, &done).await, RefreshVerdict::Continue);
    let unconfirmed = identity(&t).await;
    assert!(!unconfirmed.is_admin && unconfirmed.admin_from_idp);
    assert_eq!(admin_flags(&t, done.user).await, (true, "oidc".to_owned()));
    // The provider answers again but has ended the person: invalid_grant.
    op.set_fault(Endpoint::Token, None);
    op.revoke_refresh_tokens();
    assert_eq!(
        due_check(&t, &oidc, &done).await,
        RefreshVerdict::Revoked(SessionRevokeReason::IdpRevoked)
    );
    assert_eq!(admin_flags(&t, done.user).await, (false, "cli".to_owned()));
    t.drop_now().await;
}
