//! JWS verification (ADR-0021 §9, §15 option B): `jsonwebtoken` parses the
//! token and validates `exp` / `nbf` / `iss` / `aud`; the signature is
//! checked by forge's own ring [`CryptoProvider`] — RS256
//! (`RSA_PKCS1_2048_8192_SHA256`) and ES256 (`ECDSA_P256_SHA256_FIXED`)
//! only, verification only, every function returning a `Result`.
//!
//! The panic path of `jsonwebtoken` 11 (verifying without an installed
//! provider) is closed by construction: the only way to verify is a
//! [`JwtVerifier`], and only [`JwtVerifier::new`] installs the provider. A
//! different provider already installed in the process is `FG-SYS-0001
//! reason=jwt_provider_conflict` (the start stops).
//!
//! Before the library sees a token, the header is read here: `alg` must
//! be in the allow-list (`alg_not_allowed`), `kid` must be present
//! (`kid_missing`), and the key's kty must pair with `alg`
//! (`key_alg_mismatch`), so `none`, HS256 with an RSA key as the secret,
//! PS256 and cross-type keys never reach a verifier.

use std::collections::HashSet;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_core::config::OidcSigningAlg;
use forge_errors::ForgeError;
use jsonwebtoken::crypto::{CryptoProvider, JwtSigner, JwtVerifier as JwsVerify, KeyUtils};
use jsonwebtoken::errors::{Error as JwtError, ErrorKind};
use jsonwebtoken::jwk::{EllipticCurve, ThumbprintHash};
use jsonwebtoken::signature;
use jsonwebtoken::{Algorithm, DecodingKey, DecodingKeyKind, EncodingKey, Validation};
use serde_json::Value;

use crate::error::{id_token, internal, loggable, logout_token};
use crate::keys::{KeySet, VerifyKey};

/// The longest token accepted (a Keycloak ID token is a few KiB).
const MAX_TOKEN_BYTES: usize = 64 * 1024;

fn unsupported() -> JwtError {
    ErrorKind::Provider("forge-oidc: verification only (RS256, ES256)".to_owned()).into()
}

fn no_signer(_: &Algorithm, _: &EncodingKey) -> jsonwebtoken::errors::Result<Box<dyn JwtSigner>> {
    Err(unsupported())
}

fn verifier(
    alg: &Algorithm,
    key: &DecodingKey,
) -> jsonwebtoken::errors::Result<Box<dyn JwsVerify>> {
    match (alg, key.kind()) {
        (Algorithm::RS256, DecodingKeyKind::RsaModulusExponent { n, e }) => Ok(Box::new(Rs256 {
            n: n.clone(), // clone: the verifier owns its key for the call
            e: e.clone(),
        })),
        (Algorithm::ES256, DecodingKeyKind::SecretOrDer(point)) if point.len() == 65 => {
            Ok(Box::new(Es256 {
                point: point.clone(), // clone: as above
            }))
        }
        (Algorithm::RS256, _) => Err(ErrorKind::InvalidKeyFormat.into()),
        (Algorithm::ES256, _) => Err(ErrorKind::InvalidEcdsaKey.into()),
        _ => Err(ErrorKind::InvalidAlgorithm.into()),
    }
}

fn no_components(_: &[u8]) -> jsonwebtoken::errors::Result<(Vec<u8>, Vec<u8>)> {
    Err(unsupported())
}

fn no_ec_components(
    _: &[u8],
    _: Algorithm,
) -> jsonwebtoken::errors::Result<(EllipticCurve, Vec<u8>, Vec<u8>)> {
    Err(unsupported())
}

fn no_ed_components(_: &[u8], _: &EllipticCurve) -> jsonwebtoken::errors::Result<Vec<u8>> {
    Err(unsupported())
}

fn no_digest(_: &[u8], _: ThumbprintHash) -> jsonwebtoken::errors::Result<Vec<u8>> {
    Err(unsupported())
}

/// forge's provider: ring verification of RS256 / ES256, nothing else.
static PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: no_signer,
    verifier_factory: verifier,
    key_utils: KeyUtils {
        rsa_pub_components_from_private_key: no_components,
        rsa_pub_components_from_public_key: no_components,
        ec_pub_components_from_private_key: no_ec_components,
        ed_pub_components_from_private_key: no_ed_components,
        compute_digest: no_digest,
    },
};

