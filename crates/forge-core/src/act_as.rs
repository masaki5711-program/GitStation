//! Acting for a person (ADR-0025): the `Forge-Act-As` header grammar, the
//! assertion token `fga_<public_id>_<secret>`, the bytes its HMAC seals
//! ([`binding_bytes`], the only function that builds them) and the key
//! ring of `auth.act_as_hmac_keys_file` ([`ActAsKeys`]). Pure functions
//! only: no database, no clock.
//!
//! The assertion's secret never reaches a log line: [`AssertionToken`] has
//! no `Display`, no `Serialize`, and its `Debug` is redacted:
//!
//! ```compile_fail
//! use forge_core::act_as::AssertionToken;
//! fn show(t: &AssertionToken) -> String { format!("{t}") }
//! ```

use std::fmt;

use base64::Engine;
use forge_errors::{ErrorCode, ForgeError};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::token::{self, PublicId, TokenSecret};
use crate::UserId;

/// The header naming the person (lowercase, as `http` compares names).
pub const ACT_AS_HEADER: &str = "forge-act-as";
/// The header carrying the assertion.
pub const ASSERTION_HEADER: &str = "forge-assertion";
/// Literal prefix of every assertion (found by the secret scanners).
pub const ASSERTION_PREFIX: &str = "fga";
/// How long an assertion lives after its issue (ADR-0025 §5.1).
pub const ASSERTION_LIFETIME_SECS: i64 = 300;
/// The configuration key of the key file (ADR-0013 name `act_as_hmac_keys`).
pub const KEYS_CONFIG_KEY: &str = "auth.act_as_hmac_keys_file";
/// The longest `employee:` value (`forge.users.employee_number`).
const MAX_EMPLOYEE_CHARS: usize = 64;
/// The longest `user:` value (an `i64` in decimal).
const MAX_USER_ID_DIGITS: usize = 19;
/// The purpose strings: one key never yields two kinds of value.
const ASSERTION_PURPOSE: &[u8] = b"forge.act_as.assertion.v1";
const CLAIMED_PURPOSE: &[u8] = b"forge.act_as.claimed.v1";

type HmacSha256 = Hmac<Sha256>;

/// The person a `Forge-Act-As` value names (ADR-0025 §1).
#[derive(Clone, PartialEq, Eq)]
pub enum ActAsTarget {
    /// `user:<id>`: forge's user id (the claim's `sub.user_id`).
    User(UserId),
    /// `employee:<number>`: exactly `forge.users.employee_number`
    /// (case-sensitive). Personal data: `Debug` hides it.
    Employee(String),
}

impl fmt::Debug for ActAsTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(id) => write!(f, "ActAsTarget::User({})", id.get()),
            Self::Employee(_) => f.write_str("ActAsTarget::Employee([redacted])"),
        }
    }
}

/// Why a `Forge-Act-As` value named nobody to act for (the log `reason`
/// of `FG-INT-0006`; the answer is the same for every reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRefusal {
    MissingValue,
    Malformed,
    MultipleHeaders,
    EmailForm,
    Unknown,
    Ambiguous,
    NotHuman,
    Disabled,
}

impl TargetRefusal {
    pub const ALL: &'static [TargetRefusal] = &[
        Self::MissingValue,
        Self::Malformed,
        Self::MultipleHeaders,
        Self::EmailForm,
        Self::Unknown,
        Self::Ambiguous,
        Self::NotHuman,
        Self::Disabled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingValue => "missing_value",
            Self::Malformed => "malformed",
            Self::MultipleHeaders => "multiple_headers",
            Self::EmailForm => "email_form",
            Self::Unknown => "unknown",
            Self::Ambiguous => "ambiguous",
            Self::NotHuman => "not_human",
            Self::Disabled => "disabled",
        }
    }

    /// `FG-INT-0006` with this `reason`.
    pub fn error(self) -> ForgeError {
        ForgeError::new(ErrorCode::ActAsTargetInvalid).with_context("reason", self.as_str())
    }
}

