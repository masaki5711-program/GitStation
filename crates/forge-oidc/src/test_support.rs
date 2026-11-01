//! The in-process mock identity provider (ADR-0021 §14; feature
//! `test-support`, dev-dependency only): axum on `127.0.0.1:0` with
//! discovery, JWKS and the token endpoint (`client_secret_basic`, PKCE
//! S256, single-use codes), its signing keys (ES256 made with ring per
//! process, RS256 from the synthetic fixture), and fault injection (delay,
//! status, broken JSON, replaced keys). The browser's visit to the
//! authorization endpoint is [`MockOp::authorize`]: it reads the
//! authorization URL forge built and issues a code for chosen claims.
//! The refresh grant rotates refresh tokens and can answer without an ID
//! token ([`RefreshMode`]); [`MockOp::logout_token`] signs back-channel
//! logout tokens (M2 MR5). Everything here is synthetic.
#![allow(clippy::expect_used, clippy::panic)] // test helpers: failing loudly is the point

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use forge_core::config::OidcSigningAlg;
use forge_core::web_keys::WebKeys;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, RsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::flow::{now_secs, OidcSettings, Timeouts};
use crate::jwks::JwksTiming;

pub const CLIENT_ID: &str = "gitstation";
/// Synthetic, never a real secret.
pub const CLIENT_SECRET: &str = "synthetic-client-secret-for-tests";
pub const RSA_KID: &str = "synthetic-rsa-1";
pub const EC_KID: &str = "synthetic-ec-1";
/// The synthetic `web.session_key_file` content.
pub const WEB_KEYS: &str = "1 1111111111111111111111111111111111111111111111111111111111111111\n";

const RSA_PEM: &str = include_str!("../tests/fixtures/synthetic-rs256-key.pem");
const DISCOVERY_PATH: &str = "/realms/synthetic/.well-known/openid-configuration";
const JWKS_PATH: &str = "/realms/synthetic/certs";
const TOKEN_PATH: &str = "/realms/synthetic/token";

/// An endpoint of the mock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    Discovery,
    Jwks,
    Token,
}

/// A fault an endpoint injects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Answer after this long.
    Delay(Duration),
    /// Answer with this status (and a JSON error body).
    Status(u16),
    /// Answer 200 with a body that is not JSON.
    InvalidJson,
    /// Answer with this status and this OAuth `error` (RFC 6749 §5.2).
    OAuthError(u16, &'static str),
}

/// How the token endpoint answers `grant_type=refresh_token` (M2 MR5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// A new ID token (the sign-in's claims, fresh times, the refresh
    /// overrides merged) and a rotated refresh token.
    WithIdToken,
    /// A rotated refresh token and no ID token.
    WithoutIdToken,
}

struct Pending {
    claims: Value,
    challenge: String,
    redirect_uri: String,
}

enum Signer {
    Rsa(RsaKeyPair),
    Ec(EcdsaKeyPair),
}

struct Inner {
    issuer: String,
    codes: HashMap<String, Pending>,
    faults: HashMap<Endpoint, Fault>,
    hits: HashMap<Endpoint, usize>,
    jwks_override: Option<String>,
    discovery_override: Option<Value>,
    signing_kid: String,
    signers: Vec<(String, Signer)>,
    next_code: u64,
    /// Live refresh tokens → the claims of their sign-in (no nonce).
    refresh_tokens: HashMap<String, Value>,
    refresh_mode: RefreshMode,
    refresh_overrides: Value,
    next_refresh: u64,
    next_jti: u64,
}

