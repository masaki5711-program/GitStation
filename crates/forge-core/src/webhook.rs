//! Webhooks (M3 I2; ADR-0016 §8 and its Amendment I2; plan 02 §5.5, §11.1):
//! what a webhook may be pointed at, how a delivery is signed, how its
//! secret is kept, and which events it may carry.
//!
//! - **The URL** ([`WebhookUrl`]): an absolute `https` URL (`http` only
//!   with `webhook.allow_http`, development) of at most [`MAX_URL_BYTES`],
//!   without user information or a fragment. Logs and audit rows show its
//!   [`WebhookUrl::origin`] (scheme, host and port) only — the path or
//!   query may hold a receiver's token.
//! - **The target policy** ([`TargetPolicy`], `webhook.allowed_targets`):
//!   the operator's allowlist of `host:port` and `cidr:port` entries, empty
//!   by default (nothing can be delivered until the operator lists the
//!   internal receivers). Whatever the list says, loopback, link-local
//!   (169.254.169.254 metadata included), unspecified, multicast,
//!   broadcast, reserved, CGNAT (100.64/10), benchmarking (198.18/15),
//!   IPv4-compatible, NAT64 (both prefixes), 6to4, Teredo (2001::/32) and
//!   site-local (fec0::/10) addresses are refused; an IPv4-mapped IPv6 address is judged by its IPv4.
//!   [`TargetPolicy::check_url`] runs when a URL is stored;
//!   [`TargetPolicy::check_resolved`] runs at every delivery on the
//!   addresses the name resolves to then, and returns the one address the
//!   connection is pinned to (DNS rebinding: the checked address is the one
//!   connected to; any refused address among them refuses the name). Every
//!   refusal is `FG-INT-0010` with `reason` (log only; the user sees one
//!   sentence).
//! - **The signature** ([`signature`], [`signature_header`]):
//!   `Forge-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, t "."
//!   delivery_id "." body)>`, with a second `v1` for the previous secret
//!   during the [`SECRET_OVERLAP`] after a rotation; `Forge-Signature-Kid`
//!   names the secret versions in the same order.
//! - **The secret** ([`WebhookSecret`]): 32 random bytes, `whsec_` +
//!   base64url, shown once. Stored sealed with AES-256-GCM under a key of
//!   `webhook.secret_keys_file` (ADR-0013: outside the database), the
//!   associated data binding the ciphertext to its webhook's `uid`
//!   ([`WebhookSecretKeys`]).
//! - **The events** ([`webhook_event_types`], [`WebhookDeliverySet`]): `*`
//!   and `<family>.*` admit the integration class; a maintainer- or
//!   deployment-class type only by its exact name (ADR-0016 §5); the admin
//!   class never. Which classes the webhook's owner may receive is
//!   forge-auth's decision (`prove_webhook_delivery`), the only caller of
//!   [`WebhookDeliverySet::from_authorized_decisions`].

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_errors::{ErrorCode, ForgeError};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::event_type::{EventType, VisibilityClass};
use crate::ids::{ProjectId, UserId, WebhookId};
use crate::namespace_resolver::ProjectAccess;
use crate::user_kind::UserKind;

/// The most webhooks of one project (`FG-INT-0012 reason=webhook_limit`).
pub const MAX_WEBHOOKS_PER_PROJECT: i64 = 20;
/// The most entries of a webhook's `event_types` (the table CHECK).
pub const MAX_EVENT_TYPE_ENTRIES: usize = 64;
/// The longest URL (the table CHECK).
pub const MAX_URL_BYTES: usize = 2048;
/// Failed attempts in a row that disable a webhook (`FG-INT-0011`).
pub const DISABLE_AFTER_FAILURES: i32 = 50;
/// Failing without a success this long disables a webhook too.
pub const DISABLE_AFTER_FAILING: Duration = Duration::from_secs(24 * 3600);
/// How long the previous secret still signs after a rotation.
pub const SECRET_OVERLAP: Duration = Duration::from_secs(24 * 3600);
/// The attempts of one delivery before it is `failed` (`FG-INT-0004`).
pub const MAX_ATTEMPTS: i32 = 10;
/// The connection's own limit (the name's resolution included).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// One attempt in all: connect, send and read the answer.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of an answer's body read (the rest is never read).
pub const MAX_RESPONSE_BYTES: usize = 4096;
/// The most characters of an answer's body a log line carries.
pub const RESPONSE_HEAD_CHARS: usize = 128;
/// The delivery's signature header.
pub const SIGNATURE_HEADER: &str = "Forge-Signature";
/// The secret versions of the signature's `v1` values, in order.
pub const SIGNATURE_KID_HEADER: &str = "Forge-Signature-Kid";
/// The event type header.
pub const EVENT_HEADER: &str = "Forge-Event";
/// The delivery id header (the receiver's deduplication key).
pub const DELIVERY_HEADER: &str = "Forge-Delivery";
/// The webhook id header.
pub const WEBHOOK_HEADER: &str = "Forge-Webhook-Id";
/// The prefix of a webhook secret.
pub const SECRET_PREFIX: &str = "whsec_";
/// The configuration key of the allowlist (errors name it).
pub const ALLOWED_TARGETS_KEY: &str = "webhook.allowed_targets";
/// The configuration key of the secret keys (errors name it).
pub const SECRET_KEYS_KEY: &str = "webhook.secret_keys_file";
/// The event type allowlist entry that admits every integration-class type.
pub const ALL_EVENT_TYPES: &str = "*";

