//! CI variables — the project's secrets for its jobs (M4 C6, plan 14 §4.1,
//! §9 #2; ADR-0013, ADR-0029 decision 8 and its C6 amendment).
//!
//! - **The name** ([`check_name`]): `[A-Z][A-Z0-9_]*`, at most 63 bytes
//!   (the `secrets:` names of `.forge-ci.yml`), not one the runner sets
//!   itself (`CI`, `PATH`, `HOME`, `HOSTNAME`, `FORGE_*`, `CI_*`).
//! - **The value** ([`check_value`]): 1..=[`MAX_VALUE_BYTES`] bytes of one
//!   line (the runner hands it to Podman in an env file: no line break, no
//!   NUL or other control character, no white space at either end, no
//!   leading quote). A masked value is at least [`MIN_MASKED_BYTES`] long
//!   (a shorter one would mask ordinary words of the log).
//! - **At rest** ([`CiSecretKeys`]): AES-256-GCM under a key of
//!   `ci.secret_keys_file` (`<key_id> <64 hex>` per line; the highest
//!   key_id seals, every listed key opens), the associated data binding the
//!   ciphertext to its project and name: a value copied to another row does
//!   not open (`FG-CI-0016 reason=value_unreadable`).
//! - **In logs** ([`mask_forms`], [`mask`]): the value and its common
//!   encodings are replaced by [`MASK`]: base64 standard and URL-safe
//!   (padded and not, and the part of the encoding the value alone decides
//!   at each of the three byte offsets — so `user:secret` encoded whole is
//!   caught), base64 of the value with a trailing newline (`echo $X |
//!   base64`), hex (lower and upper case), percent-encoding and JSON string
//!   escaping. The runner masks before it sends a chunk and forge masks
//!   again before it stores one; the same forms refuse an artifact of a job
//!   that received the value (FG-CI-0021). **Limits**: a value transformed
//!   otherwise (encrypted, compressed, reversed, split across lines, other
//!   encodings, a base64 part shorter than 8 characters) is not found —
//!   masking is a safety net, not a boundary (ADR-0029: the outputs inherit
//!   the repository's marks; artifacts of jobs with secrets are
//!   Maintainers' only).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use forge_errors::{ErrorCode, ForgeError};

use crate::ids::ProjectId;

/// The configuration key of the keys (errors name it).
pub const SECRET_KEYS_KEY: &str = "ci.secret_keys_file";
/// The longest value, in bytes.
pub const MAX_VALUE_BYTES: usize = 4096;
/// The shortest value that may be masked, in bytes.
pub const MIN_MASKED_BYTES: usize = 8;
/// The longest name, in bytes.
pub const MAX_NAME_BYTES: usize = 63;
/// The most variables of one project.
pub const MAX_VARIABLES_PER_PROJECT: i64 = 200;
/// What a masked value becomes in a log.
pub const MASK: &str = "[masked]";

const SEAL_DOMAIN: &[u8] = b"forge-ci-variable/1";
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const MAX_KEYS_BYTES: u64 = 64 * 1024;

/// Why a name is refused (`None`: valid): `invalid_name` or
/// `reserved_name`.
pub fn check_name(name: &str) -> Option<&'static str> {
    let mut chars = name.chars();
    let shaped = name.len() <= MAX_NAME_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !shaped {
        return Some("invalid_name");
    }
    let reserved = matches!(name, "CI" | "PATH" | "HOME" | "HOSTNAME")
        || name.starts_with("FORGE_")
        || name.starts_with("CI_");
    reserved.then_some("reserved_name")
}

/// Why a value is refused (`None`: valid): `empty`, `too_long`,
/// `control_character` (a line break, NUL, …), `edge_whitespace`,
/// `leading_quote`, `too_short_to_mask`. Never names the value.
pub fn check_value(value: &str, masked: bool) -> Option<&'static str> {
    if value.is_empty() {
        return Some("empty");
    }
    if value.len() > MAX_VALUE_BYTES {
        return Some("too_long");
    }
    if value.chars().any(char::is_control) {
        return Some("control_character");
    }
    if value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace) {
        return Some("edge_whitespace");
    }
    if value.starts_with(['"', '\'']) {
        return Some("leading_quote");
    }
    if masked && value.len() < MIN_MASKED_BYTES {
        return Some("too_short_to_mask");
    }
    None
}

/// A variable's value. Never printed; wiped from memory when dropped.
#[derive(Clone, PartialEq, Eq)]
pub struct VariableValue(zeroize::Zeroizing<String>);

impl std::fmt::Debug for VariableValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VariableValue([redacted])")
    }
}

