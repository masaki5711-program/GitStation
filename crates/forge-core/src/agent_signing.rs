//! The server's agent part signing keys (plan 16 §2.2 and A3; ADR-0032 A3
//! amendment, ADR-0013).
//!
//! `agent.signing_keys_file` is a secret file outside the database, one
//! Ed25519 key per line: `<kid> <64 hex>` — kid 1..=65535 (decimal), the
//! key the 32-byte seed. Blank lines and lines starting with `#` are
//! ignored. The highest kid signs new versions; the older ones stay until
//! the operator removes them (rotation: add a new kid, register its public
//! key, retire the old one). Only the public halves are in the database
//! (`forge.agent_signing_keys`); doctor `agent.signing_keys` compares
//! them.
//!
//! The file is read like every secret file ([`crate::config::read_secret_text`]:
//! absolute, a regular file, at most 64 KiB, and on Unix **readable by its
//! owner only** — a key readable by group or others is refused). Every
//! failure is `FG-AGENT-0003` SigningKeyUnavailable with
//! `config_key=agent.signing_keys_file` and `reason`:
//!
//! - `insecure_permissions` (with `mode`): group or others may read it;
//! - `keys_unreadable` (with `config_reason`: `not_absolute`,
//!   `unreadable`, `invalid` — not a regular file or too large): it cannot
//!   be read;
//! - `keys_invalid` (with `rule` and `line`: `invalid_line`,
//!   `invalid_key_id`, `invalid_key`, `duplicate`, `empty`): it does not
//!   parse;
//! - `signing_key_invalid`: a seed ring refuses.
//!
//! The seed never leaves this module: [`AgentSigningKeys::sign`] signs a
//! digest, and errors and `Debug` name kids only.

use forge_errors::{ErrorCode, ForgeError};
use zeroize::Zeroizing;

/// The configuration key of the file (errors name it).
pub const SIGNING_KEYS_KEY: &str = "agent.signing_keys_file";

const MAX_KEYS_BYTES: u64 = 64 * 1024;
const MAX_KID: u32 = 65_535;

/// `FG-AGENT-0003` with `config_key` and `reason` (module docs).
pub fn key_unavailable(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SigningKeyUnavailable)
        .with_context("config_key", SIGNING_KEYS_KEY)
        .with_context("reason", reason)
}

/// A secret file error (`FG-SYS-0002` from `read_secret_text`) as
/// `FG-AGENT-0003`: `insecure_permissions` stays the reason, any other is
/// `keys_unreadable` with the file reader's reason as `config_reason`.
/// The safe context (`path`, `mode`) is kept; the original is the source.
pub(crate) fn from_secret_error(e: ForgeError) -> ForgeError {
    let original = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or("unknown", |(_, v)| v.as_str());
    let mut out = if original == "insecure_permissions" {
        key_unavailable("insecure_permissions")
    } else {
        key_unavailable("keys_unreadable").with_context("config_reason", original)
    };
    for (key, value) in e.context() {
        if !matches!(*key, "reason" | "config_key") {
            out = out.with_context(key, value);
        }
    }
    out.with_source(e)
}

struct SigningKey {
    kid: u32,
    seed: Zeroizing<[u8; 32]>,
    /// Base64 of the public key (as `forge.agent_signing_keys` stores it).
    public_key: String,
}

/// The keys of `agent.signing_keys_file` (module docs).
pub struct AgentSigningKeys {
    newest: SigningKey,
    older: Vec<SigningKey>,
}

impl std::fmt::Debug for AgentSigningKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AgentSigningKeys(newest {}, {} older)",
            self.newest.kid,
            self.older.len()
        )
    }
}

impl AgentSigningKeys {
    /// Parse the file's text. Errors are `FG-AGENT-0003
    /// reason=keys_invalid` with `rule` and `line` — never the line.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let invalid =
            |rule: &'static str| key_unavailable("keys_invalid").with_context("rule", rule);
        let mut keys: Vec<SigningKey> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bad = |rule| invalid(rule).with_context("line", index + 1);
            let mut parts = line.split_whitespace();
            let (Some(kid), Some(hex), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(bad("invalid_line"));
            };
            let kid: u32 = Some(kid)
                .filter(|k| !k.starts_with('0') && k.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|k| k.parse().ok())
                .filter(|k| (1..=MAX_KID).contains(k))
                .ok_or_else(|| bad("invalid_key_id"))?;
            let bytes = Zeroizing::new(
                crate::config::decode_hex(hex)
                    .filter(|b| b.len() == 32)
                    .ok_or_else(|| bad("invalid_key"))?,
            );
            let mut seed = Zeroizing::new([0u8; 32]);
            seed.copy_from_slice(&bytes);
            if keys.iter().any(|k| k.kid == kid) {
                return Err(bad("duplicate"));
            }
            let public_key = forge_agent::public_key_from_seed(&seed)
                .map_err(|_| key_unavailable("signing_key_invalid").with_context("kid", kid))?;
            keys.push(SigningKey {
                kid,
                seed,
                public_key,
            });
        }
        keys.sort_by_key(|k| k.kid);
        let newest = keys.pop().ok_or_else(|| invalid("empty"))?;
        Ok(Self {
            newest,
            older: keys,
        })
    }

    /// Read and parse the file at `path` (module docs: owner only on Unix).
    pub fn read(path: &std::path::Path) -> Result<Self, ForgeError> {
        let text = Zeroizing::new(
            crate::config::read_secret_text(SIGNING_KEYS_KEY, path, MAX_KEYS_BYTES)
                .map_err(from_secret_error)?,
        );
        Self::parse(&text).map_err(|e| e.with_context("path", path.display()))
    }

    /// The kid that signs new versions (the highest).
    pub fn signing_kid(&self) -> String {
        self.newest.kid.to_string()
    }

    /// Every key's kid and public key (base64), the signing key first.
    pub fn public_keys(&self) -> Vec<(String, &str)> {
        std::iter::once(&self.newest)
            .chain(self.older.iter().rev())
            .map(|k| (k.kid.to_string(), k.public_key.as_str()))
            .collect()
    }

    /// Sign `digest` with the newest key: `(kid, base64 signature)`
    /// (ADR-0032 §4; for the publisher, A5).
    pub fn sign(&self, digest: &forge_agent::Digest) -> Result<(String, String), ForgeError> {
        let signature = forge_agent::sign_digest(&self.newest.seed, digest).map_err(|_| {
            key_unavailable("signing_key_invalid").with_context("kid", self.newest.kid)
        })?;
        Ok((self.signing_kid(), signature))
    }
}

#[cfg(test)]
#[path = "agent_signing_tests.rs"]
mod tests;
