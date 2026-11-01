//! ID token verification without a database (ADR-0021 §9 "MR4 に要るテスト"):
//! every algorithm / key / claim failure is `FG-AUTH-0012` with its reason,
//! and broken JWKs and JWSs never panic (proptest). Keys and tokens come
//! from the mock provider; everything is synthetic.
#![allow(clippy::expect_used, clippy::panic)] // tests fail loudly

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_core::config::OidcSigningAlg;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::jwt::{Expected, JwtVerifier};
use forge_oidc::test_support::{MockOp, CLIENT_ID, EC_KID, RSA_KID};
use forge_oidc::KeySet as Keys;
use proptest::prelude::*;
use serde_json::{json, Value};

const NONCE: &str = "synthetic-nonce";

fn reason(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn verifier() -> JwtVerifier {
    JwtVerifier::new(&[OidcSigningAlg::Rs256, OidcSigningAlg::Es256]).expect("verifier")
}

fn expected(op: &MockOp) -> Expected<'_> {
    Expected {
        issuer: &op.issuer,
        client_id: CLIENT_ID,
        nonce: Some(NONCE),
        now: now(),
        leeway: 60,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

fn keys(op: &MockOp) -> Keys {
    Keys::parse(op.jwks_json().to_string().as_bytes()).expect("jwks")
}

fn with(mut claims: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (claims.as_object_mut(), extra) {
        for (k, v) in extra {
            if v.is_null() {
                base.remove(&k);
            } else {
                base.insert(k, v);
            }
        }
    }
    claims
}

fn fail(op: &MockOp, token: &str, keys: &Keys) -> String {
    let err = verifier()
        .verify_id_token(token, keys, &expected(op))
        .expect_err("must fail");
    assert_eq!(err.code(), ErrorCode::IdTokenInvalid, "{err}");
    reason(&err)
}

/// A JWS with this header and claims and a garbage signature (for the
/// header checks that run before any key is used).
fn unsigned(header: Value, claims: &Value) -> String {
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
        URL_SAFE_NO_PAD.encode([7u8; 64])
    )
}

#[tokio::test]
async fn valid_rs256_and_es256_tokens_verify() {
    let op = MockOp::start().await;
    let ks = keys(&op);
    for kid in [RSA_KID, EC_KID] {
        let token = op.sign(kid, json!({}), &op.claims(NONCE));
        let claims = verifier()
            .verify_id_token(&token, &ks, &expected(&op))
            .expect(kid);
        assert_eq!(claims.subject, "synthetic-sub-alice");
        assert_eq!(claims.sid.as_deref(), Some("synthetic-sid-1"));
    }
}

/// The errors.toml test of FG-AUTH-0012: each claim / signature failure.
#[tokio::test]
async fn id_token_failures_are_id_token_invalid() {
    let op = MockOp::start().await;
    let ks = keys(&op);
    let base = op.claims(NONCE);
    let t = now();
    let cases: Vec<(Value, &str)> = vec![
        (json!({"nonce": "other"}), "nonce"),
        (json!({"nonce": null}), "nonce"),
        (json!({"aud": "another-client"}), "audience"),
        (json!({"aud": [CLIENT_ID, "other"], "azp": "other"}), "azp"),
        (json!({"aud": [CLIENT_ID, "other"], "azp": null}), "azp"),
        (json!({"iss": "http://127.0.0.1:1/realms/other"}), "issuer"),
        (json!({"exp": t - 61}), "expired"),
        (json!({"iat": t + 120}), "not_yet_valid"),
        (json!({"nbf": t + 120}), "not_yet_valid"),
        (json!({"iat": null}), "missing_claim"),
        (json!({"exp": null}), "missing_claim"),
        (json!({"sub": ""}), "subject"),
        (json!({"sub": "x".repeat(256)}), "subject"),
    ];
    for (extra, expected_reason) in cases {
        let token = op.sign(RSA_KID, json!({}), &with(base.clone(), extra.clone()));
        assert_eq!(fail(&op, &token, &ks), expected_reason, "{extra}");
    }
    // Within the 60 s skew: accepted.
    for extra in [json!({"exp": t - 30}), json!({"iat": t + 30})] {
        let token = op.sign(RSA_KID, json!({}), &with(base.clone(), extra.clone()));
        verifier()
            .verify_id_token(&token, &ks, &expected(&op))
            .expect("within skew");
    }
    // Tampered payload, signature kept.
    let good = op.sign(RSA_KID, json!({}), &base);
    let parts: Vec<&str> = good.split('.').collect();
    let forged = URL_SAFE_NO_PAD.encode(with(base.clone(), json!({"sub": "mallory"})).to_string());
    let tampered = format!("{}.{forged}.{}", parts[0], parts[2]);
    assert_eq!(fail(&op, &tampered, &ks), "signature");
    // Signed by the EC key but naming the RSA kid.
    let ec = op.sign(EC_KID, json!({"kid": RSA_KID, "alg": "RS256"}), &base);
    assert_eq!(fail(&op, &ec, &ks), "signature");
}

#[tokio::test]
async fn algorithms_and_keys_outside_the_rules_are_refused_without_panic() {
    let op = MockOp::start().await;
    let ks = keys(&op);
    let claims = op.claims(NONCE);
    for alg in ["none", "HS256", "PS256", "EdDSA", ""] {
        let token = unsigned(json!({"alg": alg, "kid": RSA_KID}), &claims);
        assert_eq!(fail(&op, &token, &ks), "alg_not_allowed", "{alg}");
    }
    // HS256 with the RSA public key as the HMAC secret never verifies.
    let token = unsigned(json!({"alg": "HS256", "kid": RSA_KID}), &claims);
    assert_eq!(fail(&op, &token, &ks), "alg_not_allowed");
    assert_eq!(
        fail(&op, &unsigned(json!({"alg": "RS256"}), &claims), &ks),
        "kid_missing"
    );
    assert_eq!(
        fail(
            &op,
            &unsigned(json!({"alg": "RS256", "kid": "nope"}), &claims),
            &ks
        ),
        "unknown_kid"
    );
    // The kty / alg pairing.
    assert_eq!(
        fail(
            &op,
            &unsigned(json!({"alg": "ES256", "kid": RSA_KID}), &claims),
            &ks
        ),
        "key_alg_mismatch"
    );
    assert_eq!(
        fail(
            &op,
            &unsigned(json!({"alg": "RS256", "kid": EC_KID}), &claims),
            &ks
        ),
        "key_alg_mismatch"
    );
    // An allow-list of RS256 only refuses ES256.
    let rs_only = JwtVerifier::new(&[OidcSigningAlg::Rs256]).expect("second verifier");
    let es = op.sign(EC_KID, json!({}), &claims);
    let err = rs_only
        .verify_id_token(&es, &ks, &expected(&op))
        .expect_err("ES256 not allowed");
    assert_eq!(reason(&err), "alg_not_allowed");
    for malformed in ["", "a.b", "a.b.c.d", "!!!.e30.e30"] {
        assert_eq!(fail(&op, malformed, &ks), "malformed", "{malformed}");
    }
}

/// Broken key material marks its kid as `key_invalid` with a detail.
#[tokio::test]
async fn broken_jwk_material_is_key_invalid_with_its_detail() {
    let op = MockOp::start().await;
    let real = op.jwks_json();
    let rsa = real["keys"][0].clone();
    let ec = real["keys"][1].clone();
    let n = URL_SAFE_NO_PAD.encode([0xC5u8; 128]); // 1024 bits
    let mut off_curve = vec![0u8; 32];
    off_curve[31] = 1;
    let cases: Vec<(Value, &str, &str)> = vec![
        (
            with(rsa.clone(), json!({"n": n})),
            "RS256",
            "rsa_modulus_size",
        ),
        (with(rsa.clone(), json!({"n": "***"})), "RS256", "base64"),
        (
            with(rsa.clone(), json!({"e": "AAAC"})),
            "RS256",
            "rsa_exponent",
        ),
        (with(rsa.clone(), json!({"use": "enc"})), "RS256", "use"),
        (with(rsa.clone(), json!({"kty": "oct"})), "RS256", "kty"),
        (with(ec.clone(), json!({"crv": "P-384"})), "ES256", "crv"),
        (
            with(ec.clone(), json!({"x": URL_SAFE_NO_PAD.encode([1u8; 31])})),
            "ES256",
            "ec_coordinate_length",
        ),
        (
            with(
                ec.clone(),
                json!({"x": URL_SAFE_NO_PAD.encode(&off_curve), "y": URL_SAFE_NO_PAD.encode(&off_curve)}),
            ),
            "ES256",
            "ec_point",
        ),
    ];
    for (jwk, alg, detail) in cases {
        let kid = jwk["kid"].as_str().expect("kid").to_owned();
        let ks = Keys::parse(json!({"keys": [jwk]}).to_string().as_bytes()).expect("parses");
        let token = unsigned(json!({"alg": alg, "kid": kid}), &op.claims(NONCE));
        let err = verifier()
            .verify_id_token(&token, &ks, &expected(&op))
            .expect_err(detail);
        assert_eq!(reason(&err), "key_invalid", "{detail}: {err}");
        assert!(
            err.to_string().contains(&format!("detail={detail}")),
            "{err}"
        );
    }
    // A JWK alg that disagrees with the token's.
    let ks = Keys::parse(
        json!({"keys": [with(rsa, json!({"alg": "RS512"}))]})
            .to_string()
            .as_bytes(),
    )
    .expect("parses");
    let token = op.sign(RSA_KID, json!({}), &op.claims(NONCE));
    let err = verifier()
        .verify_id_token(&token, &ks, &expected(&op))
        .expect_err("alg");
    assert_eq!(reason(&err), "key_alg_mismatch");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Arbitrary bytes as a JWKS never panic.
    #[test]
    fn arbitrary_jwks_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Keys::parse(&bytes);
    }

    /// Arbitrary JWK fields never panic; a broken key is never usable.
    #[test]
    fn broken_jwks_fields_never_panic(
        kty in prop_oneof![Just("RSA".to_owned()), Just("EC".to_owned()), Just("oct".to_owned()), ".{0,4}"],
        n in "[A-Za-z0-9_*-]{0,400}",
        e in "[A-Za-z0-9_-]{0,8}",
        crv in prop_oneof![Just("P-256".to_owned()), Just("P-384".to_owned()), ".{0,6}"],
        x in "[A-Za-z0-9_-]{0,50}",
        y in "[A-Za-z0-9_-]{0,50}",
    ) {
        let doc = json!({"keys": [{"kid": "k", "kty": kty, "n": n, "e": e, "crv": crv, "x": x, "y": y}]});
        let ks = Keys::parse(doc.to_string().as_bytes()).expect("a JWKS document");
        let token = unsigned(json!({"alg": "RS256", "kid": "k"}), &json!({"sub": "s"}));
        let verified = verifier().verify_id_token(&token, &ks, &Expected {
            issuer: "http://127.0.0.1:1/realms/x",
            client_id: CLIENT_ID,
            nonce: None,
            now: 0,
            leeway: 60,
        });
        prop_assert!(verified.is_err());
    }

    /// Arbitrary token text never panics and never verifies.
    #[test]
    fn arbitrary_jws_never_panic(token in ".{0,300}", dots in "[A-Za-z0-9_.=-]{0,300}") {
        let ks = Keys::default();
        let exp = Expected {
            issuer: "http://127.0.0.1:1/realms/x",
            client_id: CLIENT_ID,
            nonce: Some("n"),
            now: 0,
            leeway: 60,
        };
        for t in [token.as_str(), dots.as_str()] {
            let err = verifier().verify_id_token(t, &ks, &exp).expect_err("never valid");
            prop_assert_eq!(err.code(), ErrorCode::IdTokenInvalid);
        }
    }
}
