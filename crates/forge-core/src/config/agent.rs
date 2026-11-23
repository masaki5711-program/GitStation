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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentConfig {
    /// `agent.signing_keys_file`.
    pub signing_keys_file: Option<PathBuf>,
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
        Ok(Self { signing_keys_file })
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