impl VariableValue {
    pub fn new(value: String) -> Self {
        Self(zeroize::Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// A value as stored: the key it was sealed with and `nonce | ciphertext |
/// tag`.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedValue {
    pub key_id: i16,
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for SealedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SealedValue({}, [{} bytes])",
            self.key_id,
            self.ciphertext.len()
        )
    }
}

struct SecretKey {
    id: i16,
    cipher: Aes256Gcm,
}

/// The keys of `ci.secret_keys_file` (module docs).
pub struct CiSecretKeys {
    newest: SecretKey,
    older: Vec<SecretKey>,
}

impl std::fmt::Debug for CiSecretKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "CiSecretKeys(newest {}, {} older)",
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

/// `FG-CI-0016` with `reason` (module docs of the code in errors.toml).
pub fn key_unavailable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SecretKeyUnavailable).with_context("reason", reason)
}

fn rng_unavailable() -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", "rng_unavailable")
}

fn aad(project: ProjectId, name: &str) -> Vec<u8> {
    let mut aad = SEAL_DOMAIN.to_vec();
    aad.extend_from_slice(&project.get().to_be_bytes());
    aad.push(0);
    aad.extend_from_slice(name.as_bytes());
    aad
}

impl CiSecretKeys {
    /// Parse the key file's text. Errors are `FG-SYS-0002
    /// config_key=ci.secret_keys_file` with `reason` and `line`, never the
    /// line's content.
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
            let mut bytes = crate::config::decode_hex(hex)
                .filter(|b| b.len() == 32)
                .ok_or_else(|| bad("invalid_key"))?;
            if keys.iter().any(|k| k.id == id) {
                return Err(bad("duplicate"));
            }
            let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| bad("invalid_key"))?;
            zeroize::Zeroize::zeroize(&mut bytes);
            keys.push(SecretKey { id, cipher });
        }
        keys.sort_by_key(|k| k.id);
        let newest = keys.pop().ok_or_else(|| keys_invalid("empty"))?;
        Ok(Self {
            newest,
            older: keys,
        })
    }

    /// Read and parse the file at `path` (a secret file: absolute, owner
    /// only on Unix, bounded, never printed).
    pub fn read(path: &std::path::Path) -> Result<Self, ForgeError> {
        let text = zeroize::Zeroizing::new(crate::config::read_secret_text(
            SECRET_KEYS_KEY,
            path,
            MAX_KEYS_BYTES,
        )?);
        Self::parse(&text).map_err(|e| e.with_context("path", path.display()))
    }

    /// The key that seals new values.
    pub fn sealing_key_id(&self) -> i16 {
        self.newest.id
    }

    /// Whether the file still lists `key_id`.
    pub fn has_key(&self, key_id: i16) -> bool {
        self.get(key_id).is_some()
    }

    fn get(&self, id: i16) -> Option<&SecretKey> {
        std::iter::once(&self.newest)
            .chain(&self.older)
            .find(|k| k.id == id)
    }

    /// Seal `value` of the variable `name` of `project` with the newest key.
    pub fn seal(
        &self,
        project: ProjectId,
        name: &str,
        value: &VariableValue,
    ) -> Result<SealedValue, ForgeError> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::getrandom(&mut nonce).map_err(|_| rng_unavailable())?;
        let aad = aad(project, name);
        let sealed = self
            .newest
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: value.expose().as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| {
                ForgeError::new(ErrorCode::Internal).with_context("reason", "ci_variable_seal")
            })?;
        let mut ciphertext = Vec::with_capacity(NONCE_BYTES + sealed.len());
        ciphertext.extend_from_slice(&nonce);
        ciphertext.extend_from_slice(&sealed);
        Ok(SealedValue {
            key_id: self.newest.id,
            ciphertext,
        })
    }

    /// Open the stored value of `name` of `project`. A key the file no
    /// longer lists is `FG-CI-0016 reason=key_missing` (with `key_id`); a
    /// ciphertext that does not open (another row's, changed) is
    /// `FG-CI-0016 reason=value_unreadable`.
    pub fn open(
        &self,
        project: ProjectId,
        name: &str,
        sealed: &SealedValue,
    ) -> Result<VariableValue, ForgeError> {
        let key = self
            .get(sealed.key_id)
            .ok_or_else(|| key_unavailable("key_missing").with_context("key_id", sealed.key_id))?;
        let unreadable = || {
            key_unavailable("value_unreadable")
                .with_context("key_id", sealed.key_id)
                .with_context("project_id", project.get())
        };
        if sealed.ciphertext.len() < NONCE_BYTES + TAG_BYTES {
            return Err(unreadable());
        }
        let (nonce, body) = sealed.ciphertext.split_at(NONCE_BYTES);
        let aad = aad(project, name);
        let plain = key
            .cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: body,
                    aad: &aad,
                },
            )
            .map_err(|_| unreadable())?;
        let text = String::from_utf8(plain).map_err(|_| unreadable())?;
        Ok(VariableValue::new(text))
    }
}

