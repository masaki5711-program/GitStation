//! `[agent]`: the agent marketplace's signing keys (plan 16 A3, ADR-0032
//! A3 amendment, ADR-0013).
//!
//! - `agent.signing_keys_file`: the Ed25519 seeds the server signs
//!   published agent part versions with (`<kid> <64 hex>` per line; a
//!   secret file, absolute path, owner only on Unix;
//!   [`crate::agent_signing::AgentKeyFile`]). Optional: without it
//!   nothing can be published (A5). The path is checked at load time.
//!   Here the file is read for its public keys only (doctor
//!   `agent.signing_keys`; a failure is `FG-AGENT-0014`); signing with
//!   the seeds belongs to the marketplace crate (A5).

use std::path::PathBuf;

use forge_errors::ForgeError;

use super::{database, invalid};
use crate::agent_signing::{AgentKeyFile, SIGNING_KEYS_KEY};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// `agent.signing_keys_file`.
    pub signing_keys_file: Option<PathBuf>,
    /// `agent.parts_repository`.
    pub parts_repository: String,
    /// `agent.mcp_allowed_hosts`.
    pub mcp_allowed_hosts: Vec<String>,
}

/// The default parts repository (plan 16 §9.4).
pub const DEFAULT_PARTS_REPOSITORY: &str = "marketplace/parts";

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            signing_keys_file: None,
            parts_repository: DEFAULT_PARTS_REPOSITORY.to_owned(),
            mcp_allowed_hosts: Vec::new(),
        }
    }
}

impl AgentConfig {
    pub(super) fn load<'a>(
        get: impl Fn(&str) -> Option<&'a str> + Copy,
    ) -> Result<Self, ForgeError> {
        let signing_keys_file = match get(SIGNING_KEYS_KEY).map(str::trim) {
            None => None,
            Some("") => return Err(invalid(SIGNING_KEYS_KEY, "empty")),
            Some(p) => Some(database::password_path(SIGNING_KEYS_KEY, p)?),
        };
        let parts_repository = match get("agent.parts_repository").map(str::trim) {
            None => DEFAULT_PARTS_REPOSITORY.to_owned(),
            Some(p) if valid_project_path(p) => p.to_owned(),
            Some(_) => return Err(invalid("agent.parts_repository", "not_a_project_path")),
        };
        let mut mcp_allowed_hosts = Vec::new();
        for host in get("agent.mcp_allowed_hosts")
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            if !valid_host(host) {
                return Err(invalid("agent.mcp_allowed_hosts", "invalid_host"));
            }
            mcp_allowed_hosts.push(host.to_ascii_lowercase());
        }
        Ok(Self {
            signing_keys_file,
            parts_repository,
            mcp_allowed_hosts,
        })
    }

    /// Read `agent.signing_keys_file` now (`None` when not configured);
    /// every failure is `FG-AGENT-0014`. Public keys only: forge-core never
    /// signs (the marketplace crate does, A5).
    pub fn signing_keys(&self) -> Result<Option<AgentKeyFile>, ForgeError> {
        self.signing_keys_file
            .as_deref()
            .map(AgentKeyFile::read)
            .transpose()
    }
}

/// `group/…/project`: 2 to 20 lower-case segments (the namespace rule's
/// characters; the database decides whether it exists).
fn valid_project_path(p: &str) -> bool {
    let segments: Vec<&str> = p.split('/').collect();
    (2..=20).contains(&segments.len())
        && p.len() <= 255
        && segments.iter().all(|s| {
            !s.is_empty()
                && !s.starts_with('.')
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
}

/// A host name or a `.suffix` (letters, digits, `-`, `.`).
fn valid_host(h: &str) -> bool {
    let name = h.strip_prefix('.').unwrap_or(h);
    !name.is_empty()
        && h.len() <= 253
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<AgentConfig, ForgeError> {
        AgentConfig::load(|k| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v))
    }

    #[test]
    fn the_parts_repository_and_mcp_hosts_have_safe_defaults() {
        let c = load(&[]).expect("defaults");
        assert_eq!(c.parts_repository, "marketplace/parts");
        assert!(c.mcp_allowed_hosts.is_empty());
        let c = load(&[
            ("agent.parts_repository", "tools/agent-parts"),
            ("agent.mcp_allowed_hosts", "MCP.internal, .corp.internal"),
        ])
        .expect("set");
        assert_eq!(c.parts_repository, "tools/agent-parts");
        assert_eq!(c.mcp_allowed_hosts, ["mcp.internal", ".corp.internal"]);
        assert!(load(&[("agent.parts_repository", "parts")]).is_err());
        assert!(load(&[("agent.parts_repository", "a/../b")]).is_err());
        assert!(load(&[("agent.mcp_allowed_hosts", "https://x")]).is_err());
    }
}