/// The wait after failed attempt `attempt` (1-based) before the next:
/// 10 s × 4^(attempt − 1), at most 4 h (about 16 h over [`MAX_ATTEMPTS`]).
pub fn retry_delay(attempt: i32) -> Duration {
    let exp = u32::try_from(attempt.saturating_sub(1).clamp(0, 10)).unwrap_or(0);
    Duration::from_secs(10u64.saturating_mul(4u64.saturating_pow(exp)))
        .min(Duration::from_secs(4 * 3600))
}

fn forbidden(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::WebhookTargetForbidden).with_context("reason", reason)
}

/// The host of a webhook URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetHost {
    /// A DNS name, lowercase.
    Name(String),
    /// An address literal.
    Ip(IpAddr),
}

/// A webhook URL that passed the shape rules (not yet the policy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookUrl {
    url: url::Url,
    host: TargetHost,
    port: u16,
}

impl WebhookUrl {
    /// Parse `raw`. Every refusal is `FG-INT-0010` with `reason`
    /// (`invalid_url`, `scheme`, `userinfo`, `fragment`) and never the URL.
    pub fn parse(raw: &str, allow_http: bool) -> Result<Self, ForgeError> {
        if raw.is_empty()
            || raw.len() > MAX_URL_BYTES
            || raw
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return Err(forbidden("invalid_url"));
        }
        let lower = raw.get(..8).unwrap_or(raw).to_ascii_lowercase();
        let after = if lower.starts_with("https://") {
            raw.get(8..)
        } else if lower.starts_with("http://") {
            raw.get(7..)
        } else {
            // Another scheme, or none at all.
            return Err(if raw.contains("://") {
                forbidden("scheme")
            } else {
                forbidden("invalid_url")
            });
        };
        // `https:///x` would be read as the host `x`.
        if after.is_none_or(|rest| rest.is_empty() || rest.starts_with('/')) {
            return Err(forbidden("invalid_url"));
        }
        let url = url::Url::parse(raw).map_err(|_| forbidden("invalid_url"))?;
        match url.scheme() {
            "https" => {}
            "http" if allow_http => {}
            _ => return Err(forbidden("scheme")),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(forbidden("userinfo"));
        }
        if url.fragment().is_some() {
            return Err(forbidden("fragment"));
        }
        let host = match url.host() {
            Some(url::Host::Domain(name)) => TargetHost::Name(name.to_ascii_lowercase()),
            Some(url::Host::Ipv4(a)) => TargetHost::Ip(IpAddr::V4(a)),
            Some(url::Host::Ipv6(a)) => TargetHost::Ip(IpAddr::V6(a)),
            None => return Err(forbidden("invalid_url")),
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| forbidden("invalid_url"))?;
        Ok(Self { url, host, port })
    }

    /// The URL as stored (normalized).
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn host(&self) -> &TargetHost {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn is_https(&self) -> bool {
        self.url.scheme() == "https"
    }

    /// `scheme://host:port` — what logs and audit rows show.
    pub fn origin(&self) -> String {
        let host = match &self.host {
            TargetHost::Name(n) => n.clone(), // clone: the origin owns its text
            TargetHost::Ip(IpAddr::V4(a)) => a.to_string(),
            TargetHost::Ip(IpAddr::V6(a)) => format!("[{a}]"),
        };
        format!("{}://{}:{}", self.url.scheme(), host, self.port)
    }
}