/// The forms of `value` a log is masked for (module docs): the value
/// itself and its common encodings, each at least [`MIN_MASKED_BYTES`]
/// long, longest first, without duplicates.
pub fn mask_forms(value: &str) -> Vec<String> {
    if value.len() < MIN_MASKED_BYTES {
        return Vec::new();
    }
    let bytes = value.as_bytes();
    let mut with_newline = bytes.to_vec();
    with_newline.push(b'\n');
    let mut forms = vec![
        value.to_owned(),
        STANDARD.encode(bytes),
        STANDARD_NO_PAD.encode(bytes),
        URL_SAFE.encode(bytes),
        URL_SAFE_NO_PAD.encode(bytes),
        STANDARD.encode(&with_newline),
        STANDARD_NO_PAD.encode(&with_newline),
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
            .to_string(),
        bytes.iter().map(|b| format!("{b:02x}")).collect(),
        bytes.iter().map(|b| format!("{b:02X}")).collect(),
    ];
    // JSON string escaping (without the quotes) when it differs.
    if let Ok(json) = serde_json::to_string(value) {
        let inner = json.trim_start_matches('"').trim_end_matches('"');
        forms.push(inner.to_owned());
    }
    // The value inside a longer base64 text: at each byte offset, the
    // characters only the value's bits decide.
    for offset in 1..3usize {
        let mut shifted = vec![0u8; offset];
        shifted.extend_from_slice(bytes);
        for encoded in [STANDARD.encode(&shifted), URL_SAFE.encode(&shifted)] {
            forms.push(base64_core(&encoded, offset, bytes.len()));
        }
    }
    for encoded in [STANDARD.encode(bytes), URL_SAFE.encode(bytes)] {
        forms.push(base64_core(&encoded, 0, bytes.len()));
    }
    forms.retain(|f| f.len() >= MIN_MASKED_BYTES);
    forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    forms.dedup();
    forms
}

/// The characters of `encoded` (base64 of `offset` other bytes then a
/// value of `len` bytes) that only the value's bits decide: from the first
/// character starting at or after bit `8 * offset` to the last one ending
/// at or before the value's last bit.
fn base64_core(encoded: &str, offset: usize, len: usize) -> String {
    let first = (8 * offset).div_ceil(6);
    let end = (8 * (offset + len)) / 6;
    encoded.get(first..end).unwrap_or("").to_owned()
}

/// Whether `bytes` holds any of `forms` (an artifact of a job that received
/// secrets, FG-CI-0021).
pub fn contains_any(bytes: &[u8], forms: &[String]) -> bool {
    forms.iter().any(|f| {
        let f = f.as_bytes();
        !f.is_empty() && bytes.windows(f.len()).any(|w| w == f)
    })
}