impl ActAsTarget {
    /// Parse one header value (the caller refuses two headers with
    /// [`TargetRefusal::MultipleHeaders`]). Never panics; quotes nothing.
    pub fn parse(raw: &str) -> Result<Self, TargetRefusal> {
        if raw.is_empty() {
            return Err(TargetRefusal::MissingValue);
        }
        if raw.trim() != raw || raw.contains(',') {
            return Err(TargetRefusal::Malformed);
        }
        if let Some(rest) = raw.strip_prefix("user:") {
            if rest.contains('@') {
                return Err(TargetRefusal::EmailForm);
            }
            if rest.is_empty()
                || rest.len() > MAX_USER_ID_DIGITS
                || !rest.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(TargetRefusal::Malformed);
            }
            let id = crate::parse_positive_id(rest).ok_or(TargetRefusal::Malformed)?;
            return Ok(Self::User(UserId::new(id)));
        }
        if let Some(rest) = raw.strip_prefix("employee:") {
            let ok = !rest.is_empty()
                && rest.chars().count() <= MAX_EMPLOYEE_CHARS
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            return if ok {
                Ok(Self::Employee(rest.to_owned()))
            } else {
                Err(TargetRefusal::Malformed)
            };
        }
        Err(TargetRefusal::Malformed)
    }

    /// The form, for the log (`user` / `employee`; never the value).
    pub fn form(&self) -> &'static str {
        match self {
            Self::User(_) => "user",
            Self::Employee(_) => "employee",
        }
    }
}

/// `FG-AUTH-0020` with `reason` (ADR-0025 §5.3).
pub fn assertion_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ActAsAssertionInvalid).with_context("reason", reason)
}

/// What an assertion lets the person's integration do (ADR-0025 §4: one
/// intent per action; X3 adds the binding intents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    Approve,
    Unapprove,
}

impl Intent {
    pub const ALL: &'static [Intent] = &[Self::Approve, Self::Unapprove];

    /// The stored and requested name (the CHECK of `act_as_assertions.intent`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Unapprove => "unapprove",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.as_str() == name)
    }
}

/// An assertion as created once at the claim, or as presented in
/// `Forge-Assertion`: `fga_<16 hex>_<64 hex>`. No `Display`, no
/// `Serialize`, redacted `Debug`, no `==`.
pub struct AssertionToken {
    public_id: PublicId,
    secret: TokenSecret,
}

impl AssertionToken {
    /// A fresh assertion from the OS RNG (`FG-SYS-0001` when it fails).
    pub fn generate() -> Result<Self, ForgeError> {
        let (public_id, secret) = token::random_parts().map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "rng_unavailable")
        })?;
        Ok(Self { public_id, secret })
    }

    /// Parse a presented value (`malformed` otherwise). Never panics.
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        let (public_id, secret) =
            token::parse_prefixed(raw, ASSERTION_PREFIX).map_err(|_| "malformed")?;
        Ok(Self { public_id, secret })
    }

    /// The 16 hex characters: identifies the row, safe to log.
    pub fn public_id(&self) -> &str {
        self.public_id.as_str()
    }

    pub fn secret(&self) -> &[u8; token::SECRET_LEN] {
        self.secret.as_bytes()
    }

    /// The `fga_…` text: call it only to answer the claim, once.
    pub fn reveal(&self) -> String {
        format!(
            "{ASSERTION_PREFIX}_{}_{}",
            self.public_id.as_str(),
            token::to_hex(self.secret.as_bytes())
        )
    }
}

impl fmt::Debug for AssertionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AssertionToken({ASSERTION_PREFIX}_{}_[redacted])",
            self.public_id.as_str()
        )
    }
}