struct Rs256 {
    n: Vec<u8>,
    e: Vec<u8>,
}

impl signature::Verifier<Vec<u8>> for Rs256 {
    fn verify(&self, msg: &[u8], signature: &Vec<u8>) -> Result<(), signature::Error> {
        ring::signature::RsaPublicKeyComponents {
            n: &self.n,
            e: &self.e,
        }
        .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg, signature)
        .map_err(|_| signature::Error::new())
    }
}

impl JwsVerify for Rs256 {
    fn algorithm(&self) -> Algorithm {
        Algorithm::RS256
    }
}

struct Es256 {
    point: Vec<u8>,
}

impl signature::Verifier<Vec<u8>> for Es256 {
    fn verify(&self, msg: &[u8], signature: &Vec<u8>) -> Result<(), signature::Error> {
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            &self.point,
        )
        .verify(msg, signature)
        .map_err(|_| signature::Error::new())
    }
}

impl JwsVerify for Es256 {
    fn algorithm(&self) -> Algorithm {
        Algorithm::ES256
    }
}

/// What a token must satisfy besides its signature.
#[derive(Debug, Clone)]
pub struct Expected<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    /// `Some` at the login (the state's nonce).
    pub nonce: Option<&'a str>,
    /// Seconds since the epoch (the caller's clock).
    pub now: u64,
    /// The allowed clock skew in seconds (60).
    pub leeway: u64,
}

/// The verified claims the sign-in uses. `claims` keeps the whole payload
/// (the employee number and the roles are read from it by name).
#[derive(Debug, Clone)]
pub struct IdClaims {
    pub subject: String,
    pub sid: Option<String>,
    pub claims: Value,
}

/// The only way to verify a JWS in forge (see the module documentation).
#[derive(Debug)]
pub struct JwtVerifier {
    allowed: Vec<OidcSigningAlg>,
}

impl JwtVerifier {
    /// Install forge's provider (once per process; a second verifier
    /// reuses it) and allow `algs`.
    pub fn new(algs: &[OidcSigningAlg]) -> Result<Self, ForgeError> {
        if let Err(existing) = CryptoProvider::install_default(&PROVIDER) {
            if !std::ptr::eq(existing, &PROVIDER) {
                return Err(internal("jwt_provider_conflict"));
            }
        }
        Ok(Self {
            allowed: algs.to_vec(),
        })
    }