/// `text` with every one of `forms` replaced by [`MASK`] (the forms as
/// [`mask_forms`] returns them: longest first, so a longer form is
/// replaced before a shorter one it contains).
pub fn mask(text: &str, forms: &[String]) -> String {
    let mut out = text.to_owned();
    for form in forms {
        if !form.is_empty() && out.contains(form.as_str()) {
            out = out.replace(form.as_str(), MASK);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

    use super::*;

    const KEY_A: &str = "1 0000000000000000000000000000000000000000000000000000000000000001";
    const KEY_B: &str = "2 0000000000000000000000000000000000000000000000000000000000000002";

    fn keys(text: &str) -> CiSecretKeys {
        CiSecretKeys::parse(text).expect("keys")
    }

    #[test]
    fn names_follow_the_secret_names_of_the_definition_and_leave_the_runners_own() {
        assert_eq!(check_name("DEPLOY_TOKEN"), None);
        assert_eq!(check_name("A1"), None);
        for bad in ["", "deploy", "1A", "A-B", &"A".repeat(64)] {
            assert_eq!(check_name(bad), Some("invalid_name"), "{bad}");
        }
        for reserved in ["CI", "PATH", "HOME", "HOSTNAME", "FORGE_JOB_ID", "CI_TOKEN"] {
            assert_eq!(check_name(reserved), Some("reserved_name"), "{reserved}");
        }
    }

    #[test]
    fn values_are_one_line_without_edges_and_masked_ones_are_long_enough() {
        assert_eq!(check_value("s3cr3t-value", true), None);
        assert_eq!(check_value("short", false), None);
        assert_eq!(check_value("short", true), Some("too_short_to_mask"));
        assert_eq!(check_value("", false), Some("empty"));
        assert_eq!(
            check_value("a\nb-long-value", false),
            Some("control_character")
        );
        assert_eq!(
            check_value("a\0b-long-value", false),
            Some("control_character")
        );
        assert_eq!(check_value(" padded-value", false), Some("edge_whitespace"));
        assert_eq!(check_value("padded-value ", false), Some("edge_whitespace"));
        assert_eq!(check_value("\"quoted-value", false), Some("leading_quote"));
        assert_eq!(
            check_value(&"x".repeat(MAX_VALUE_BYTES + 1), false),
            Some("too_long")
        );
    }

    #[test]
    fn a_sealed_value_opens_only_for_its_project_and_name() {
        let k = keys(&format!("{KEY_A}\n{KEY_B}\n"));
        assert_eq!(k.sealing_key_id(), 2);
        let value = VariableValue::new("s3cr3t-value".to_owned());
        let sealed = k.seal(ProjectId::new(7), "TOKEN", &value).expect("seal");
        assert_eq!(sealed.key_id, 2);
        assert!(!format!("{sealed:?}").contains("s3cr3t"));
        let opened = k.open(ProjectId::new(7), "TOKEN", &sealed).expect("open");
        assert_eq!(opened.expose(), "s3cr3t-value");
        assert!(!format!("{opened:?}").contains("s3cr3t"));
        for (project, name) in [(8, "TOKEN"), (7, "OTHER")] {
            let e = k
                .open(ProjectId::new(project), name, &sealed)
                .expect_err("bound to its row");
            assert_eq!(e.code(), ErrorCode::SecretKeyUnavailable);
            assert!(e.to_string().contains("value_unreadable"), "{e}");
        }
    }

    #[test]
    fn a_value_sealed_with_a_removed_key_is_key_missing() {
        let both = keys(&format!("{KEY_A}\n{KEY_B}\n"));
        let sealed = both
            .seal(
                ProjectId::new(1),
                "X",
                &VariableValue::new("value-123".to_owned()),
            )
            .expect("seal");
        let old_only = keys(KEY_A);
        let e = old_only
            .open(ProjectId::new(1), "X", &sealed)
            .expect_err("key 2 is gone");
        assert_eq!(e.code(), ErrorCode::SecretKeyUnavailable);
        assert!(e.to_string().contains("key_missing"), "{e}");
        assert!(both.has_key(1) && !old_only.has_key(2));
    }

    #[test]
    fn a_bad_key_file_names_the_line_but_never_its_content() {
        for (text, reason) in [
            ("", "empty"),
            ("1 abc", "invalid_key"),
            ("0 00", "invalid_key_id"),
            ("1", "invalid"),
            (&format!("{KEY_A}\n{KEY_A}") as &str, "duplicate"),
        ] {
            let e = CiSecretKeys::parse(text).expect_err(reason);
            assert_eq!(e.code(), ErrorCode::ConfigInvalid);
            let shown = e.to_string();
            assert!(shown.contains(reason), "{shown}");
            assert!(!shown.contains("0000000000000001"), "{shown}");
        }
    }

    #[test]
    fn masking_covers_the_value_and_its_common_encodings() {
        let value = "s3cr3t/value+x";
        let forms = mask_forms(value);
        let encoded = [
            value.to_owned(),
            STANDARD.encode(value),
            URL_SAFE_NO_PAD.encode(value),
            STANDARD.encode(format!("{value}\n")),
            "s3cr3t%2Fvalue%2Bx".to_owned(),
        ];
        for e in &encoded {
            let text = format!("before {e} after");
            assert_eq!(mask(&text, &forms), "before [masked] after", "{e}");
        }
        assert!(mask_forms("short").is_empty());
        // Hex, JSON escaping and base64 at every byte offset (a value
        // encoded with a prefix such as `user:`).
        let quoted = "pa\"ss/word-12";
        let quoted_forms = mask_forms(quoted);
        for e in [
            quoted
                .bytes()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            quoted
                .bytes()
                .map(|b| format!("{b:02X}"))
                .collect::<String>(),
            "pa\\\"ss/word-12".to_owned(),
        ] {
            assert!(contains_any(e.as_bytes(), &quoted_forms), "{e}");
        }
        for prefix in ["", "u:", "us:", "use:", "user:"] {
            let blob = STANDARD.encode(format!("{prefix}{value}-and-more"));
            assert!(
                contains_any(blob.as_bytes(), &forms),
                "base64 with prefix {prefix:?} not found"
            );
            let url = URL_SAFE_NO_PAD.encode(format!("{prefix}{value}"));
            assert!(contains_any(url.as_bytes(), &forms), "{prefix:?}");
        }
        assert_eq!(mask("nothing here", &forms), "nothing here");
    }
}