/// A running mock provider (stops when dropped).
pub struct MockOp {
    pub issuer: String,
    shared: Arc<Mutex<Inner>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockOp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn pem_der(pem: &str) -> Vec<u8> {
    let body: String = pem
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    STANDARD.decode(body).expect("fixture PEM")
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The synthetic web keys.
pub fn web_keys() -> WebKeys {
    WebKeys::parse(WEB_KEYS).expect("synthetic web keys")
}

impl MockOp {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock op");
        let port = listener.local_addr().expect("addr").port();
        let issuer = format!("http://127.0.0.1:{port}/realms/synthetic");
        let rng = SystemRandom::new();
        let rsa = RsaKeyPair::from_pkcs8(&pem_der(RSA_PEM)).expect("fixture RSA key");
        let ec_pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("ec key");
        let ec =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, ec_pkcs8.as_ref(), &rng)
                .expect("ec key");
        let shared = Arc::new(Mutex::new(Inner {
            issuer: issuer.clone(),
            codes: HashMap::new(),
            faults: HashMap::new(),
            hits: HashMap::new(),
            jwks_override: None,
            discovery_override: None,
            signing_kid: RSA_KID.to_owned(),
            signers: vec![
                (RSA_KID.to_owned(), Signer::Rsa(rsa)),
                (EC_KID.to_owned(), Signer::Ec(ec)),
            ],
            next_code: 1,
            refresh_tokens: HashMap::new(),
            refresh_mode: RefreshMode::WithIdToken,
            refresh_overrides: json!({}),
            next_refresh: 0,
            next_jti: 1,
        }));
        let app = axum::Router::<Arc<Mutex<Inner>>>::new()
            // The mock is not forge's server: plain axum routes (forge-web's
            // registry-only rule covers forge's own routes).
            .route(DISCOVERY_PATH, axum::routing::get(discovery))
            .route(JWKS_PATH, axum::routing::get(jwks))
            .route(TOKEN_PATH, axum::routing::post(token))
            .with_state(Arc::clone(&shared));
        let task = tokio::spawn(async move {
            // ignore: the mock ends with the test
            let _ = axum::serve(listener, app).await;
        });
        Self {
            issuer,
            shared,
            task,
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.shared.lock().expect("mock state")
    }

    /// Settings for `Oidc::new` against this provider, with short timeouts.
    pub fn settings(&self, base_origin: &str) -> OidcSettings {
        OidcSettings {
            issuer: self.issuer.clone(),
            client_id: CLIENT_ID.to_owned(),
            client_secret: CLIENT_SECRET.to_owned(),
            ca_pem: None,
            employee_number_claim: "employee_number".to_owned(),
            access_role: "access".to_owned(),
            signing_algs: vec![OidcSigningAlg::Rs256, OidcSigningAlg::Es256],
            auto_link: true,
            base_origin: base_origin.to_owned(),
            session_idle: Duration::from_secs(4 * 3600),
            session_absolute: Duration::from_secs(12 * 3600),
            timeouts: Timeouts {
                connect: Duration::from_millis(500),
                fetch: Duration::from_millis(800),
                token: Duration::from_millis(800),
                refresh: Duration::from_millis(800),
                jwks: JwksTiming::default(),
                prefetch_backoff: [Duration::from_millis(10), Duration::from_millis(20)],
            },
        }
    }

    pub fn set_fault(&self, endpoint: Endpoint, fault: Option<Fault>) {
        let mut inner = self.inner();
        match fault {
            Some(f) => inner.faults.insert(endpoint, f),
            None => inner.faults.remove(&endpoint),
        };
    }

    pub fn hits(&self, endpoint: Endpoint) -> usize {
        self.inner().hits.get(&endpoint).copied().unwrap_or(0)
    }

    /// Serve this JWKS document instead of the real keys (`None`: back).
    pub fn set_jwks(&self, body: Option<String>) {
        self.inner().jwks_override = body;
    }

    /// Serve this discovery document instead (`None`: back).
    pub fn set_discovery(&self, doc: Option<Value>) {
        self.inner().discovery_override = doc;
    }

    /// Sign the ID tokens of later codes with this key (`RSA_KID`, `EC_KID`).
    pub fn sign_with(&self, kid: &str) {
        self.inner().signing_kid = kid.to_owned();
    }

    /// How later refreshes answer, with `overrides` merged into a refreshed
    /// ID token's claims (a `null` removes a claim).
    pub fn set_refresh(&self, mode: RefreshMode, overrides: Value) {
        let mut inner = self.inner();
        inner.refresh_mode = mode;
        inner.refresh_overrides = overrides;
    }

    /// End every refresh token (an administrator ended the sessions in
    /// the realm): later refreshes are `invalid_grant`.
    pub fn revoke_refresh_tokens(&self) {
        self.inner().refresh_tokens.clear();
    }

    /// A back-channel logout token signed with the current signing key:
    /// `iss`, `aud`, `iat` now, a fresh `jti`, `sid` synthetic-sid-1 and
    /// the back-channel event, with `overrides` merged (`null` removes).
    pub fn logout_token(&self, overrides: Value) -> String {
        let now = now_secs().expect("clock after 1970");
        let mut inner = self.inner();
        let jti = format!("synthetic-jti-{}", inner.next_jti);
        inner.next_jti += 1;
        let mut claims = json!({
            "iss": inner.issuer,
            "aud": CLIENT_ID,
            "iat": now,
            "jti": jti,
            "sid": "synthetic-sid-1",
            "events": { crate::jwt::BACKCHANNEL_LOGOUT_EVENT: {} },
        });
        merge(&mut claims, overrides);
        let kid = inner.signing_kid.clone();
        sign_with(&inner, &kid, json!({"typ": "logout+jwt"}), &claims)
    }

    /// The JWKS of the mock's keys.
    pub fn jwks_json(&self) -> Value {
        jwks_of(&self.inner())
    }

    /// Sign `claims` with the key `kid` and the given header (`alg` and
    /// `kid` are added unless present).
    pub fn sign(&self, kid: &str, header: Value, claims: &Value) -> String {
        sign_with(&self.inner(), kid, header, claims)
    }

    /// The claims a sign-in gets unless overridden: the synthetic person
    /// alice with the access role.
    pub fn claims(&self, nonce: &str) -> Value {
        let now = now_secs().expect("clock after 1970");
        json!({
            "iss": self.issuer,
            "aud": CLIENT_ID,
            "azp": CLIENT_ID,
            "sub": "synthetic-sub-alice",
            "sid": "synthetic-sid-1",
            "exp": now + 300,
            "iat": now,
            "nonce": nonce,
            "email": "alice@example.test",
            "email_verified": true,
            "preferred_username": "alice",
            "name": "Alice Synthetic",
            "employee_number": "SYN0001",
            "resource_access": { CLIENT_ID: { "roles": ["access"] } },
        })
    }

    /// The browser at the authorization endpoint: read forge's
    /// authorization URL and issue a code whose ID token has the default
    /// claims with `overrides` merged in (a `null` removes a claim).
    /// Returns `(code, state)`.
    pub fn authorize(&self, location: &str, overrides: Value) -> (String, String) {
        let url = url::Url::parse(location).expect("authorization url");
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            q.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
        let nonce = q.get("nonce").expect("nonce").clone();
        let mut claims = self.claims(&nonce);
        merge(&mut claims, overrides);
        let mut inner = self.inner();
        let code = format!("synthetic-code-{}", inner.next_code);
        inner.next_code += 1;
        inner.codes.insert(
            code.clone(),
            Pending {
                claims,
                challenge: q.get("code_challenge").expect("challenge").clone(),
                redirect_uri: q.get("redirect_uri").expect("redirect_uri").clone(),
            },
        );
        (code, q.get("state").expect("state").clone())
    }
}

/// Merge `overrides` into `claims` (a `null` removes a claim).
fn merge(claims: &mut Value, overrides: Value) {
    if let (Some(base), Value::Object(extra)) = (claims.as_object_mut(), overrides) {
        for (k, v) in extra {
            if v.is_null() {
                base.remove(&k);
            } else {
                base.insert(k, v);
            }
        }
    }
}

fn jwks_of(inner: &Inner) -> Value {
    let keys: Vec<Value> = inner
        .signers
        .iter()
        .map(|(kid, signer)| match signer {
            Signer::Rsa(k) => {
                let c = ring::rsa::PublicKeyComponents::<Vec<u8>>::from(k.public());
                json!({"kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig",
                       "n": b64(&c.n), "e": b64(&c.e)})
            }
            Signer::Ec(k) => {
                let p = k.public_key().as_ref();
                json!({"kid": kid, "kty": "EC", "alg": "ES256", "use": "sig", "crv": "P-256",
                       "x": b64(&p[1..33]), "y": b64(&p[33..65])})
            }
        })
        .collect();
    json!({ "keys": keys })
}

fn sign_with(inner: &Inner, kid: &str, mut header: Value, claims: &Value) -> String {
    let (_, signer) = inner
        .signers
        .iter()
        .find(|(k, _)| k == kid)
        .expect("known kid");
    let alg = match signer {
        Signer::Rsa(_) => "RS256",
        Signer::Ec(_) => "ES256",
    };
    if let Some(h) = header.as_object_mut() {
        h.entry("alg").or_insert(json!(alg));
        h.entry("kid").or_insert(json!(kid));
        h.entry("typ").or_insert(json!("JWT"));
    }
    let message = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(claims.to_string().as_bytes())
    );
    let rng = SystemRandom::new();
    let signature = match signer {
        Signer::Rsa(k) => {
            let mut sig = vec![0u8; k.public().modulus_len()];
            k.sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &rng,
                message.as_bytes(),
                &mut sig,
            )
            .expect("rsa sign");
            sig
        }
        Signer::Ec(k) => k
            .sign(&rng, message.as_bytes())
            .expect("ec sign")
            .as_ref()
            .to_vec(),
    };
    format!("{message}.{}", b64(&signature))
}

