//! The PFS portal's widget token (ADR-0036) against the mock provider and
//! a migrated database: a portal access token (`azp = portal`) verified
//! with the realm's keys, its party and role reported for `authorize()`,
//! the person found by the sign-in's link rules read-only (never created
//! or linked), and every refusal (`FG-AUTH-0026`, `FG-AUTH-0027`).
//! Synthetic data only.
#![allow(clippy::expect_used, clippy::panic)] // tests fail loudly

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_core::config::OidcClaimsSource;
use forge_core::portal::PortalAuthenticator;
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::test_support::{web_keys, MockOp, RSA_KID};
use forge_oidc::{Oidc, PortalWidgets};
use serde_json::{json, Value};

const BASE: &str = "http://localhost:8080";

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

fn widgets(t: &FreshDb, op: &MockOp) -> PortalWidgets {
    let mut settings = op.settings(BASE);
    settings.claims_source = OidcClaimsSource::AccessToken;
    settings.employee_number_claim = "staff_id".to_owned();
    settings.timeouts.fetch = Duration::from_secs(2);
    let oidc = Arc::new(Oidc::new(settings, &web_keys(), t.db.clone()).expect("oidc"));
    PortalWidgets::new(oidc, "portal")
}

/// The portal's access token for the synthetic PFS person, with
/// `overrides` (a `null` removes a claim).
fn token(op: &MockOp, overrides: Value) -> String {
    let mut claims = op.pfs_access_claims();
    claims["azp"] = json!("portal");
    if let (Some(c), Some(o)) = (claims.as_object_mut(), overrides.as_object()) {
        for (k, v) in o {
            if v.is_null() {
                c.remove(k);
            } else {
                c.insert(k.clone(), v.clone());
            }
        }
    }
    op.sign(RSA_KID, json!({}), &claims)
}

async fn staff(t: &FreshDb, name: &str, number: &str) -> forge_core::UserId {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.employee_number = Some(number.to_owned());
    users::create(
        &t.db,
        &ChangeContext::new(Actor::System, RequestId::generate()),
        &new,
    )
    .await
    .expect("user")
    .user_id
}

#[tokio::test]
async fn a_portal_token_names_the_linked_person_and_its_facts() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let person = staff(&t, "pfsperson", "syn9001").await;
    let portal = widgets(&t, &op);
    let rid = RequestId::generate();
    let ok = portal
        .authenticate_portal(&rid, &token(&op, json!({})))
        .await
        .expect("verified");
    assert_eq!(ok.user_id, person);
    assert!(ok.active && ok.portal_party && ok.access_role);
    // Another party's token (the gitstation client's own) is verified, and
    // its party is reported for authorize() to refuse.
    let own = portal
        .authenticate_portal(&rid, &token(&op, json!({ "azp": "gitstation" })))
        .await
        .expect("verified");
    assert!(!own.portal_party);
    // No access role on gitstation.
    let bare = portal
        .authenticate_portal(&rid, &token(&op, json!({ "resource_access": {} })))
        .await
        .expect("verified");
    assert!(!bare.access_role);
    // Nothing was linked or created by these calls.
    assert!(t
        .json_rows("SELECT id FROM forge.user_identities")
        .await
        .is_empty());
    assert_eq!(t.json_rows("SELECT id FROM forge.users").await.len(), 1);
}

#[tokio::test]
async fn a_bad_portal_token_is_portal_token_invalid() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    staff(&t, "pfsperson", "SYN9001").await;
    let portal = widgets(&t, &op);
    let rid = RequestId::generate();
    let cases = [
        (token(&op, json!({ "exp": now() - 3600 })), "expired"),
        (
            token(&op, json!({ "iss": "https://elsewhere.example.test" })),
            "issuer",
        ),
        (token(&op, json!({ "typ": "ID" })), "typ"),
        (token(&op, json!({ "azp": null })), "missing_claim"),
        ("not-a-jwt".to_owned(), "malformed"),
    ];
    for (presented, want) in cases {
        let err = portal
            .authenticate_portal(&rid, &presented)
            .await
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::PortalTokenInvalid, "{want}: {err}");
        assert_eq!(reason(&err), want, "{err}");
    }
    // A signature of another key.
    let mut forged = token(&op, json!({}));
    forged.push('x');
    let err = portal
        .authenticate_portal(&rid, &forged)
        .await
        .expect_err("forged");
    assert_eq!(err.code(), ErrorCode::PortalTokenInvalid);
}

#[tokio::test]
async fn an_unlinked_portal_user_is_refused() {
    let t = FreshDb::migrated().await;
    let op = MockOp::start().await;
    let portal = widgets(&t, &op);
    let err = portal
        .authenticate_portal(&RequestId::generate(), &token(&op, json!({})))
        .await
        .expect_err("nobody has the number");
    assert_eq!(err.code(), ErrorCode::PortalUserNotLinked);
    assert_eq!(reason(&err), "unlinked");
    // A widget call creates no user.
    assert!(t.json_rows("SELECT id FROM forge.users").await.is_empty());
}