    /// The header's `alg` and `kid`, checked against the allow-list.
    fn header(&self, token: &str, fail: Fail) -> Result<(OidcSigningAlg, String), ForgeError> {
        if token.len() > MAX_TOKEN_BYTES || token.split('.').count() != 3 {
            return Err(fail("malformed"));
        }
        let encoded = token.split('.').next().unwrap_or_default();
        let header: Value = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| fail("malformed"))?;
        let alg_name = header.get("alg").and_then(Value::as_str).unwrap_or("");
        let alg = OidcSigningAlg::parse(alg_name)
            .filter(|a| self.allowed.contains(a))
            .ok_or_else(|| fail("alg_not_allowed").with_context("alg", loggable(alg_name)))?;
        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| fail("kid_missing"))?;
        Ok((alg, kid.to_owned()))
    }

    /// The `kid` a token names, after the header checks (the JWKS lookup
    /// needs it before verification).
    pub fn kid(&self, token: &str) -> Result<String, ForgeError> {
        self.header(token, id_token).map(|(_, kid)| kid)
    }

    /// [`Self::kid`] for a back-channel logout token (`FG-AUTH-0017`).
    pub fn logout_kid(&self, token: &str) -> Result<String, ForgeError> {
        self.header(token, logout_token).map(|(_, kid)| kid)
    }

    /// Verify an ID token against `keys` and `expected` (ADR-0021 §9).
    pub fn verify_id_token(
        &self,
        token: &str,
        keys: &KeySet,
        expected: &Expected<'_>,
    ) -> Result<IdClaims, ForgeError> {
        let claims = self.verified_payload(token, keys, expected, id_token, true)?;
        check_claims(claims, expected)
    }

    /// Verify a back-channel logout token (ADR-0021 §8, §9): the ID
    /// token's signature, key and `iss` / `aud` rules, then the logout
    /// token's own claims ([`check_logout_claims`]). Every refusal is
    /// `FG-AUTH-0017` with the reason. `expected.nonce` is ignored (a
    /// logout token must have none).
    pub fn verify_logout_token(
        &self,
        token: &str,
        keys: &KeySet,
        expected: &Expected<'_>,
    ) -> Result<LogoutClaims, ForgeError> {
        let claims = self.verified_payload(token, keys, expected, logout_token, false)?;
        check_logout_claims(&claims, expected)
    }

    /// The signature, the key and the library's claim checks. `exp` (and
    /// `sub`) are required of an ID token (`id`); a logout token's `exp`
    /// is optional and checked by [`check_logout_claims`] when present.
    fn verified_payload(
        &self,
        token: &str,
        keys: &KeySet,
        expected: &Expected<'_>,
        fail: Fail,
        id: bool,
    ) -> Result<Value, ForgeError> {
        let (alg, kid) = self.header(token, fail)?;
        let entry = keys.get(&kid).ok_or_else(|| fail("unknown_kid"))?;
        let key = entry
            .key
            .as_ref()
            .map_err(|detail| fail("key_invalid").with_context("detail", detail))?;
        let (algorithm, decoding) = match (alg, key) {
            (OidcSigningAlg::Rs256, VerifyKey::Rsa { n, e }) => {
                (Algorithm::RS256, DecodingKey::from_rsa_raw_components(n, e))
            }
            (OidcSigningAlg::Es256, VerifyKey::P256 { point }) => {
                (Algorithm::ES256, DecodingKey::from_ec_der(point))
            }
            (OidcSigningAlg::Rs256 | OidcSigningAlg::Es256, _) => {
                return Err(mismatch(alg, key, fail));
            }
        };
        if entry.alg.as_deref().is_some_and(|a| a != alg.as_str()) {
            return Err(mismatch(alg, key, fail));
        }
        let mut validation = Validation::new(algorithm);
        validation.algorithms = vec![algorithm];
        validation.leeway = expected.leeway;
        validation.validate_exp = id;
        validation.validate_nbf = true;
        validation.validate_aud = true;
        validation.aud = Some(HashSet::from([expected.client_id.to_owned()]));
        validation.iss = Some(HashSet::from([expected.issuer.to_owned()]));
        let required: &[&str] = if id {
            &["exp", "iss", "aud", "sub"]
        } else {
            &["iss", "aud"]
        };
        validation.required_spec_claims = required.iter().map(|c| (*c).to_owned()).collect();
        let data = jsonwebtoken::decode::<Value>(token, &decoding, &validation)
            .map_err(|e| from_library(&e, fail))?;
        Ok(data.claims)
    }
}

