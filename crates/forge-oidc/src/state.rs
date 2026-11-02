//! The login state (ADR-0021 §3): `state`, `nonce` and the PKCE verifier
//! (256 bits each from the OS CSPRNG), `next` and the creation time, sealed
//! with AES-256-GCM under the `oidc-state` key (`web.session_key_file`,
//! HKDF per purpose) into the `__Host-gstn_oidc` cookie. Nothing is kept on
//! the server: the browser carries it, and every callback clears it.
//!
//! Also the rule for `next` (a relative path only, parsed by ADR-0015's
//! rules, otherwise `/`).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_core::web_keys::PurposeKeys;
use forge_errors::ForgeError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{random32, state};

/// The associated data of the sealed cookie (binds it to its name).
const AAD: &[u8] = b"gitstation/web/oidc-state-cookie/v1";
/// A cookie created this far in the future is refused too (clock skew).
const FUTURE_SKEW_SECS: u64 = 60;

/// One started sign-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginState {
    /// base64url of 32 random bytes.
    pub state: String,
    pub nonce: String,
    /// The PKCE `code_verifier` (43 characters).
    pub verifier: String,
    /// Where to go after the sign-in (already checked).
    pub next: String,
    /// Seconds since the epoch.
    pub created: u64,
}

impl LoginState {
    /// A new state for `next` at `now`.
    pub fn generate(next: String, now: u64) -> Result<Self, ForgeError> {
        Ok(Self {
            state: URL_SAFE_NO_PAD.encode(random32()?),
            nonce: URL_SAFE_NO_PAD.encode(random32()?),
            verifier: URL_SAFE_NO_PAD.encode(random32()?),
            next,
            created: now,
        })
    }

    /// The PKCE `code_challenge` (S256) of the verifier.
    pub fn challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()))
    }

    /// The cookie value.
    pub fn seal(&self, keys: &PurposeKeys) -> Result<String, ForgeError> {
        let plain = serde_json::to_vec(self)
            .map_err(|e| crate::error::internal("state_encode").with_source(e))?;
        Ok(URL_SAFE_NO_PAD.encode(keys.seal(AAD, &plain)?))
    }

    /// Open a cookie value at `now` (`FG-AUTH-0011`: `state_cookie_invalid`
    /// with `detail`, `state_expired`).
    pub fn open(
        cookie: &str,
        keys: &PurposeKeys,
        now: u64,
        max_age: u64,
    ) -> Result<Self, ForgeError> {
        let invalid =
            |detail: &'static str| state("state_cookie_invalid").with_context("detail", detail);
        let sealed = URL_SAFE_NO_PAD
            .decode(cookie)
            .map_err(|_| invalid("encoding"))?;
        let plain = keys.open(AAD, &sealed).map_err(|e| invalid(e.as_str()))?;
        let opened: Self = serde_json::from_slice(&plain).map_err(|_| invalid("json"))?;
        if opened.created.saturating_add(max_age) < now
            || opened.created > now.saturating_add(FUTURE_SKEW_SECS)
        {
            return Err(state("state_expired"));
        }
        Ok(opened)
    }

    /// The query's `state` equals this one (constant time).
    pub fn matches(&self, presented: &str) -> bool {
        presented.len() == self.state.len()
            && bool::from(subtle::ConstantTimeEq::ct_eq(
                presented.as_bytes(),
                self.state.as_bytes(),
            ))
    }
}

/// `next` as the sign-in will use it, and whether the given value was
/// refused (`reason=next_rejected` in the log). Accepted: `/`, or a path
/// starting with one `/` (never `//` or `/\`) without control characters
/// or backslashes that ADR-0015's parser reads, with an optional query.
pub fn safe_next(raw: Option<&str>) -> (String, bool) {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return ("/".to_owned(), false);
    };
    let ok = raw.len() <= 2048
        && raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.contains('\\')
        && !raw.contains('#')
        && !raw.chars().any(char::is_control)
        && {
            let (path, query) = match raw.split_once('?') {
                Some((p, q)) => (p, Some(q)),
                None => (raw, None),
            };
            path == "/" || forge_core::web_path::parse_web_path(path, query).is_ok()
        };
    if ok {
        (raw.to_owned(), false)
    } else {
        ("/".to_owned(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_is_a_relative_path_or_home() {
        for good in ["/", "/acme/forge", "/acme/forge/-/tree/main", "/acme?tab=1"] {
            assert_eq!(safe_next(Some(good)), (good.to_owned(), false), "{good}");
        }
        assert_eq!(safe_next(None), ("/".to_owned(), false));
        for bad in [
            "https://evil.example.invalid/",
            "//evil.example.invalid/x",
            "/\\evil.example.invalid",
            "/acme\r\nSet-Cookie: x=y",
            "/acme\u{7}",
            "javascript:alert(1)",
            "acme/forge",
            "/acme/#x",
            "/acme//forge",
        ] {
            assert_eq!(safe_next(Some(bad)), ("/".to_owned(), true), "{bad:?}");
        }
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        // RFC 7636 appendix B.
        let s = LoginState {
            state: String::new(),
            nonce: String::new(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned(),
            next: "/".to_owned(),
            created: 0,
        };
        assert_eq!(s.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
