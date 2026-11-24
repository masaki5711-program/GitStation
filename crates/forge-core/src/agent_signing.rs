//! The server's agent part signing key file, read for its PUBLIC keys
//! only (plan 16 §2.2 and A3; ADR-0032 A3 amendment, ADR-0013).
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
//! **forge-core never signs.** Signing belongs to the marketplace feature
//! crate (`forge-marketplace`, A5: CLAUDE.md contract 9, plan 16 §2.3), so
//! no crate that merely holds the configuration can sign a part around the
//! publish decision. This module derives each seed's public key and drops
//! the seed at once (zeroized); [`AgentKeyFile`] holds kids and public keys
//! only.
//!
//! The file is read like every secret file ([`crate::config::read_secret_text`]:
//! absolute, a regular file, at most 64 KiB, and on Unix **readable by its
//! owner only**; on Linux the owner must also be this process's user).
//! Every failure is `FG-AGENT-0014` SigningKeyFileInvalid with
//! `config_key=agent.signing_keys_file` and `reason`:
//!
//! - `insecure_permissions` (with `mode`, or `rule=owner`): group or
//!   others may read it, or another user owns it;
//! - `keys_unreadable` (with `config_reason`: `not_absolute`,
//!   `unreadable`, `invalid` — not a regular file or too large): it cannot
//!   be read;
//! - `keys_invalid` (with `rule` and `line`: `invalid_line`,
//!   `invalid_key_id`, `invalid_key`, `duplicate`, `empty`): it does not
//!   parse;
//! - `signing_key_invalid`: a seed ring refuses.
//!
//! Errors and `Debug` name kids only, never a seed.

use std::path::Path;

use forge_errors::{ErrorCode, ForgeError};
use zeroize::Zeroizing;

/// The configuration key of the file (errors name it).
pub const SIGNING_KEYS_KEY: &str = "agent.signing_keys_file";

const MAX_KEYS_BYTES: u64 = 64 * 1024;
const MAX_KID: u32 = 65_535;

/// Whether reading the file checks its permissions on this OS (Unix:
/// owner only; elsewhere the ACL is the operator's, ADR-0013 — doctor
/// warns).
pub const PERMISSIONS_CHECKED: bool = cfg!(unix);

/// `FG-AGENT-0014` with `config_key` and `reason` (module docs).
pub fn key_file_invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::SigningKeyFileInvalid)
        .with_context("config_key", SIGNING_KEYS_KEY)
        .with_context("reason", reason)
}

/// A secret file error (`FG-SYS-0002` from `read_secret_text`) as
/// `FG-AGENT-0014`: `insecure_permissions` stays the reason, any other is
/// `keys_unreadable` with the file reader's reason as `config_reason`.
/// The safe context (`path`, `mode`) is kept; the original is the source.
pub(crate) fn from_secret_error(e: ForgeError) -> ForgeError {
    let original = e
        .context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or("unknown", |(_, v)| v.as_str());
    let mut out = if original == "insecure_permissions" {
        key_file_invalid("insecure_permissions")
    } else {
        key_file_invalid("keys_unreadable").with_context("config_reason", original)
    };
    for (key, value) in e.context() {
        if !matches!(*key, "reason" | "config_key") {
            out = out.with_context(key, value);
        }
    }
    out.with_source(e)
}

/// One key of the file: its kid and public key (the seed is gone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPublicKey {
    pub kid: u32,
    /// Base64 of the public key (as `forge.agent_signing_keys` stores it).
    pub public_key: String,
}

/// The public keys of `agent.signing_keys_file` (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKeyFile {
    /// Sorted by kid, highest (the signing key) last.
    keys: Vec<AgentPublicKey>,
}

impl AgentKeyFile {
    /// Parse the file's text. Errors are `FG-AGENT-0014
    /// reason=keys_invalid` with `rule` and `line` — never the line.
    pub fn parse(text: &str) -> Result<Self, ForgeError> {
        let invalid =
            |rule: &'static str| key_file_invalid("keys_invalid").with_context("rule", rule);
        let mut keys: Vec<AgentPublicKey> = Vec::new();
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
                .map_err(|_| key_file_invalid("signing_key_invalid").with_context("kid", kid))?;
            keys.push(AgentPublicKey { kid, public_key });
        }
        if keys.is_empty() {
            return Err(invalid("empty"));
        }
        keys.sort_by_key(|k| k.kid);
        Ok(Self { keys })
    }

    /// Read and parse the file at `path` (module docs: owner only on Unix).
    pub fn read(path: &Path) -> Result<Self, ForgeError> {
        let text = Zeroizing::new(
            crate::config::read_secret_text(SIGNING_KEYS_KEY, path, MAX_KEYS_BYTES)
                .map_err(from_secret_error)?,
        );
        check_owner(path)?;
        Self::parse(&text).map_err(|e| e.with_context("path", path.display()))
    }

    /// The kid that signs new versions (the highest).
    pub fn signing_kid(&self) -> u32 {
        self.keys.last().map_or(0, |k| k.kid)
    }

    /// Every key, the signing key first.
    pub fn keys(&self) -> impl Iterator<Item = &AgentPublicKey> {
        self.keys.iter().rev()
    }
}

/// Linux: the file belongs to the user forge runs as (`/proc/self` is
/// owned by the process's effective user). Another owner could replace
/// the key; refused as `insecure_permissions` with `rule=owner`.
#[cfg(target_os = "linux")]
fn check_owner(path: &Path) -> Result<(), ForgeError> {
    use std::os::unix::fs::MetadataExt as _;
    let unreadable = |e: std::io::Error| {
        key_file_invalid("keys_unreadable")
            .with_context("config_reason", "unreadable")
            .with_context("path", path.display())
            .with_source(e)
    };
    let file = std::fs::metadata(path).map_err(unreadable)?;
    let me = std::fs::metadata("/proc/self").map_err(unreadable)?;
    if file.uid() != me.uid() {
        return Err(key_file_invalid("insecure_permissions")
            .with_context("rule", "owner")
            .with_context("path", path.display())
            .with_context("owner_uid", file.uid()));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_owner(_path: &Path) -> Result<(), ForgeError> {
    Ok(())
}

#[cfg(test)]
#[path = "agent_signing_tests.rs"]
mod tests;