/// Record the hit and apply the endpoint's fault (`Some` = answer this).
async fn fault(shared: &Arc<Mutex<Inner>>, endpoint: Endpoint) -> Option<Response> {
    let fault = {
        let mut inner = shared.lock().expect("mock state");
        *inner.hits.entry(endpoint).or_insert(0) += 1;
        inner.faults.get(&endpoint).copied()
    };
    match fault {
        None => None,
        Some(Fault::Delay(d)) => {
            tokio::time::sleep(d).await;
            None
        }
        Some(Fault::OAuthError(s, error)) => Some(
            (
                StatusCode::from_u16(s).expect("status"),
                axum::Json(json!({"error": error})),
            )
                .into_response(),
        ),
        Some(Fault::Status(s)) => Some(
            (
                StatusCode::from_u16(s).expect("status"),
                axum::Json(json!({"error": "server_error"})),
            )
                .into_response(),
        ),
        Some(Fault::InvalidJson) => Some((StatusCode::OK, "<html>not json</html>").into_response()),
    }
}

async fn discovery(State(shared): State<Arc<Mutex<Inner>>>) -> Response {
    if let Some(r) = fault(&shared, Endpoint::Discovery).await {
        return r;
    }
    let inner = shared.lock().expect("mock state");
    if let Some(doc) = &inner.discovery_override {
        return axum::Json(doc.clone()).into_response();
    }
    let i = &inner.issuer;
    axum::Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/auth"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/certs"),
        "end_session_endpoint": format!("{i}/logout"),
        "code_challenge_methods_supported": ["S256"],
    }))
    .into_response()
}