/// An IPv4-mapped IPv6 address as its IPv4, every other address as is.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(a) => a.to_ipv4_mapped().map_or(IpAddr::V6(a), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn denied_v4(a: Ipv4Addr) -> Option<&'static str> {
    let o = a.octets();
    if o[0] == 0 {
        Some("unspecified")
    } else if o[0] == 127 {
        Some("loopback")
    } else if o[0] == 169 && o[1] == 254 {
        Some("link_local")
    } else if a.is_broadcast() {
        Some("broadcast")
    } else if (224..=239).contains(&o[0]) {
        Some("multicast")
    } else if o[0] >= 240 {
        Some("reserved")
    } else if o[0] == 100 && o[1] & 0xc0 == 64 {
        // 100.64.0.0/10 (I2 review: refused whatever the allowlist says).
        Some("cgnat")
    } else if o[0] == 198 && o[1] & 0xfe == 18 {
        // 198.18.0.0/15.
        Some("benchmarking")
    } else {
        None
    }
}

fn denied_v6(a: Ipv6Addr) -> Option<&'static str> {
    let s = a.segments();
    if a.is_unspecified() {
        Some("unspecified")
    } else if a.is_loopback() {
        Some("loopback")
    } else if s[0] & 0xffc0 == 0xfe80 {
        Some("link_local")
    } else if s[0] & 0xff00 == 0xff00 {
        Some("multicast")
    } else if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        Some("nat64")
    } else if s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1 {
        // 64:ff9b:1::/48, local-use NAT64.
        Some("nat64")
    } else if s[0] == 0x2001 && s[1] == 0 {
        // 2001::/32, Teredo.
        Some("teredo")
    } else if s[0] & 0xffc0 == 0xfec0 {
        // fec0::/10, deprecated site-local.
        Some("site_local")
    } else if s[0] == 0x2002 {
        Some("six_to_four")
    } else if s[..6].iter().all(|x| *x == 0) {
        Some("ipv4_compatible")
    } else {
        None
    }
}

/// The always-refused range `ip` falls in, if any (after [`canonical`]).
pub fn denied_range(ip: IpAddr) -> Option<&'static str> {
    match canonical(ip) {
        IpAddr::V4(a) => denied_v4(a),
        IpAddr::V6(a) => denied_v6(a),
    }
}

/// An address range of the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn parse(raw: &str, v6: bool) -> Option<Self> {
        let (addr, prefix) = match raw.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (raw, None),
        };
        let net: IpAddr = if v6 {
            IpAddr::V6(addr.parse().ok()?)
        } else {
            IpAddr::V4(addr.parse().ok()?)
        };
        let max = if v6 { 128 } else { 32 };
        let prefix = match prefix {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse().ok()?,
            Some(_) => return None,
            None => max,
        };
        (prefix <= max).then_some(Self { net, prefix })
    }

    /// Whether `ip` (canonical) is inside.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(n) & mask == u128::from(a) & mask
            }
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// One entry of `webhook.allowed_targets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRule {
    /// `host:port`: this DNS name (lowercase) on this port.
    Host { name: String, port: u16 },
    /// `cidr:port` (`[v6/len]:port` for IPv6; an address alone is /32 or
    /// /128): addresses in the range on this port.
    Cidr { range: Cidr, port: u16 },
}

/// The operator's target policy: the allowlist and `webhook.allow_http`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TargetPolicy {
    rules: Vec<TargetRule>,
    allow_http: bool,
    /// Tests of the delivery itself only (feature `test-support`): a
    /// loopback address passes like any listed one. No configuration can
    /// set it.
    test_loopback: bool,
}

fn rule_invalid(index: usize) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", ALLOWED_TARGETS_KEY)
        .with_context("reason", "invalid")
        .with_context("entry", index)
}

fn port_of(raw: &str) -> Option<u16> {
    (!raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()))
        .then(|| raw.parse::<u16>().ok())
        .flatten()
        .filter(|p| *p > 0)
}

