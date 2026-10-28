//! `[audit]`: the HMAC keys of the audit seal (ADR-0018 §4, ADR-0013).
//!
//! `audit.seal_keys_file` names a secret file outside the database, one key
//! per line: `<key_id> <hex>` — key_id 1..=32767, the key 32..=64 bytes as
//! hex. Blank lines and lines starting with `#` are ignored. The highest
//! key_id signs new seals; the older ones stay to verify the seals they
//! signed (keys are only ever added). The path is checked at load time; the
//! command that seals or verifies reads the file ([`AuditConfig::seal_keys`]).

use std::path::{Path, PathBuf};

use forge_errors::ForgeError;

use super::{invalid, secret};

const KEY: &str = "audit.seal_keys_file";

/// Upper bound for the key file (a mistaken `/dev/zero` must not exhaust
/// memory; 64 KiB holds hundreds of keys).
const MAX_KEYS_BYTES: u64 = 64 * 1024;
const MIN_KEY_BYTES: usize = 32;
const MAX_KEY_BYTES: usize = 64;

#[derive(Debug)]
pub struct AuditConfig {
    pub(super) seal_keys_file: Option<PathBuf>,
}

impl AuditConfig {
    pub(super) fn new(seal_keys_file: Option<PathBuf>) -> Self {
        Self { seal_keys_file }
    }

    /// Whether `audit.seal_keys_file` is configured.
    pub fn has_seal_keys(&self) -> bool {
        self.seal_keys_file.is_some()
    }

    /// Read `audit.seal_keys_file` now (`None` when it is not configured).
    pub fn seal_keys(&self) -> Result<Option<SealKeys>, ForgeError> {
        self.seal_keys_file
            .as_deref()
            .map(read_seal_keys)
            .transpose()
    }
}

fn read_seal_keys(path: &Path) -> Result<SealKeys, ForgeError> {
    let text = secret::read_secret_text(KEY, path, MAX_KEYS_BYTES)?;
    SealKeys::parse(&text).map_err(|e| e.with_context("path", path.display()))
}

/// One HMAC key of the audit seal. Never printed.
pub struct SealKey {
    id: i16,
    bytes: Vec<u8>,
}

impl SealKey {
    /// The key_id stored with every seal it signs.
    pub fn id(&self) -> i16 {
        self.id
    }

    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SealKey({}, [redacted])", self.id)
    }
}

/// The keys of `audit.seal_keys_file`: the signing key (highest key_id)
/// and the older ones.
#[derive(Debug)]
pub struct SealKeys {
    newest: SealKey,
    older: Vec<SealKey>,
}

impl SealKeys {
    /// Parse the key file's text. Errors are `FG-SYS-0002
    /// config_key=audit.seal_keys_file` with `reason` and `line` — never
    /// the line's content.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let mut keys: Vec<SealKey> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |reason| invalid(KEY, reason).with_context("line", index + 1);
            let mut parts = line.split_whitespace();
            let (Some(id), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("invalid"));
            };
            let id: i16 = id
                .parse()
                .ok()
                .filter(|id| *id >= 1)
                .ok_or_else(|| bad("invalid_key_id"))?;
            let bytes = decode_hex(hex)
                .filter(|b| (MIN_KEY_BYTES..=MAX_KEY_BYTES).contains(&b.len()))
                .ok_or_else(|| bad("invalid_key"))?;
            if keys.iter().any(|k| k.id == id) {
                return Err(bad("duplicate"));
            }
            keys.push(SealKey { id, bytes });
        }
        keys.sort_by_key(|k| k.id);
        let newest = keys.pop().ok_or_else(|| invalid(KEY, "empty"))?;
        Ok(Self {
            newest,
            older: keys,
        })
    }

    /// The key that signs new seals: the highest key_id.
    pub fn signing(&self) -> &SealKey {
        &self.newest
    }

    /// The key a stored seal names, if the file has it.
    pub fn get(&self, id: i16) -> Option<&SealKey> {
        std::iter::once(&self.newest)
            .chain(&self.older)
            .find(|k| k.id == id)
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: &str, n: usize) -> String {
        byte.repeat(n)
    }

    #[test]
    fn the_highest_key_id_signs_and_older_keys_stay_for_verification() {
        let text = format!(
            "# rotated 2026-10\n\n7 {}\n  3   {}\n",
            key("ab", 32),
            key("0F", 64)
        );
        let keys = SealKeys::parse(&text).expect("keys");
        assert_eq!(keys.signing().id(), 7);
        assert_eq!(keys.signing().expose(), vec![0xab; 32].as_slice());
        assert_eq!(keys.get(3).map(|k| k.expose().len()), Some(64));
        assert!(keys.get(4).is_none());
        assert!(!format!("{keys:?}").contains("ab"), "{keys:?}");
    }

    #[test]
    fn a_bad_key_file_names_the_line_but_never_its_content() {
        let short = format!("1 {}", key("ab", 31));
        let long = format!("1 {}", key("ab", 65));
        let odd = format!("1 {}a", key("ab", 32));
        let not_hex = format!("1 {}zz", key("ab", 32));
        let dup = format!("1 {}\n1 {}", key("ab", 32), key("cd", 32));
        let cases: &[(&str, &str)] = &[
            ("", "empty"),
            ("# only a comment\n", "empty"),
            ("1", "invalid"),
            (&short, "invalid_key"),
            (&long, "invalid_key"),
            (&odd, "invalid_key"),
            (&not_hex, "invalid_key"),
            (&dup, "duplicate"),
        ];
        for (text, reason) in cases {
            let err = SealKeys::parse(text).expect_err(reason);
            let shown = err.to_string();
            assert!(shown.contains(&format!("reason={reason}")), "{shown}");
            assert!(shown.contains("audit.seal_keys_file"), "{shown}");
            assert!(!shown.contains("abab"), "{shown}");
        }
        for id in ["0", "-1", "32768", "x"] {
            let err = SealKeys::parse(&format!("{id} {}", key("ab", 32))).expect_err(id);
            assert!(err.to_string().contains("reason=invalid_key_id"), "{err}");
            assert!(err.to_string().contains("line=1"), "{err}");
        }
    }
}
