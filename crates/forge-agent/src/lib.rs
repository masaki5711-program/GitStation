//! forge agent parts (plan 16 A1, ADR-0032), pure: reads bytes, returns
//! values. No file, network, clock, DB or async runtime — the caller
//! (`gstn agent verify`, later forge-marketplace) reads the files.
//!
//! - [`parse_part`]: a part directory (`part.toml` and its files) → a
//!   [`Part`], with the lint: install paths only under `.claude/` (or
//!   `CLAUDE.md` / `.mcp.json`), regular files only, declared files only,
//!   at most [`MAX_PART_FILES`] files and [`MAX_PART_BYTES`] bytes.
//! - [`digest`]: the part's digest over a canonical encoding (sorted,
//!   length-prefixed), independent of the order files were listed or read.
//! - [`verify_signature`] / [`sign_digest`]: Ed25519 over the digest with a
//!   domain prefix; keys by `kid` from a [`KeyRing`].
//! - [`scan`]: the static scan — undeclared child processes / network /
//!   permission widening, code the scan cannot see through (computed
//!   `require`, `eval`, …), and MCP endpoints outside the allow-list.
//! - [`verify_lock`]: `.gitstation/agent.lock.json` against the working
//!   tree's managed files and the keys.
//!
//! Every finding is a [`Problem`]: a registered `FG-AGENT-*` code, a stable
//! `reason` and an optional subject (a path, a host, a kid).

mod digest;
mod lock;
mod manifest;
mod path;
mod scan;
mod sign;

pub use digest::{digest, Digest};
pub use lock::{verify_lock, Lock, LockFile, LockPart, LockReport, LOCK_PATH};
pub use manifest::{
    parse_part, Capability, EntryKind, Kind, Manifest, Part, PartFile, TreeEntry, MANIFEST,
    MAX_PART_BYTES, MAX_PART_FILES,
};
pub use path::{check_install_path, is_managed_path};
pub use scan::{scan, ScanPolicy};
pub use sign::{
    public_key_from_seed, sign_digest, verify_signature, KeyRing, PublicKey, SIGNATURE_DOMAIN,
};

use forge_errors::ErrorCode;

/// One finding: what is wrong, why (a stable snake_case reason), and about
/// what (a path, a host, a kid — never file content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: ErrorCode,
    pub reason: &'static str,
    pub subject: Option<String>,
}

impl Problem {
    pub(crate) fn new(code: ErrorCode, reason: &'static str, subject: Option<String>) -> Self {
        Self {
            code,
            reason,
            subject,
        }
    }

    pub(crate) fn about(code: ErrorCode, reason: &'static str, subject: &str) -> Self {
        Self::new(code, reason, Some(subject.to_owned()))
    }

    /// How serious the finding is when several are reported together: the
    /// first code of the most serious problem is the one a caller exits
    /// with (tampering before keys before drift before form).
    pub fn rank(&self) -> usize {
        const ORDER: [ErrorCode; 6] = [
            ErrorCode::PartSignatureInvalid,
            ErrorCode::SigningKeyUnavailable,
            ErrorCode::PartCapabilityUndeclared,
            ErrorCode::McpEndpointNotAllowed,
            ErrorCode::AgentLockMismatch,
            ErrorCode::PartManifestInvalid,
        ];
        ORDER
            .iter()
            .position(|c| *c == self.code)
            .unwrap_or(ORDER.len())
    }
}

/// The most serious of `problems` (module docs of [`Problem::rank`]).
pub fn worst(problems: &[Problem]) -> Option<&Problem> {
    problems.iter().min_by_key(|p| p.rank())
}

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    s
}

/// 32 bytes from 64 lower-case hex digits; anything else is `None`.
pub(crate) fn unhex32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = nibble(*bytes.get(2 * i)?)?;
        let lo = nibble(*bytes.get(2 * i + 1)?)?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests;
