//! The agent marketplace's tables (plan 16 §3, A3; migration
//! 20270103100000, ADR-0032 A3 amendment).
//!
//! A3 ships the schema and its guards; the commands that write it come
//! with A5 (publish, promote, revoke, `forge admin agent-key`), A7 (the
//! builder's records) and A10 (lesson injections). What the database
//! itself refuses, whoever writes:
//!
//! - `agent_signing_keys`: the kid and public key never change;
//!   retirement and revocation are set once; never deleted.
//! - `agent_parts`: the identity never changes; deprecation is set once;
//!   never deleted.
//! - `agent_part_versions`: inserted as `beta` / `published`, signed by a
//!   key that is neither retired nor revoked, of a part that is not
//!   deprecated; afterwards only beta -> stable (while published) and
//!   published -> revoked; a revoked version is final; never deleted.
//! - `agent_part_files`: append-only, inserted only in the transaction
//!   that inserted the version, `sha256` = the content's, at most 200
//!   files and 1 MiB per version.
//!
//! Every violation is SQLSTATE `XF001`, `FG-DB-0009` AppendOnlyViolation
//! (alert) through [`crate::query_error`]. forge_app holds no DELETE on these
//! four tables and UPDATE only on the columns that may change.
//!
//! Reads here: [`signing_keys`] (doctor `agent.signing_keys`).

use forge_errors::ForgeError;

use crate::{read_query_error, Db};

/// The audit resource of a signing key's change (tables.toml).
pub const KEY_RESOURCE: &str = "agent_signing_key";
/// The audit resource of a part's or a version's change (tables.toml).
pub const PART_RESOURCE: &str = "agent_part";

/// A registered signing key: the public half only (the seed is in
/// `agent.signing_keys_file`, never in the database).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredSigningKey {
    /// The key file's key id (decimal text, 1..=65535).
    pub kid: String,
    /// Base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
    /// Retired: signs nothing new, still verifies what it signed.
    pub retired: bool,
    /// Revoked: its signatures are refused.
    pub revoked: bool,
}

/// Every registered signing key, by kid (the read pool).
pub async fn signing_keys(db: &Db) -> Result<Vec<StoredSigningKey>, ForgeError> {
    const QUERY: &str = "agent_marketplace.signing_keys";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT kid, public_key, retired_at IS NOT NULL AS retired, \
             revoked_at IS NOT NULL AS revoked \
             FROM forge.agent_signing_keys ORDER BY kid::integer",
        )
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

#[cfg(test)]
#[path = "agent_marketplace_tests.rs"]
mod tests;
