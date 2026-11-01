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

use crate::error::{id_token, internal, loggable};
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
    fn header(&self, token: &str) -> Result<(OidcSigningAlg, String), ForgeError> {
        if token.len() > MAX_TOKEN_BYTES || token.split('.').count() != 3 {
            return Err(id_token("malformed"));
        }
        let encoded = token.split('.').next().unwrap_or_default();
        let header: Value = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| id_token("malformed"))?;
        let alg_name = header.get("alg").and_then(Value::as_str).unwrap_or("");
        let alg = OidcSigningAlg::parse(alg_name)
            .filter(|a| self.allowed.contains(a))
            .ok_or_else(|| id_token("alg_not_allowed").with_context("alg", loggable(alg_name)))?;
        let kid = header
            .get("kid")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| id_token("kid_missing"))?;
        Ok((alg, kid.to_owned()))
    }

    /// The `kid` a token names, after the header checks (the JWKS lookup
    /// needs it before verification).
    pub fn kid(&self, token: &str) -> Result<String, ForgeError> {
        self.header(token).map(|(_, kid)| kid)
    }

    /// Verify an ID token against `keys` and `expected` (ADR-0021 §9).
    pub fn verify_id_token(
        &self,
        token: &str,
        keys: &KeySet,
        expected: &Expected<'_>,
    ) -> Result<IdClaims, ForgeError> {
        let (alg, kid) = self.header(token)?;
        let entry = keys.get(&kid).ok_or_else(|| id_token("unknown_kid"))?;
        let key = entry
            .key
            .as_ref()
            .map_err(|detail| id_token("key_invalid").with_context("detail", detail))?;
        let (algorithm, decoding) = match (alg, key) {
            (OidcSigningAlg::Rs256, VerifyKey::Rsa { n, e }) => {
                (Algorithm::RS256, DecodingKey::from_rsa_raw_components(n, e))
            }
            (OidcSigningAlg::Es256, VerifyKey::P256 { point }) => {
                (Algorithm::ES256, DecodingKey::from_ec_der(point))
            }
            (OidcSigningAlg::Rs256 | OidcSigningAlg::Es256, _) => {
                return Err(mismatch(alg, key));
            }
        };
        if entry.alg.as_deref().is_some_and(|a| a != alg.as_str()) {
            return Err(mismatch(alg, key));
        }
        let mut validation = Validation::new(algorithm);
        validation.algorithms = vec![algorithm];
        validation.leeway = expected.leeway;
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.validate_aud = true;
        validation.aud = Some(HashSet::from([expected.client_id.to_owned()]));
        validation.iss = Some(HashSet::from([expected.issuer.to_owned()]));
        validation.required_spec_claims = ["exp", "iss", "aud", "sub"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let data = jsonwebtoken::decode::<Value>(token, &decoding, &validation)
            .map_err(|e| from_library(&e))?;
        check_claims(data.claims, expected)
    }
}

fn mismatch(alg: OidcSigningAlg, key: &VerifyKey) -> ForgeError {
    id_token("key_alg_mismatch")
        .with_context("alg", alg.as_str())
        .with_context("kty", key.kty())
}

/// A library error as the reason it means.
fn from_library(e: &JwtError) -> ForgeError {
    match e.kind() {
        ErrorKind::InvalidSignature => id_token("signature"),
        ErrorKind::ExpiredSignature => id_token("expired"),
        ErrorKind::ImmatureSignature => id_token("not_yet_valid"),
        ErrorKind::InvalidIssuer => id_token("issuer"),
        ErrorKind::InvalidAudience => id_token("audience"),
        ErrorKind::MissingRequiredClaim(claim) => {
            id_token("missing_claim").with_context("claim", loggable(claim))
        }
        ErrorKind::InvalidClaimFormat(claim) => {
            id_token("malformed").with_context("claim", loggable(claim))
        }
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => {
            id_token("alg_not_allowed")
        }
        ErrorKind::InvalidKeyFormat | ErrorKind::InvalidEcdsaKey | ErrorKind::InvalidRsaKey(_) => {
            id_token("key_invalid").with_context("detail", "format")
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
        | ErrorKind::Provider(_) => id_token("malformed"),
        // `ErrorKind` is non-exhaustive: a kind added later is a refusal too.
        _ => id_token("malformed").with_context("detail", "unknown_kind"),
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