/// The error constructor of the token being verified: `FG-AUTH-0012` for
/// an ID token, `FG-AUTH-0017` for a logout token.
type Fail = fn(&'static str) -> ForgeError;

fn mismatch(alg: OidcSigningAlg, key: &VerifyKey, fail: Fail) -> ForgeError {
    fail("key_alg_mismatch")
        .with_context("alg", alg.as_str())
        .with_context("kty", key.kty())
}

/// A library error as the reason it means.
fn from_library(e: &JwtError, fail: Fail) -> ForgeError {
    match e.kind() {
        ErrorKind::InvalidSignature => fail("signature"),
        ErrorKind::ExpiredSignature => fail("expired"),
        ErrorKind::ImmatureSignature => fail("not_yet_valid"),
        ErrorKind::InvalidIssuer => fail("issuer"),
        ErrorKind::InvalidAudience => fail("audience"),
        ErrorKind::MissingRequiredClaim(claim) => {
            fail("missing_claim").with_context("claim", loggable(claim))
        }
        ErrorKind::InvalidClaimFormat(claim) => {
            fail("malformed").with_context("claim", loggable(claim))
        }
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => fail("alg_not_allowed"),
        ErrorKind::InvalidKeyFormat | ErrorKind::InvalidEcdsaKey | ErrorKind::InvalidRsaKey(_) => {
            fail("key_invalid").with_context("detail", "format")
        }
        ErrorKind::InvalidToken
        | ErrorKind::InvalidEddsaKey
        | ErrorKind::RsaFailedSigning
        | ErrorKind::Signing(_)
        | ErrorKind::UnsupportedAlgorithm
        | ErrorKind::InvalidSubject
        | ErrorKind::MissingAlgorithm
        | ErrorKind::Base64(_)
        | ErrorKind::Json(_)
        | ErrorKind::Utf8(_)
        | ErrorKind::Provider(_) => fail("malformed"),
        // `ErrorKind` is non-exhaustive: a kind added later is a refusal too.
        _ => fail("malformed").with_context("detail", "unknown_kind"),
    }
}

/// The claims the library does not check: `iat` (required, not in the
/// future beyond the skew), `azp` with several audiences, `nonce`, `sub`.
fn check_claims(claims: Value, expected: &Expected<'_>) -> Result<IdClaims, ForgeError> {
    let iat = claims
        .get("iat")
        .and_then(Value::as_u64)
        .ok_or_else(|| id_token("missing_claim").with_context("claim", "iat"))?;
    if iat > expected.now.saturating_add(expected.leeway) {
        return Err(id_token("not_yet_valid").with_context("claim", "iat"));
    }
    let several = claims
        .get("aud")
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() > 1);
    let azp = claims.get("azp").and_then(Value::as_str);
    if (several || azp.is_some()) && azp != Some(expected.client_id) {
        return Err(id_token("azp"));
    }
    if let Some(nonce) = expected.nonce {
        let presented = claims.get("nonce").and_then(Value::as_str).unwrap_or("");
        let same = presented.len() == nonce.len()
            && bool::from(subtle::ConstantTimeEq::ct_eq(
                presented.as_bytes(),
                nonce.as_bytes(),
            ));
        if !same {
            return Err(id_token("nonce"));
        }
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().count() <= 255 && !s.contains('\0'))
        .ok_or_else(|| id_token("subject"))?
        .to_owned();
    let sid = claims
        .get("sid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().count() <= 255 && !s.contains('\0'))
        .map(str::to_owned);
    Ok(IdClaims {
        subject,
        sid,
        claims,
    })
}

/// The event a back-channel logout token carries (OIDC Back-Channel
/// Logout 1.0 §2.4).
pub const BACKCHANNEL_LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// A logout token older than this is refused (`too_old`, ADR-0021 §8).
pub const LOGOUT_MAX_AGE_SECS: u64 = 120;

/// The verified claims of a back-channel logout token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutClaims {
    pub jti: String,
    pub sid: Option<String>,
    pub sub: Option<String>,
    /// `iat`, seconds since the epoch.
    pub iat: u64,
}

/// A short identifier claim: 1..=255 characters, no NUL.
fn short_claim(claims: &Value, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().count() <= 255 && !s.contains('\0'))
        .map(str::to_owned)
}

/// The logout token's own rules (ADR-0021 §8): `iat` required, not in
/// the future beyond the skew and at most 2 minutes old; `exp`, when
/// present, not passed beyond the skew; the back-channel event in
/// `events`; `sid` or `sub`; no `nonce`; a `jti`. `azp` follows the ID
/// token's rule.
fn check_logout_claims(
    claims: &Value,
    expected: &Expected<'_>,
) -> Result<LogoutClaims, ForgeError> {
    let iat = claims
        .get("iat")
        .and_then(Value::as_u64)
        .ok_or_else(|| logout_token("missing_claim").with_context("claim", "iat"))?;
    if iat > expected.now.saturating_add(expected.leeway) {
        return Err(logout_token("not_yet_valid").with_context("claim", "iat"));
    }
    if expected.now.saturating_sub(iat) > LOGOUT_MAX_AGE_SECS + expected.leeway {
        return Err(logout_token("too_old"));
    }
    match claims.get("exp") {
        None => {}
        Some(exp) => {
            let exp = exp
                .as_u64()
                .ok_or_else(|| logout_token("malformed").with_context("claim", "exp"))?;
            if exp.saturating_add(expected.leeway) < expected.now {
                return Err(logout_token("expired"));
            }
        }
    }
    let several = claims
        .get("aud")
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() > 1);
    let azp = claims.get("azp").and_then(Value::as_str);
    if (several || azp.is_some()) && azp != Some(expected.client_id) {
        return Err(logout_token("audience").with_context("claim", "azp"));
    }
    let events = claims
        .get("events")
        .ok_or_else(|| logout_token("missing_claim").with_context("claim", "events"))?;
    if !events.as_object().is_some_and(|e| {
        e.get(BACKCHANNEL_LOGOUT_EVENT)
            .is_some_and(Value::is_object)
    }) {
        return Err(logout_token("events"));
    }
    if claims.get("nonce").is_some() {
        return Err(logout_token("nonce_present"));
    }
    let jti = short_claim(claims, "jti")
        .ok_or_else(|| logout_token("missing_claim").with_context("claim", "jti"))?;
    let sid = short_claim(claims, "sid");
    let sub = short_claim(claims, "sub");
    if sid.is_none() && sub.is_none() {
        return Err(logout_token("missing_claim").with_context("claim", "sid_or_sub"));
    }
    Ok(LogoutClaims { jti, sid, sub, iat })
}