fn host_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl TargetPolicy {
    /// Parse `webhook.allowed_targets`: entries separated by commas or
    /// white space. A bad entry is `FG-SYS-0002
    /// config_key=webhook.allowed_targets reason=invalid` with its 1-based
    /// `entry` number — never its text.
    pub fn parse(text: &str, allow_http: bool) -> Result<Self, ForgeError> {
        let mut rules = Vec::new();
        for (index, entry) in text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|e| !e.is_empty())
            .enumerate()
        {
            let bad = || rule_invalid(index + 1);
            let rule = if let Some(rest) = entry.strip_prefix('[') {
                let (inside, port) = rest.split_once("]:").ok_or_else(bad)?;
                TargetRule::Cidr {
                    range: Cidr::parse(inside, true).ok_or_else(bad)?,
                    port: port_of(port).ok_or_else(bad)?,
                }
            } else {
                let (host, port) = entry.rsplit_once(':').ok_or_else(bad)?;
                let port = port_of(port).ok_or_else(bad)?;
                let looks_v4 = host
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.' || b == b'/');
                if looks_v4 {
                    TargetRule::Cidr {
                        range: Cidr::parse(host, false).ok_or_else(bad)?,
                        port,
                    }
                } else if host_name_ok(host) {
                    TargetRule::Host {
                        name: host.to_ascii_lowercase(),
                        port,
                    }
                } else {
                    return Err(bad());
                }
            };
            rules.push(rule);
        }
        Ok(Self {
            rules,
            allow_http,
            test_loopback: false,
        })
    }

    /// The same policy that also lets a loopback address through (the
    /// delivery tests' local receivers). Tests only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn allowing_loopback_for_tests(mut self) -> Self {
        self.test_loopback = true;
        self
    }

    /// The always-refused range of `ip` under this policy.
    fn denied(&self, ip: IpAddr) -> Option<&'static str> {
        denied_range(ip).filter(|r| !(self.test_loopback && *r == "loopback"))
    }

    pub fn allow_http(&self) -> bool {
        self.allow_http
    }

    /// No entry: nothing can be delivered.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[TargetRule] {
        &self.rules
    }

    fn name_listed(&self, name: &str, port: u16) -> bool {
        self.rules
            .iter()
            .any(|r| matches!(r, TargetRule::Host { name: n, port: p } if n == name && *p == port))
    }

    fn address_listed(&self, ip: IpAddr, port: u16) -> bool {
        self.rules
            .iter()
            .any(|r| matches!(r, TargetRule::Cidr { range, port: p } if *p == port && range.contains(ip)))
    }

    fn any_range_on(&self, port: u16) -> bool {
        self.rules
            .iter()
            .any(|r| matches!(r, TargetRule::Cidr { port: p, .. } if *p == port))
    }

    /// The checks a URL must pass when it is stored (and again before each
    /// delivery): the scheme, an always-refused address literal or a
    /// `localhost` name, and the allowlist. A name only an address range
    /// could admit is decided by [`Self::check_resolved`].
    pub fn check_url(&self, url: &WebhookUrl) -> Result<(), ForgeError> {
        let refuse = |reason| forbidden(reason).with_context("origin", url.origin());
        if !url.is_https() && !self.allow_http {
            return Err(refuse("scheme"));
        }
        match &url.host {
            TargetHost::Ip(ip) => {
                let ip = canonical(*ip);
                if let Some(range) = self.denied(ip) {
                    return Err(refuse(range));
                }
                if self.address_listed(ip, url.port) {
                    Ok(())
                } else {
                    Err(refuse("not_allowlisted"))
                }
            }
            TargetHost::Name(name) => {
                if name == "localhost" || name.ends_with(".localhost") {
                    return Err(refuse("loopback"));
                }
                if self.name_listed(name, url.port) || self.any_range_on(url.port) {
                    Ok(())
                } else {
                    Err(refuse("not_allowlisted"))
                }
            }
        }
    }

    /// The checks at delivery time on the addresses `url`'s name resolves
    /// to now: none is `reason=dns`; any always-refused address refuses
    /// the name (`resolved_denied`, with `address` and `range`); a name not
    /// listed itself needs every address in a listed range on the port
    /// (`resolved_not_allowlisted`). Returns the address to connect to (the
    /// first): the connection is pinned to it, so the name cannot resolve
    /// elsewhere between the check and the connect.
    pub fn check_resolved(&self, url: &WebhookUrl, addrs: &[IpAddr]) -> Result<IpAddr, ForgeError> {
        self.check_url(url)?;
        let refuse = |reason| forbidden(reason).with_context("origin", url.origin());
        let name = match &url.host {
            TargetHost::Ip(ip) => return Ok(canonical(*ip)),
            TargetHost::Name(name) => name,
        };
        let first = addrs
            .first()
            .copied()
            .map(canonical)
            .ok_or_else(|| refuse("dns"))?;
        for a in addrs.iter().copied().map(canonical) {
            if let Some(range) = self.denied(a) {
                return Err(refuse("resolved_denied")
                    .with_context("address", a)
                    .with_context("range", range));
            }
        }
        if self.name_listed(name, url.port) {
            return Ok(first);
        }
        if let Some(a) = addrs
            .iter()
            .copied()
            .map(canonical)
            .find(|a| !self.address_listed(*a, url.port))
        {
            return Err(refuse("resolved_not_allowlisted").with_context("address", a));
        }
        Ok(first)
    }
}

