//! The act-as check of `forge doctor` (M3 S2, ADR-0025 §5.2, runbook
//! act-as): `act_as.keys`.
//!
//! - the key file (`auth.act_as_hmac_keys_file`) is read and parsed as
//!   `forge serve` does: unreadable, malformed, or not exactly one
//!   `active` key is `FG-SYS-0002` (alert);
//! - no key file while an active token holds the `act_as` scope is
//!   `FG-SYS-0002 reason=act_as_keys_missing` (kanflow's approvals stop
//!   with `act_as_not_configured`);
//! - otherwise ok, with the counts of active, still verifying and retired
//!   keys (or that acting for a person is off).

use forge_core::config::Config;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

use super::{fail, ok, Check};

pub(super) const NAME: &str = "act_as.keys";

pub(super) async fn keys_check(db: &Db, cfg: &Config) -> Check {
    let keys = match cfg.auth.act_as_keys() {
        Ok(keys) => keys,
        Err(e) => return fail(NAME, e),
    };
    if let Some(keys) = keys {
        let c = keys.counts();
        return ok(
            NAME,
            &format!(
                "{} active, {} verifying, {} retired",
                c.active, c.valid, c.retired
            ),
        );
    }
    match forge_db::act_as::active_act_as_tokens(db).await {
        Ok(0) => ok(NAME, "not configured (acting for a person is off)"),
        Ok(n) => fail(
            NAME,
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", forge_core::act_as::KEYS_CONFIG_KEY)
                .with_context("reason", "act_as_keys_missing")
                .with_context("active_act_as_tokens", n),
        ),
        Err(e) => fail(NAME, e),
    }
}
