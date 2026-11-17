//! `[auth.oidc]`: the browser sign-in through an OIDC identity provider
//! (Keycloak; M2 MR4, ADR-0021 §12).
//!
//! - `auth.oidc.issuer` — the issuer URL exactly as the provider's discovery
//!   document states it (`https://…`, or `http://` on a loopback host for
//!   development). No query, fragment or user info.
//! - `auth.oidc.client_id`, `auth.oidc.client_secret_file` (a secret file,
//!   ADR-0013: absolute, owner-only, read at load) — the confidential client.
//! - `auth.oidc.ca_file` — the PEM bundle of the internal CA the provider's
//!   certificate chains to; required for an `https` issuer (no public roots,
//!   ADR-0021 §2), refused for an `http` one.
//! - `auth.oidc.employee_number_claim` (default `employee_number`),
//!   `auth.oidc.access_role` (default `access`), `auth.oidc.signing_algs`
//!   (default `RS256`; a comma list of `RS256` and `ES256` only),
//!   `auth.oidc.auto_link` (default `true`, ADR-0021 §13).
//! - `auth.oidc.claims_source` (default `id_token`; `access_token` for a
//!   provider that puts the identity claims and the client roles in the
//!   access token only, the PFS Keycloak — ADR-0021 amendment 2026-11-17).
//! - `auth.oidc.allow_insecure_issuer` (default `false`): an `http` issuer
//!   on a host other than the loopback, for a trial provider only.
//!
//! The section is all or nothing: without any `auth.oidc.*` value browser
//! sign-in is off (`FG-AUTH-0018` at `/-/auth/login`, doctor warns); with
//! some values the required ones must all be there. Every problem is
//! `FG-SYS-0002` with `config_key` and `reason` (`missing`, `invalid`,
//! `scheme`, `insecure_http` (an `http` issuer off the loopback without
//! `auth.oidc.allow_insecure_issuer = true`), `has_query`, `has_user_info`, `ca_required`,
//! `ca_not_allowed`, `alg_not_allowed`, `not_absolute`, `empty`, …).

use std::path::{Path, PathBuf};

use forge_errors::ForgeError;

use super::secret::{self, Secret};
use super::{database, invalid};

const ISSUER: &str = "auth.oidc.issuer";
const CLIENT_ID: &str = "auth.oidc.client_id";
const CLIENT_SECRET_FILE: &str = "auth.oidc.client_secret_file";
const CA_FILE: &str = "auth.oidc.ca_file";
const EMPLOYEE_NUMBER_CLAIM: &str = "auth.oidc.employee_number_claim";
const ACCESS_ROLE: &str = "auth.oidc.access_role";
const SIGNING_ALGS: &str = "auth.oidc.signing_algs";
const AUTO_LINK: &str = "auth.oidc.auto_link";
const CLAIMS_SOURCE: &str = "auth.oidc.claims_source";
const ALLOW_INSECURE_ISSUER: &str = "auth.oidc.allow_insecure_issuer";

/// Every `auth.oidc.*` key (for the all-or-nothing rule).
const ALL: &[&str] = &[
    ISSUER,
    CLIENT_ID,
    CLIENT_SECRET_FILE,
    CA_FILE,
    EMPLOYEE_NUMBER_CLAIM,
    ACCESS_ROLE,
    SIGNING_ALGS,
    AUTO_LINK,
    CLAIMS_SOURCE,
    ALLOW_INSECURE_ISSUER,
];

/// Where the person's identity claims and client roles are read
/// (`auth.oidc.claims_source`, ADR-0021 amendment 2026-11-17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OidcClaimsSource {
    /// The verified ID token (ADR-0021 §4, the default).
    #[default]
    IdToken,
    /// The verified access token (a Keycloak JWT whose `azp` is the
    /// client); the ID token is still verified for `sub`, `nonce` and
    /// `sid`, and both must name the same `sub`.
    AccessToken,
}