#[cfg(test)]
mod logout_tests {
    use forge_errors::ErrorCode;
    use serde_json::json;

    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn expected() -> Expected<'static> {
        Expected {
            issuer: "https://sso.example.test/realms/iot",
            client_id: "gitstation",
            nonce: None,
            now: NOW,
            leeway: 60,
        }
    }

    fn good() -> Value {
        json!({
            "iss": "https://sso.example.test/realms/iot",
            "aud": "gitstation",
            "iat": NOW,
            "jti": "synthetic-jti-1",
            "sid": "synthetic-sid-1",
            "events": { BACKCHANNEL_LOGOUT_EVENT: {} },
        })
    }

    fn reason(mut claims: Value, key: &str, value: Option<Value>) -> String {
        match value {
            Some(v) => claims[key] = v,
            None => {
                claims.as_object_mut().expect("object").remove(key);
            }
        }
        let err = check_logout_claims(&claims, &expected()).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::LogoutTokenInvalid);
        err.to_string()
    }

    #[test]
    fn a_good_logout_token_names_its_session() {
        let claims = check_logout_claims(&good(), &expected()).expect("good");
        assert_eq!(claims.jti, "synthetic-jti-1");
        assert_eq!(claims.sid.as_deref(), Some("synthetic-sid-1"));
        let mut by_sub = good();
        by_sub.as_object_mut().expect("object").remove("sid");
        by_sub["sub"] = json!("synthetic-sub-1");
        let claims = check_logout_claims(&by_sub, &expected()).expect("sub only");
        assert_eq!(
            (claims.sid, claims.sub.as_deref()),
            (None, Some("synthetic-sub-1"))
        );
    }

    #[test]
    fn every_logout_claim_failure_has_its_reason() {
        let cases: Vec<(&str, Option<Value>, &str)> = vec![
            ("iat", None, "reason=missing_claim"),
            ("iat", Some(json!(NOW + 61)), "reason=not_yet_valid"),
            ("iat", Some(json!(NOW - 181)), "reason=too_old"),
            ("exp", Some(json!(NOW - 61)), "reason=expired"),
            ("exp", Some(json!("soon")), "reason=malformed"),
            ("events", None, "claim=events"),
            ("events", Some(json!({"other": {}})), "reason=events"),
            (
                "events",
                Some(json!({ BACKCHANNEL_LOGOUT_EVENT: "x" })),
                "reason=events",
            ),
            ("nonce", Some(json!("n")), "reason=nonce_present"),
            ("jti", None, "claim=jti"),
            ("jti", Some(json!("")), "claim=jti"),
            ("sid", None, "claim=sid_or_sub"),
            ("azp", Some(json!("someone-else")), "claim=azp"),
            ("aud", Some(json!(["gitstation", "other"])), "claim=azp"),
        ];
        for (key, value, want) in cases {
            let got = reason(good(), key, value.clone());
            assert!(got.contains(want), "{key}={value:?}: {got}");
        }
        // Within the skew and the age both pass.
        let mut edge = good();
        edge["iat"] = json!(NOW - 180);
        edge["exp"] = json!(NOW - 59);
        check_logout_claims(&edge, &expected()).expect("at the edges");
    }
}