/// The hex HMAC-SHA256 of `t "." delivery_id "." body` under `secret`.
pub fn signature(
    secret: &str,
    t: u64,
    delivery_id: &str,
    body: &[u8],
) -> Result<String, ForgeError> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "webhook_signature_key")
    })?;
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(delivery_id.as_bytes());
    mac.update(b".");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `t=<t>,v1=<sig>[,v1=<sig>]`.
pub fn signature_header(t: u64, signatures: &[String]) -> String {
    let mut out = format!("t={t}");
    for s in signatures {
        out.push_str(",v1=");
        out.push_str(s);
    }
    out
}

/// A webhook's secret. Never printed; shown to the person once; wiped
/// from memory when dropped (`zeroize`).
pub struct WebhookSecret(zeroize::Zeroizing<String>);

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookSecret([redacted])")
    }
}

fn rng_unavailable() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "rng_unavailable")
}

impl WebhookSecret {
    /// 32 random bytes from the operating system as `whsec_<base64url>`.
    pub fn generate() -> Result<Self, ForgeError> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|_| rng_unavailable())?;
        let secret = Self(zeroize::Zeroizing::new(format!(
            "{SECRET_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(bytes)
        )));
        zeroize::Zeroize::zeroize(&mut bytes);
        Ok(secret)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A secret as stored: the key it was sealed with and `nonce | ciphertext |
/// tag`.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedSecret {
    pub key_id: i16,
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for SealedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SealedSecret({}, [{} bytes])",
            self.key_id,
            self.ciphertext.len()
        )
    }
}

const SEAL_DOMAIN: &[u8] = b"forge-webhook-secret/1";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAX_KEYS_BYTES: u64 = 64 * 1024;

struct SecretKey {
    id: i16,
    cipher: Aes256Gcm,
}

/// The keys of `webhook.secret_keys_file`: one `<key_id> <64 hex digits>`
/// per line (key_id 1..=32767; blank lines and `#` comments ignored). The
/// highest key_id seals; every listed key opens, so a rotation adds a key
/// and keeps the old one until every secret was sealed again.
pub struct WebhookSecretKeys {
    newest: SecretKey,
    older: Vec<SecretKey>,
}

impl std::fmt::Debug for WebhookSecretKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WebhookSecretKeys(newest {}, {} older)",
            self.newest.id,
            self.older.len()
        )
    }
}

fn keys_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::ConfigInvalid)
        .with_context("config_key", SECRET_KEYS_KEY)
        .with_context("reason", reason)
}

fn aad(uid: &uuid::Uuid) -> Vec<u8> {
    let mut aad = SEAL_DOMAIN.to_vec();
    aad.extend_from_slice(uid.as_bytes());
    aad
}