async fn jwks(State(shared): State<Arc<Mutex<Inner>>>) -> Response {
    if let Some(r) = fault(&shared, Endpoint::Jwks).await {
        return r;
    }
    let inner = shared.lock().expect("mock state");
    match &inner.jwks_override {
        Some(body) => (StatusCode::OK, body.clone()).into_response(),
        None => axum::Json(jwks_of(&inner)).into_response(),
    }
}

fn oauth_error(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

async fn token(
    State(shared): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(r) = fault(&shared, Endpoint::Token).await {
        return r;
    }
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let mut inner = shared.lock().expect("mock state");
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {}
        Some("refresh_token") => return refresh(&mut inner, &form),
        _ => return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    }
    // Single use: a second redemption is invalid_grant.
    let Some(pending) = form.get("code").and_then(|c| inner.codes.remove(c)) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if b64(&Sha256::digest(verifier.as_bytes())) != pending.challenge
        || form.get("redirect_uri") != Some(&pending.redirect_uri)
    {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let kid = inner.signing_kid.clone();
    let id_token = sign_with(&inner, &kid, json!({}), &pending.claims);
    let refresh_token = new_refresh_token(&mut inner, &pending.claims);
    axum::Json(json!({
        "access_token": "synthetic-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
        "refresh_token": refresh_token,
    }))
    .into_response()
}

/// A new live refresh token for `claims` (the first one of the process is
/// `synthetic-refresh-token`).
fn new_refresh_token(inner: &mut Inner, claims: &Value) -> String {
    let token = match inner.next_refresh {
        0 => "synthetic-refresh-token".to_owned(),
        n => format!("synthetic-refresh-token-{n}"),
    };
    inner.next_refresh += 1;
    let mut claims = claims.clone();
    if let Some(c) = claims.as_object_mut() {
        c.remove("nonce");
    }
    inner.refresh_tokens.insert(token.clone(), claims);
    token
}

/// `grant_type=refresh_token`: a live token is rotated; an unknown or
/// ended one is `invalid_grant`.
fn refresh(inner: &mut Inner, form: &HashMap<String, String>) -> Response {
    let Some(mut claims) = form
        .get("refresh_token")
        .and_then(|t| inner.refresh_tokens.remove(t))
    else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let now = now_secs().expect("clock after 1970");
    claims["iat"] = json!(now);
    claims["exp"] = json!(now + 300);
    let rotated = new_refresh_token(inner, &claims);
    let mut body = json!({
        "access_token": "synthetic-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "refresh_token": rotated,
    });
    if inner.refresh_mode == RefreshMode::WithIdToken {
        merge(&mut claims, inner.refresh_overrides.clone());
        let kid = inner.signing_kid.clone();
        body["id_token"] = json!(sign_with(inner, &kid, json!({}), &claims));
    }
    axum::Json(body).into_response()
}