/// What an assertion is bound to (ADR-0025 §5.2): every field enters the
/// MAC, so a changed row never verifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionBinding<'a> {
    pub public_id: &'a str,
    pub integration_id: i64,
    pub user_id: i64,
    pub intent: Intent,
    pub project_id: i64,
    pub merge_request_id: i64,
    pub head_sha: &'a str,
    pub target_ref: &'a str,
    pub target_sha: &'a str,
    /// `expires_at` in microseconds since the Unix epoch, as stored.
    pub expires_at_micros: i64,
}

/// The length-prefixed concatenation of the binding (the only function
/// that builds what the assertion's MAC covers).
pub fn binding_bytes(b: &AssertionBinding<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    let mut field = |bytes: &[u8]| {
        // Each field fits easily (refs at most 1024 bytes).
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(bytes);
    };
    field(b.public_id.as_bytes());
    field(&b.integration_id.to_be_bytes());
    field(&b.user_id.to_be_bytes());
    field(b.intent.as_str().as_bytes());
    field(&b.project_id.to_be_bytes());
    field(&b.merge_request_id.to_be_bytes());
    field(b.head_sha.as_bytes());
    field(b.target_ref.as_bytes());
    field(b.target_sha.as_bytes());
    field(&b.expires_at_micros.to_be_bytes());
    out
}

/// The state of one key of the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Seals new assertions and the audit's `act_as_claimed` (exactly one).
    Active,
    /// Still verifies (the rotation's 10 minutes after a new active key).
    Valid,
    /// Kept for the audit's comparison; its assertions are `key_retired`.
    Retired,
}

struct Key {
    kid: i16,
    bytes: [u8; 32],
    state: KeyState,
}

/// The keys of `auth.act_as_hmac_keys_file` (ADR-0025 §5.2). One line per
/// key: `<kid>:<base64 of 32 bytes>` followed by `active` or `retired`
/// (or nothing: a key that still verifies); blank lines and `#` comments
/// are ignored. Exactly one key is `active`. The bytes never print.
pub struct ActAsKeys {
    keys: Vec<Key>,
}

impl fmt::Debug for ActAsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActAsKeys")
            .field("kids", &self.keys.iter().map(|k| k.kid).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// How many keys of each state the ring holds (`forge doctor act_as.keys`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCounts {
    pub active: usize,
    pub valid: usize,
    pub retired: usize,
}

/// Why a presented assertion did not verify against the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacRefusal {
    /// Sealed with a retired key (or one the ring no longer has).
    KeyRetired,
    MacMismatch,
}

impl MacRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeyRetired => "key_retired",
            Self::MacMismatch => "mac_mismatch",
        }
    }
}

fn keys_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", KEYS_CONFIG_KEY)
        .with_context("reason", reason)
}