impl OidcClaimsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdToken => "id_token",
            Self::AccessToken => "access_token",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "id_token" => Some(Self::IdToken),
            "access_token" => Some(Self::AccessToken),
            _ => None,
        }
    }
}

/// A JWS algorithm the ID token may be signed with (ADR-0021 §9): only
/// these two; `none`, HS*, PS* and EdDSA are refused at load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OidcSigningAlg {
    Rs256,
    Es256,
}

impl OidcSigningAlg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }
}

/// `auth.oidc.*`, validated.
#[derive(Debug)]
pub struct OidcConfig {
    /// The issuer exactly as configured (compared byte for byte with `iss`).
    pub issuer: String,
    pub client_id: String,
    /// The client secret (`client_secret_basic`); never printed.
    pub client_secret: Secret,
    /// The CA bundle (PEM) for an `https` issuer.
    pub ca_file: Option<PathBuf>,
    pub employee_number_claim: String,
    pub access_role: String,
    pub signing_algs: Vec<OidcSigningAlg>,
    pub auto_link: bool,
    /// `auth.oidc.claims_source`.
    pub claims_source: OidcClaimsSource,
    /// `auth.oidc.allow_insecure_issuer`: an `http` issuer was accepted
    /// off the loopback (trials only; `forge doctor` should say so).
    pub allow_insecure_issuer: bool,
}

/// `[auth]`.
#[derive(Debug, Default)]
pub struct AuthConfig {
    /// `None`: browser sign-in is not configured (ADR-0021 §12).
    pub oidc: Option<OidcConfig>,
    /// `auth.act_as_hmac_keys_file` (M3 S2, ADR-0025 §5.2; a secret file,
    /// ADR-0013 name `act_as_hmac_keys`): `None` — acting for a person is
    /// off (`FG-AUTH-0003 reason=act_as_not_configured`; doctor
    /// `act_as.keys`). Read by [`AuthConfig::act_as_keys`].
    pub(super) act_as_keys_file: Option<PathBuf>,
}

/// Upper bound for the act-as key file (as `api.cursor_keys_file`).
const MAX_ACT_AS_KEYS_BYTES: u64 = 64 * 1024;

impl AuthConfig {
    /// Whether `auth.act_as_hmac_keys_file` is configured.
    pub fn has_act_as_keys(&self) -> bool {
        self.act_as_keys_file.is_some()
    }