impl WebhookSecretKeys {
    /// Parse the key file's text. Errors are `FG-SYS-0002
    /// config_key=webhook.secret_keys_file` with `reason` and `line`, never
    /// the line's content.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let mut keys: Vec<SecretKey> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |reason| keys_invalid(reason).with_context("line", index + 1);
            let mut parts = line.split_whitespace();
            let (Some(id), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("invalid"));
            };
            let id: i16 = Some(id)
                .filter(|k| k.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|k| k.parse().ok())
                .filter(|k| *k >= 1)
                .ok_or_else(|| bad("invalid_key_id"))?;
            let bytes = crate::config::decode_hex(hex)
                .filter(|b| b.len() == 32)
                .ok_or_else(|| bad("invalid_key"))?;
            if keys.iter().any(|k| k.id == id) {
                return Err(bad("duplicate"));
            }
            let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| bad("invalid_key"))?;
            keys.push(SecretKey { id, cipher });
        }
        keys.sort_by_key(|k| k.id);
        let newest = keys.pop().ok_or_else(|| keys_invalid("empty"))?;
        Ok(Self {
            newest,
            older: keys,
        })
    }

    /// Read and parse the file at `path` (a secret file: bounded, never
    /// printed).
    pub fn read(path: &std::path::Path) -> Result<Self, ForgeError> {
        let text = crate::config::read_secret_text(SECRET_KEYS_KEY, path, MAX_KEYS_BYTES)?;
        Self::parse(&text).map_err(|e| e.with_context("path", path.display()))
    }

    /// The key that seals new secrets.
    pub fn sealing_key_id(&self) -> i16 {
        self.newest.id
    }

    fn get(&self, id: i16) -> Option<&SecretKey> {
        std::iter::once(&self.newest)
            .chain(&self.older)
            .find(|k| k.id == id)
    }

    /// Seal `secret` for the webhook whose row has `uid`, with the newest
    /// key.
    pub fn seal(
        &self,
        uid: &uuid::Uuid,
        secret: &WebhookSecret,
    ) -> Result<SealedSecret, ForgeError> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce).map_err(|_| rng_unavailable())?;
        let aad = aad(uid);
        let sealed = self
            .newest
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret.expose().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "webhook_secret_seal")
            })?;
        let mut ciphertext = Vec::with_capacity(NONCE_BYTES + sealed.len());
        ciphertext.extend_from_slice(&nonce);
        ciphertext.extend_from_slice(&sealed);
        Ok(SealedSecret {
            key_id: self.newest.id,
            ciphertext,
        })
    }

    /// Open a stored secret of the webhook whose row has `uid`. A key the
    /// file no longer has is `FG-SYS-0002 reason=key_missing` (with
    /// `key_id`); a ciphertext that does not open (another row's, changed)
    /// is `FG-SYS-0001 reason=webhook_secret_unreadable`.
    pub fn open(
        &self,
        uid: &uuid::Uuid,
        key_id: i16,
        ciphertext: &[u8],
    ) -> Result<WebhookSecret, ForgeError> {
        let key = self
            .get(key_id)
            .ok_or_else(|| keys_invalid("key_missing").with_context("key_id", key_id))?;
        let unreadable = || {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "webhook_secret_unreadable")
        };
        if ciphertext.len() < NONCE_BYTES + TAG_BYTES {
            return Err(unreadable());
        }
        let (nonce, sealed) = ciphertext.split_at(NONCE_BYTES);
        let aad = aad(uid);
        let plain = key
            .cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| unreadable())?;
        let text = zeroize::Zeroizing::new(String::from_utf8(plain).map_err(|_| unreadable())?);
        if !text.starts_with(SECRET_PREFIX) {
            return Err(unreadable());
        }
        Ok(WebhookSecret(text))
    }
}

/// Whether `raw` is one entry of a webhook's `event_types`: [`ALL_EVENT_TYPES`]
/// or `<family>.*` (integration-class types of the family, at least one
/// today), or the exact name of an integration-, maintainer- or
/// deployment-class type. The admin class is never a webhook's.
pub fn webhook_event_type_entry_ok(raw: &str) -> bool {
    if raw == ALL_EVENT_TYPES {
        return true;
    }
    match raw.strip_suffix(".*") {
        Some(family) if !family.is_empty() && !family.contains('*') => EventType::ALL
            .iter()
            .filter(|t| t.visibility_class() == VisibilityClass::Integration)
            .any(|t| in_family(*t, family)),
        Some(_) => false,
        None => {
            EventType::parse(raw).is_some_and(|t| t.visibility_class() != VisibilityClass::Admin)
        }
    }
}

fn in_family(t: EventType, family: &str) -> bool {
    t.as_str()
        .strip_prefix(family)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// Check a webhook's `event_types` (1..=64 entries, each
/// [`webhook_event_type_entry_ok`]) and return them sorted without
/// duplicates. `FG-WEB-0006 field=event_types` with `rule` (`count`,
/// `unknown_event_type`) otherwise.
pub fn webhook_event_types(entries: &[String]) -> Result<Vec<String>, ForgeError> {
    let invalid = |rule: &'static str| {
        ForgeError::new(ErrorCode::RequestInvalid)
            .with_context("reason", "field_invalid")
            .with_context("field", "event_types")
            .with_context("rule", rule)
    };
    if entries.is_empty() || entries.len() > MAX_EVENT_TYPE_ENTRIES {
        return Err(invalid("count"));
    }
    if !entries.iter().all(|e| webhook_event_type_entry_ok(e)) {
        return Err(invalid("unknown_event_type"));
    }
    let mut out = entries.to_vec();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The event types `entries` admit among the classes the owner may
/// receive, sorted by name: `*` / `<family>.*` the integration class, a
/// name its own class; the admin class never.
pub fn admitted_event_types(entries: &[String], classes: &[VisibilityClass]) -> Vec<EventType> {
    let mut out: Vec<EventType> = EventType::ALL
        .into_iter()
        .filter(|t| {
            let class = t.visibility_class();
            class != VisibilityClass::Admin
                && classes.contains(&class)
                && entries.iter().any(|e| {
                    e == t.as_str()
                        || (class == VisibilityClass::Integration
                            && (e == ALL_EVENT_TYPES
                                || e.strip_suffix(".*").is_some_and(|f| in_family(*t, f))))
                })
        })
        .collect();
    out.sort_by_key(|t| t.as_str());
    out
}

/// What forge-auth's `prove_webhook_delivery` decides on: the webhook as
/// stored and its owner's standing on the project, read together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookMaterial {
    pub webhook: WebhookId,
    pub project: ProjectId,
    pub enabled: bool,
    /// The allowlist as stored.
    pub event_types: Vec<String>,
    pub owner: UserId,
    pub owner_active: bool,
    pub owner_admin: bool,
    pub owner_kind: UserKind,
    /// The owner's access facts on the project (`None`: the project row is
    /// gone).
    pub access: Option<ProjectAccess>,
}

