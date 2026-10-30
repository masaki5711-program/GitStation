//! Event idempotency keys (ADR-0016 §6, closed in its D3 amendment).
//! `forge.events.dedup_key` is unique instance-wide, so a key built from a
//! client value would let one caller pre-empt another's change. A
//! [`DedupKey`] is therefore built only from server-side values: fixed
//! words (`&'static str`), ids and 32-byte hashes. Its shape is
//! `^[a-z][a-z_]*(:[0-9a-z_-]{1,64}){1,4}$`, at most 200 characters; a
//! violation is `FG-DB-0010 reason=invalid_dedup_key`.
//!
//! A client string cannot be a part:
//!
//! ```compile_fail
//! use forge_core::dedup_key::{DedupKey, DedupPart};
//! let from_client = String::from("x");
//! let _ = DedupKey::new("push", &[DedupPart::Word(&from_client)]);
//! ```

use forge_errors::{ErrorCode, ForgeError};

/// The longest key, in characters (the `forge.events` CHECK).
pub const MAX_CHARS: usize = 200;
/// The most parts after the prefix.
const MAX_PARTS: usize = 4;
/// The longest part, in characters.
const MAX_PART_CHARS: usize = 64;

/// One part of a key after its prefix.
#[derive(Debug, Clone, Copy)]
pub enum DedupPart<'a> {
    /// A database id (decimal; a negative one keeps the shape).
    Id(i64),
    /// A SHA-256 digest computed by forge (lowercase hex, 64 characters).
    Hash(&'a [u8; 32]),
    /// A fixed word from the source code.
    Word(&'static str),
}

/// A well-formed idempotency key. No constructor takes a runtime string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DedupKey(String);

fn invalid() -> ForgeError {
    ForgeError::new(ErrorCode::ChangeRejected).with_context("reason", "invalid_dedup_key")
}

fn is_prefix(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

fn is_part(s: &str) -> bool {
    (1..=MAX_PART_CHARS).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '_' || c == '-')
}

impl DedupKey {
    /// `<prefix>:<part>[:<part>…]` with one to four parts.
    /// `FG-DB-0010 reason=invalid_dedup_key` (with `prefix`) when the
    /// prefix or a word breaks the shape, the part count is outside 1..=4,
    /// or the key is longer than [`MAX_CHARS`].
    pub fn new(prefix: &'static str, parts: &[DedupPart<'_>]) -> Result<DedupKey, ForgeError> {
        let mut key = String::from(prefix);
        for part in parts {
            key.push(':');
            match part {
                DedupPart::Id(id) => key.push_str(&id.to_string()),
                DedupPart::Hash(bytes) => {
                    for b in bytes.iter() {
                        key.push_str(&format!("{b:02x}"));
                    }
                }
                DedupPart::Word(word) => key.push_str(word),
            }
        }
        if Self::is_well_formed(&key) {
            Ok(DedupKey(key))
        } else {
            Err(invalid().with_context("prefix", prefix))
        }
    }

    /// Whether `key` has the documented shape and length (what the writers
    /// check again before inserting).
    pub fn is_well_formed(key: &str) -> bool {
        if key.len() > MAX_CHARS {
            return false;
        }
        let mut pieces = key.split(':');
        let prefix_ok = pieces.next().is_some_and(is_prefix);
        let parts: Vec<&str> = pieces.collect();
        prefix_ok && (1..=MAX_PARTS).contains(&parts.len()) && parts.iter().all(|p| is_part(p))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A key that skips every check, so tests can prove the writers check
    /// again. Never available in a production build.
    #[cfg(any(test, feature = "test-support"))]
    pub fn unchecked(raw: &str) -> DedupKey {
        DedupKey(raw.to_owned())
    }
}