    /// Read and parse `auth.act_as_hmac_keys_file` now (`None` when it is
    /// not configured): unreadable, malformed, or not exactly one `active`
    /// key is `FG-SYS-0002 config_key=auth.act_as_hmac_keys_file`.
    pub fn act_as_keys(&self) -> Result<Option<crate::act_as::ActAsKeys>, ForgeError> {
        let Some(path) = self.act_as_keys_file.as_deref() else {
            return Ok(None);
        };
        let key = crate::act_as::KEYS_CONFIG_KEY;
        // The key file's text is wiped when it goes (ADR-0025 review, LOW c).
        let text =
            zeroize::Zeroizing::new(secret::read_secret_text(key, path, MAX_ACT_AS_KEYS_BYTES)?);
        crate::act_as::ActAsKeys::parse(&text)
            .map(Some)
            .map_err(|e| e.with_context("path", path.display()))
    }

    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let act_as_keys_file = match get(crate::act_as::KEYS_CONFIG_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(crate::act_as::KEYS_CONFIG_KEY, "empty")),
            Some(p) => Some(database::password_path(crate::act_as::KEYS_CONFIG_KEY, p)?),
        };
        let mut config = Self::load_oidc(get)?;
        config.act_as_keys_file = act_as_keys_file;
        Ok(config)
    }

    fn load_oidc<'a>(get: impl Fn(&str) -> Option<&'a str> + Copy) -> Result<Self, ForgeError> {
        let text = |key: &'static str| match get(key).map(str::trim) {
            Some("") => Err(invalid(key, "empty")),
            other => Ok(other),
        };
        let mut any = false;
        for key in ALL {
            any |= text(key)?.is_some();
        }
        if !any {
            return Ok(Self {
                oidc: None,
                act_as_keys_file: None,
            });
        }
        let required = |key: &'static str| text(key)?.ok_or_else(|| invalid(key, "missing"));
        let flag = |key: &'static str| match text(key)? {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(invalid(key, "invalid")),
        };
        let allow_insecure_issuer = flag(ALLOW_INSECURE_ISSUER)?;
        let issuer = required(ISSUER)?;
        let https = check_issuer(issuer, allow_insecure_issuer)?;
        let client_id = required(CLIENT_ID)?;
        if client_id.chars().count() > 255 || client_id.chars().any(char::is_control) {
            return Err(invalid(CLIENT_ID, "invalid"));
        }
        let secret_path =
            database::password_path(CLIENT_SECRET_FILE, required(CLIENT_SECRET_FILE)?)?;
        let client_secret = secret::read_password(CLIENT_SECRET_FILE, Path::new(&secret_path))?;
        let ca_file = text(CA_FILE)?
            .map(|p| database::password_path(CA_FILE, p))
            .transpose()?;
        match (https, ca_file.is_some()) {
            (true, false) => return Err(invalid(CA_FILE, "ca_required")),
            (false, true) => return Err(invalid(CA_FILE, "ca_not_allowed")),
            (true, true) | (false, false) => {}
        }
        let claim = |key: &'static str, default: &str| -> Result<String, ForgeError> {
            let value = text(key)?.unwrap_or(default);
            let ok = value.len() <= 128
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
            if ok {
                Ok(value.to_owned())
            } else {
                Err(invalid(key, "invalid"))
            }
        };
        let signing_algs = match text(SIGNING_ALGS)? {
            None => vec![OidcSigningAlg::Rs256],
            Some(list) => {
                let mut algs = Vec::new();
                for name in list.split(',').map(str::trim) {
                    let alg = OidcSigningAlg::parse(name)
                        .ok_or_else(|| invalid(SIGNING_ALGS, "alg_not_allowed"))?;
                    if !algs.contains(&alg) {
                        algs.push(alg);
                    }
                }
                algs
            }
        };
        let auto_link = match text(AUTO_LINK)? {
            None | Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(invalid(AUTO_LINK, "invalid")),
        };
        let claims_source = match text(CLAIMS_SOURCE)? {
            None => OidcClaimsSource::IdToken,
            Some(v) => {
                OidcClaimsSource::parse(v).ok_or_else(|| invalid(CLAIMS_SOURCE, "invalid"))?
            }
        };
        Ok(Self {
            act_as_keys_file: None,
            oidc: Some(OidcConfig {
                issuer: issuer.to_owned(),
                client_id: client_id.to_owned(),
                client_secret,
                ca_file,
                employee_number_claim: claim(EMPLOYEE_NUMBER_CLAIM, "employee_number")?,
                access_role: claim(ACCESS_ROLE, "access")?,
                signing_algs,
                auto_link,
                claims_source,
                allow_insecure_issuer,
            }),
        })
    }
}

/// The issuer's rules; `true` for `https`. `http` is accepted on the
/// loopback, elsewhere only with `allow_insecure` (a trial provider).
fn check_issuer(raw: &str, allow_insecure: bool) -> Result<bool, ForgeError> {
    let bad = |reason| invalid(ISSUER, reason);
    let url = url::Url::parse(raw).map_err(|_| bad("invalid"))?;
    let host = url.host_str().ok_or_else(|| bad("invalid"))?;
    let https = match url.scheme() {
        "https" => true,
        "http" if allow_insecure || matches!(host, "localhost" | "127.0.0.1" | "[::1]") => false,
        "http" => return Err(bad("insecure_http")),
        _ => return Err(bad("scheme")),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("has_user_info"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad("has_query"));
    }
    Ok(https)
}