impl ActAsKeys {
    /// Parse the file's text: an unreadable line, a duplicate kid, a key
    /// that is not 32 bytes, or not exactly one `active` key is
    /// `FG-SYS-0002 config_key=auth.act_as_hmac_keys_file` with `reason`
    /// (and `line`); the key bytes are never quoted.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let mut keys: Vec<Key> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = |reason| keys_invalid(reason).with_context("line", n + 1);
            let mut words = line.split_whitespace();
            let key = words.next().ok_or_else(|| at("malformed_line"))?;
            let state = match words.next() {
                None => KeyState::Valid,
                Some("active") => KeyState::Active,
                Some("retired") => KeyState::Retired,
                Some(_) => return Err(at("unknown_marker")),
            };
            if words.next().is_some() {
                return Err(at("malformed_line"));
            }
            let (kid, b64) = key.split_once(':').ok_or_else(|| at("malformed_line"))?;
            let kid: i16 = kid
                .parse()
                .ok()
                .filter(|k| *k >= 1)
                .ok_or_else(|| at("kid_invalid"))?;
            if keys.iter().any(|k| k.kid == kid) {
                return Err(at("kid_duplicate"));
            }
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|_| at("key_not_base64"))?;
            let bytes: [u8; 32] = raw.try_into().map_err(|_| at("key_length"))?;
            keys.push(Key { kid, bytes, state });
        }
        match keys.iter().filter(|k| k.state == KeyState::Active).count() {
            1 => Ok(Self { keys }),
            0 => Err(keys_invalid("no_active_key")),
            _ => Err(keys_invalid("several_active_keys")),
        }
    }

    /// The counts by state.
    pub fn counts(&self) -> KeyCounts {
        let count = |s| self.keys.iter().filter(|k| k.state == s).count();
        KeyCounts {
            active: count(KeyState::Active),
            valid: count(KeyState::Valid),
            retired: count(KeyState::Retired),
        }
    }

    fn active(&self) -> Result<&Key, ForgeError> {
        self.keys
            .iter()
            .find(|k| k.state == KeyState::Active)
            .ok_or_else(|| keys_invalid("no_active_key"))
    }

    /// The active key's id.
    pub fn active_kid(&self) -> Result<i16, ForgeError> {
        self.active().map(|k| k.kid)
    }

    fn mac(key: &[u8; 32], purpose: &[u8], parts: &[&[u8]]) -> Result<[u8; 32], ForgeError> {
        let mut mac = HmacSha256::new_from_slice(key).map_err(|_| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "act_as_mac_key")
        })?;
        mac.update(purpose);
        for p in parts {
            mac.update(p);
        }
        Ok(mac.finalize().into_bytes().into())
    }

    /// Seal an assertion with the active key: `(kid, mac)`.
    pub fn seal(
        &self,
        secret: &[u8; token::SECRET_LEN],
        binding: &AssertionBinding<'_>,
    ) -> Result<(i16, [u8; 32]), ForgeError> {
        let key = self.active()?;
        let mac = Self::mac(
            &key.bytes,
            ASSERTION_PURPOSE,
            &[secret, &binding_bytes(binding)],
        )?;
        Ok((key.kid, mac))
    }

    /// Verify a stored MAC (constant time). A key this ring does not hold
    /// any more is `key_retired`, like a retired one.
    pub fn verify(
        &self,
        kid: i16,
        secret: &[u8; token::SECRET_LEN],
        binding: &AssertionBinding<'_>,
        stored: &[u8],
    ) -> Result<(), MacRefusal> {
        let Some(key) = self.keys.iter().find(|k| k.kid == kid) else {
            return Err(MacRefusal::KeyRetired);
        };
        if key.state == KeyState::Retired {
            return Err(MacRefusal::KeyRetired);
        }
        let mac = Self::mac(
            &key.bytes,
            ASSERTION_PURPOSE,
            &[secret, &binding_bytes(binding)],
        )
        .map_err(|_| MacRefusal::MacMismatch)?;
        if bool::from(mac.as_slice().ct_eq(stored)) {
            Ok(())
        } else {
            Err(MacRefusal::MacMismatch)
        }
    }

    /// A MAC computed for nothing (an unknown public id), so a lookup that
    /// finds no row costs what one that finds it does.
    pub fn dummy_verify(&self, secret: &[u8; token::SECRET_LEN]) {
        if let Ok(key) = self.active() {
            std::hint::black_box(Self::mac(&key.bytes, ASSERTION_PURPOSE, &[secret]).is_ok());
        }
    }

    /// The audit's `act_as_claimed`: `hex(HMAC(K[kid], purpose ‖ value))`
    /// with the active key (or `kid`, for the operator's comparison).
    pub fn claimed(&self, value: &str, kid: Option<i16>) -> Result<(i16, String), ForgeError> {
        let key = match kid {
            None => self.active()?,
            Some(kid) => self
                .keys
                .iter()
                .find(|k| k.kid == kid)
                .ok_or_else(|| keys_invalid("kid_unknown").with_context("kid", kid))?,
        };
        let mac = Self::mac(&key.bytes, CLAIMED_PURPOSE, &[value.as_bytes()])?;
        Ok((key.kid, token::to_hex(&mac)))
    }
}

#[cfg(test)]
#[path = "act_as_tests.rs"]
mod tests;
