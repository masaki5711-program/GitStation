//! The agent marketplace's signing keys in `forge doctor` (plan 16 A3 and
//! §8, runbook agent-signing-key):
//!
//! - `agent.signing_keys` — `agent.signing_keys_file` read and parsed
//!   (absolute, owner only on Unix, `<kid> <64 hex>` lines), and every key
//!   in it registered in `forge.agent_signing_keys` with the same public
//!   key and not revoked; the signing key (the highest kid) not retired.
//!   `FG-AGENT-0003` with the reader's `reason` (`insecure_permissions`,
//!   `keys_unreadable`, `keys_invalid`, `signing_key_invalid`) or, against
//!   the database, `key_not_registered`, `key_mismatch` (the file's key
//!   is not the registered public key: a wrong or forged file),
//!   `key_revoked` (a revoked key is still in the file) or
//!   `signing_key_retired`, each with `kid`. A skip when the file is not
//!   configured (this host signs nothing). Never a seed.

use forge_core::agent_signing::{key_unavailable, AgentSigningKeys};
use forge_core::config::Config;
use forge_db::agent_marketplace::StoredSigningKey;
use forge_db::Db;
use forge_errors::ForgeError;

use super::{fail, ok, skip, Check};

pub(super) const NAME: &str = "agent.signing_keys";

/// The check's outcome for the loaded keys and the registered ones (pure:
/// the tests feed both).
fn line(keys: Result<Option<AgentSigningKeys>, ForgeError>, stored: &[StoredSigningKey]) -> Check {
    let keys =
        match keys {
            Ok(Some(keys)) => keys,
            Ok(None) => return skip(
                NAME,
                "agent.signing_keys_file is not configured (this host cannot publish agent parts)",
            ),
            Err(e) => return fail(NAME, e.with_context("runbook", "agent-signing-key")),
        };
    let signing = keys.signing_kid();
    let problem = |reason: &'static str, kid: &str| {
        fail(
            NAME,
            key_unavailable(reason)
                .with_context("kid", kid)
                .with_context("runbook", "agent-signing-key"),
        )
    };
    for (kid, public_key) in keys.public_keys() {
        let Some(row) = stored.iter().find(|s| s.kid == kid) else {
            return problem("key_not_registered", &kid);
        };
        if row.public_key != public_key {
            return problem("key_mismatch", &kid);
        }
        if row.revoked {
            return problem("key_revoked", &kid);
        }
        if kid == signing && row.retired {
            return problem("signing_key_retired", &kid);
        }
    }
    ok(
        NAME,
        &format!(
            "loaded; new versions are signed with kid {signing}; {} key(s) in the file, every one registered",
            keys.public_keys().len()
        ),
    )
}

/// The check.
pub(super) async fn signing_keys_check(db: &Db, cfg: &Config) -> Check {
    match forge_db::agent_marketplace::signing_keys(db).await {
        Ok(stored) => line(cfg.agent.signing_keys(), &stored),
        Err(e) => fail(NAME, e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use forge_errors::ErrorCode;

    use super::super::Outcome;
    use super::*;

    const SEED_1: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const SEED_2: &str = "0202020202020202020202020202020202020202020202020202020202020202";

    fn keys(text: &str) -> Result<Option<AgentSigningKeys>, ForgeError> {
        AgentSigningKeys::parse(text).map(Some)
    }

    fn stored(text: &str) -> Vec<StoredSigningKey> {
        AgentSigningKeys::parse(text)
            .expect("keys")
            .public_keys()
            .into_iter()
            .map(|(kid, public_key)| StoredSigningKey {
                kid,
                public_key: public_key.to_owned(),
                retired: false,
                revoked: false,
            })
            .collect()
    }

    fn reason(c: &Check) -> String {
        let Outcome::Fail(e) = &c.outcome else {
            panic!("expected a failure");
        };
        assert_eq!(e.code(), ErrorCode::SigningKeyUnavailable);
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .expect("reason")
    }

    #[test]
    fn the_key_file_must_match_the_registered_keys() {
        let both = format!("1 {SEED_1}\n2 {SEED_2}\n");
        assert!(matches!(line(Ok(None), &[]).outcome, Outcome::Skip(_)));
        assert!(matches!(
            line(keys(&both), &stored(&both)).outcome,
            Outcome::Ok(_)
        ));
        // Only the older key registered: the signing key is not.
        let only_one = stored(&format!("1 {SEED_1}"));
        assert_eq!(reason(&line(keys(&both), &only_one)), "key_not_registered");
        // The same kid with another public key.
        let other = stored(&format!("1 {SEED_2}"));
        assert_eq!(
            reason(&line(keys(&format!("1 {SEED_1}")), &other)),
            "key_mismatch"
        );
        // A revoked key still in the file; a retired signing key.
        let mut revoked = stored(&both);
        revoked[1].revoked = true;
        assert_eq!(reason(&line(keys(&both), &revoked)), "key_revoked");
        let mut retired = stored(&both);
        retired[0].retired = true;
        assert_eq!(reason(&line(keys(&both), &retired)), "signing_key_retired");
        // A retired older key is fine (it only verifies).
        let mut older_retired = stored(&both);
        older_retired[1].retired = true;
        assert!(matches!(
            line(keys(&both), &older_retired).outcome,
            Outcome::Ok(_)
        ));
        // The reader's refusal is the check's failure.
        assert_eq!(reason(&line(keys(""), &[])), "keys_invalid");
    }
}