/// The events one webhook may carry now: the classes `authorize()` allowed
/// its owner on its project, narrowed by its allowlist. Built only by
/// forge-auth (`prove_webhook_delivery`); used for one delivery decision
/// and never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookDeliverySet {
    webhook: WebhookId,
    project: ProjectId,
    event_types: Vec<EventType>,
}

impl WebhookDeliverySet {
    /// The only constructor (callers outside forge-auth are refused by the
    /// decided-callers gate).
    pub fn from_authorized_decisions(
        webhook: WebhookId,
        project: ProjectId,
        classes: &[VisibilityClass],
        allowlist: &[String],
    ) -> Self {
        Self {
            webhook,
            project,
            event_types: admitted_event_types(allowlist, classes),
        }
    }

    pub fn webhook(&self) -> WebhookId {
        self.webhook
    }

    pub fn project(&self) -> ProjectId {
        self.project
    }

    pub fn admits(&self, t: EventType) -> bool {
        self.event_types.contains(&t)
    }
}

/// At most [`RESPONSE_HEAD_CHARS`] printable characters of an answer's
/// body for one log line: bytes outside printable ASCII become `?` (white
/// space a space), and every run of 20 or more token-like characters
/// (`A-Z a-z 0-9 _ - + / =`) becomes `[redacted]` before the cut.
pub fn response_head(body: &[u8]) -> String {
    let printable: String = body
        .iter()
        .take(MAX_RESPONSE_BYTES)
        .map(|b| match *b {
            b' '..=b'~' => char::from(*b),
            b'\n' | b'\r' | b'\t' => ' ',
            _ => '?',
        })
        .collect();
    let token = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=');
    let mut out = String::with_capacity(printable.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 20 {
            out.push_str("[redacted]");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in printable.chars() {
        if token(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out.chars().take(RESPONSE_HEAD_CHARS).collect()
}

/// A field `webhook.updated` changed (names only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WebhookField {
    Url,
    EventTypes,
    Enabled,
    /// The URL or event types changed by someone else than the owner: the
    /// caller answers for the webhook now (I2 review).
    Owner,
}

impl WebhookField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::EventTypes => "event_types",
            Self::Enabled => "enabled",
            Self::Owner => "owner",
        }
    }
}

/// Why a webhook is disabled (`webhooks.disabled_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableReason {
    /// A Maintainer disabled it.
    Manual,
    /// [`DISABLE_AFTER_FAILURES`] failed attempts in a row.
    Failures,
    /// Failing without a success for [`DISABLE_AFTER_FAILING`].
    Failing24h,
    /// `forge restore finish` disabled it: its signing secret may be one
    /// rotated away after the backup (M3 O1). Rotate the secret, then enable.
    Restore,
}

impl DisableReason {
    pub const ALL: [DisableReason; 4] = [
        Self::Manual,
        Self::Failures,
        Self::Failing24h,
        Self::Restore,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Failures => "failures",
            Self::Failing24h => "failing_24h",
            Self::Restore => "restore",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == raw)
    }
}

