//! The CI secrets check of `forge doctor` (M4 C6, plan 14 §6, runbook
//! ci-secrets):
//!
//! - `ci.secret_keys` — `ci.secret_keys_file` read and parsed (absolute,
//!   owner only on Unix, `<key_id> <64 hex>` lines), and every key a
//!   stored CI variable was sealed with still listed. `FG-CI-0016` with
//!   `reason` `keys_unreadable` (with `config_reason`), `not_configured`
//!   (variables exist but no file: a job that asks for them fails) or
//!   `key_missing` (with `key_id` and `variables`); a skip when nothing is
//!   configured and nothing is stored. Never a key or a value.

use forge_core::ci_secret::{key_unavailable, CiSecretKeys};
use forge_core::config::Config;
use forge_db::Db;
use forge_errors::ForgeError;

use super::{fail, ok, skip, Check};

pub(super) const NAME: &str = "ci.secret_keys";

/// The check's outcome for the loaded keys and the stored key use (pure:
/// the tests feed both).
fn line(keys: Result<Option<CiSecretKeys>, ForgeError>, used: &[(i16, i64)]) -> Check {
    let stored: i64 = used.iter().map(|(_, n)| n).sum();
    let keys = match keys {
        Ok(keys) => keys,
        Err(e) => {
            let config_reason = e
                .context()
                .iter()
                .find(|(k, _)| *k == "reason")
                .map_or_else(|| "unknown".to_owned(), |(_, v)| v.clone()); // clone: the new error owns it
            return fail(
                NAME,
                key_unavailable("keys_unreadable")
                    .with_context("config_key", forge_core::ci_secret::SECRET_KEYS_KEY)
                    .with_context("config_reason", config_reason)
                    .with_context("runbook", "ci-secrets"),
            );
        }
    };
    let Some(keys) = keys else {
        if stored > 0 {
            return fail(
                NAME,
                key_unavailable("not_configured")
                    .with_context("variables", stored)
                    .with_context("runbook", "ci-secrets"),
            );
        }
        return skip(
            NAME,
            "ci.secret_keys_file is not configured and no CI variable is stored (no variable can be set)",
        );
    };
    if let Some((key_id, n)) = used.iter().find(|(id, _)| !keys.has_key(*id)) {
        return fail(
            NAME,
            key_unavailable("key_missing")
                .with_context("key_id", key_id)
                .with_context("variables", n)
                .with_context("runbook", "ci-secrets"),
        );
    }
    ok(
        NAME,
        &format!(
            "loaded; new values are sealed with key_id {}; {stored} stored value(s), every key listed",
            keys.sealing_key_id()
        ),
    )
}

/// The check.
pub(super) async fn secret_keys_check(db: &Db, cfg: &Config) -> Check {
    match forge_db::ci_variables::key_use(db).await {
        Ok(used) => line(cfg.ci.secret_keys(), &used),
        Err(e) => fail(NAME, e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use forge_errors::ErrorCode;

    use super::super::Outcome;
    use super::*;

    const KEY: &str = "1 0000000000000000000000000000000000000000000000000000000000000001";

    fn reason(c: &Check) -> String {
        let Outcome::Fail(e) = &c.outcome else {
            panic!("expected a failure");
        };
        assert_eq!(e.code(), ErrorCode::SecretKeyUnavailable);
        e.context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .expect("reason")
    }

    #[test]
    fn missing_or_unreadable_keys_fail_when_values_need_them() {
        assert!(matches!(line(Ok(None), &[]).outcome, Outcome::Skip(_)));
        assert_eq!(reason(&line(Ok(None), &[(1, 2)])), "not_configured");
        let parsed = CiSecretKeys::parse(KEY).expect("keys");
        assert!(matches!(
            line(Ok(Some(parsed)), &[(1, 2)]).outcome,
            Outcome::Ok(_)
        ));
        let parsed = CiSecretKeys::parse(KEY).expect("keys");
        assert_eq!(reason(&line(Ok(Some(parsed)), &[(2, 1)])), "key_missing");
        let bad = CiSecretKeys::parse("1 zz").map(Some);
        assert_eq!(reason(&line(bad, &[])), "keys_unreadable");
    }
}