/// The audit details of a webhook change (`webhook.*`, M3 I2), all on the
/// webhook's project. Only the automatic disable publishes an event
/// (`webhook.disabled`); the URL appears as its origin only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookChange {
    /// `webhook.created`.
    Created {
        webhook: WebhookId,
        project: ProjectId,
        origin: String,
        event_types: Vec<String>,
        secret_version: i32,
    },
    /// `webhook.updated`: the fields changed and the state after
    /// (`owner_from` → `owner`: the owner before and after).
    Updated {
        webhook: WebhookId,
        project: ProjectId,
        owner_from: UserId,
        owner: UserId,
        fields: Vec<WebhookField>,
        origin: String,
        enabled: bool,
        event_types: Vec<String>,
    },
    /// `webhook.deleted` (its delivery history goes with it).
    Deleted {
        webhook: WebhookId,
        project: ProjectId,
        origin: String,
    },
    /// `webhook.secret_rotated`: the new version; the previous secret
    /// still signs until `previous_valid_until` (RFC 3339, UTC).
    SecretRotated {
        webhook: WebhookId,
        project: ProjectId,
        /// The rotation makes the caller the owner.
        owner_from: UserId,
        owner: UserId,
        secret_version: i32,
        previous_valid_until: String,
    },
    /// `webhook.disabled` by the worker (actor `system`; the event).
    Disabled {
        webhook: WebhookId,
        project: ProjectId,
        reason: DisableReason,
        consecutive_failures: i32,
        /// RFC 3339, UTC.
        failing_since: Option<String>,
    },
    /// `webhook.redelivered`: a person asked for one delivery again.
    Redelivered {
        webhook: WebhookId,
        project: ProjectId,
        delivery: crate::ids::WebhookDeliveryId,
    },
}

impl WebhookChange {
    pub fn action(&self) -> &'static str {
        match self {
            Self::Created { .. } => "webhook.created",
            Self::Updated { .. } => "webhook.updated",
            Self::Deleted { .. } => "webhook.deleted",
            Self::SecretRotated { .. } => "webhook.secret_rotated",
            Self::Disabled { .. } => "webhook.disabled",
            Self::Redelivered { .. } => "webhook.redelivered",
        }
    }

    /// `webhook.disabled` publishes [`EventType::WebhookDisabled`]; the
    /// others are audit-only.
    pub fn event_type(&self) -> Option<EventType> {
        match self {
            Self::Disabled { .. } => Some(EventType::WebhookDisabled),
            Self::Created { .. }
            | Self::Updated { .. }
            | Self::Deleted { .. }
            | Self::SecretRotated { .. }
            | Self::Redelivered { .. } => None,
        }
    }

    /// Created `{"webhook_id","project_id","origin","event_types","secret_version"}`,
    /// Updated `{"webhook_id","project_id","owner_from","owner_id","fields","origin","enabled","event_types"}`,
    /// Deleted `{"webhook_id","project_id","origin"}`, SecretRotated
    /// `{"webhook_id","project_id","owner_from","owner_id","secret_version","previous_valid_until"}`,
    /// Disabled `{"webhook_id","project_id","reason","consecutive_failures","failing_since"}`,
    /// Redelivered `{"webhook_id","project_id","delivery_id"}`.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Created {
                webhook,
                project,
                origin,
                event_types,
                secret_version,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "origin": origin,
                "event_types": event_types,
                "secret_version": secret_version,
            }),
            Self::Updated {
                webhook,
                project,
                owner_from,
                owner,
                fields,
                origin,
                enabled,
                event_types,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "owner_from": owner_from.get(),
                "owner_id": owner.get(),
                "fields": fields.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
                "origin": origin,
                "enabled": enabled,
                "event_types": event_types,
            }),
            Self::Deleted {
                webhook,
                project,
                origin,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "origin": origin,
            }),
            Self::SecretRotated {
                webhook,
                project,
                owner_from,
                owner,
                secret_version,
                previous_valid_until,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "owner_from": owner_from.get(),
                "owner_id": owner.get(),
                "secret_version": secret_version,
                "previous_valid_until": previous_valid_until,
            }),
            Self::Disabled {
                webhook,
                project,
                reason,
                consecutive_failures,
                failing_since,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "reason": reason.as_str(),
                "consecutive_failures": consecutive_failures,
                "failing_since": failing_since,
            }),
            Self::Redelivered {
                webhook,
                project,
                delivery,
            } => serde_json::json!({
                "webhook_id": webhook.get(),
                "project_id": project.get(),
                "delivery_id": delivery.get(),
            }),
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_retry_delay_grows_fourfold_up_to_four_hours() {
        assert_eq!(retry_delay(1), Duration::from_secs(10));
        assert_eq!(retry_delay(2), Duration::from_secs(40));
        assert_eq!(retry_delay(5), Duration::from_secs(2560));
        assert_eq!(retry_delay(6), Duration::from_secs(10_240));
        assert_eq!(retry_delay(7), Duration::from_secs(4 * 3600));
        assert_eq!(retry_delay(100), Duration::from_secs(4 * 3600));
        assert_eq!(retry_delay(0), Duration::from_secs(10));
        let total: Duration = (1..MAX_ATTEMPTS).map(retry_delay).sum();
        assert!(total < Duration::from_secs(24 * 3600), "{total:?}");
    }
}
