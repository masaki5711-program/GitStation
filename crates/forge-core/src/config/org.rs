//! `[org]`: the copy of the PFS organisation master and how old the org
//! facts may be before everything derived from them fails closed
//! (ADR-0038 §1, §4).
//!
//! | key | default | rule |
//! |---|---|---|
//! | `org.master_sync_enabled` | `false` | the worker copies `GET {portal.registry_url}/api/v1/org/units` and `/org/positions`; needs `portal.registry_url` (and `portal.ca_file` for https) and `auth.oidc` (its client's client-credentials token) |
//! | `org.master_sync_interval_secs` | 900 | between two successful syncs; 60 ..= 86 400 |
//! | `org.master_timeout_ms` | 5 000 | one GET, connect and answer; 500 ..= 30 000 |
//! | `org.master_max_age_hours` | 24 | a copy older than this is stale: derived org facts are `unknown` (fail closed), doctor raises `FG-INT-0028`; 1 ..= 720 |
//! | `org.allow_large_shrink` | `false` | let one sync remove more than half of the units, empty a plant or leave no positions (otherwise refused, `FG-INT-0027 reason=shrink`); an operator's deliberate, temporary opt-in |
//! | `org.max_profile_age_hours` | 24 | a person's org-claims snapshot (`user_org_profiles.confirmed_at`) older than this is not used for derived org facts; 1 ..= 720 |
//!
//! The registry's origin and CA are the portal's (`portal.registry_url`,
//! `portal.ca_file`): one PFS registry-api, one trust root. The token is the
//! sign-in client's (`auth.oidc.client_id` / `client_secret_file`), the same
//! as the portal news. Every problem is `FG-SYS-0002` with `config_key` and
//! `reason`.

use std::time::Duration;

use forge_errors::ForgeError;

use super::{invalid, unsigned};

pub(super) const ENABLED_KEY: &str = "org.master_sync_enabled";
const INTERVAL_KEY: &str = "org.master_sync_interval_secs";
const TIMEOUT_KEY: &str = "org.master_timeout_ms";
const MASTER_AGE_KEY: &str = "org.master_max_age_hours";
const PROFILE_AGE_KEY: &str = "org.max_profile_age_hours";
const SHRINK_KEY: &str = "org.allow_large_shrink";

/// `[org]` as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgConfig {
    pub master_sync_enabled: bool,
    pub master_sync_interval: Duration,
    pub master_timeout: Duration,
    pub master_max_age: Duration,
    pub max_profile_age: Duration,
    pub allow_large_shrink: bool,
}

impl Default for OrgConfig {
    fn default() -> Self {
        Self {
            master_sync_enabled: false,
            master_sync_interval: Duration::from_secs(900),
            master_timeout: Duration::from_millis(5_000),
            master_max_age: Duration::from_secs(24 * 3600),
            max_profile_age: Duration::from_secs(24 * 3600),
            allow_large_shrink: false,
        }
    }
}

fn ranged<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    key: &'static str,
    default: u32,
    (min, max): (u32, u32),
) -> Result<u64, ForgeError> {
    let n = unsigned(get, key, default)?;
    if n < min {
        return Err(invalid(key, "below_min"));
    }
    if n > max {
        return Err(invalid(key, "above_max"));
    }
    Ok(u64::from(n))
}

impl OrgConfig {
    pub(super) fn load<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Result<Self, ForgeError> {
        let flag = |key: &'static str| match get(key).map(str::trim) {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some("") => Err(invalid(key, "empty")),
            Some(_) => Err(invalid(key, "invalid")),
        };
        let master_sync_enabled = flag(ENABLED_KEY)?;
        let allow_large_shrink = flag(SHRINK_KEY)?;
        let hours = |key, default| ranged(&get, key, default, (1, 720)).map(|h| h * 3600);
        Ok(Self {
            master_sync_enabled,
            allow_large_shrink,
            master_sync_interval: Duration::from_secs(ranged(
                &get,
                INTERVAL_KEY,
                900,
                (60, 86_400),
            )?),
            master_timeout: Duration::from_millis(ranged(&get, TIMEOUT_KEY, 5_000, (500, 30_000))?),
            master_max_age: Duration::from_secs(hours(MASTER_AGE_KEY, 24)?),
            max_profile_age: Duration::from_secs(hours(PROFILE_AGE_KEY, 24)?),
        })
    }

    /// The rules across sections: the sync reads the portal's registry, so
    /// it needs its origin, and an https origin needs its CA (only the
    /// operator's roots are trusted). `auth.oidc` (the token) is checked
    /// where the worker builds the task.
    pub(super) fn check_registry(
        &self,
        registry_url: Option<&str>,
        has_ca: bool,
    ) -> Result<(), ForgeError> {
        if !self.master_sync_enabled {
            return Ok(());
        }
        let Some(url) = registry_url else {
            return Err(invalid(ENABLED_KEY, "needs_portal_registry_url"));
        };
        if url.starts_with("https://") && !has_ca {
            return Err(invalid("portal.ca_file", "missing"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use forge_errors::ErrorCode;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<OrgConfig, ForgeError> {
        let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
        OrgConfig::load(|k| map.get(k).copied())
    }

    fn reason(pairs: &[(&str, &str)]) -> String {
        let err = load(pairs).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        err.to_string()
    }

    #[test]
    fn the_org_master_sync_is_off_by_default() {
        let cfg = load(&[]).expect("defaults");
        assert_eq!(cfg, OrgConfig::default());
        assert!(!cfg.master_sync_enabled);
        assert_eq!(cfg.master_sync_interval, Duration::from_secs(900));
        assert_eq!(cfg.master_timeout, Duration::from_secs(5));
        assert_eq!(cfg.master_max_age, Duration::from_secs(86_400));
        assert_eq!(cfg.max_profile_age, Duration::from_secs(86_400));
        // Off: nothing else is needed.
        cfg.check_registry(None, false).expect("off");
    }

    #[test]
    fn the_org_keys_are_read_and_bounded() {
        let cfg = load(&[
            (ENABLED_KEY, "true"),
            (INTERVAL_KEY, "60"),
            (TIMEOUT_KEY, "30000"),
            (MASTER_AGE_KEY, "2"),
            (PROFILE_AGE_KEY, "720"),
            (SHRINK_KEY, "true"),
        ])
        .expect("set");
        assert!(cfg.allow_large_shrink);
        assert!(reason(&[(SHRINK_KEY, "1")]).contains("org.allow_large_shrink"));
        assert!(cfg.master_sync_enabled);
        assert_eq!(cfg.master_sync_interval, Duration::from_secs(60));
        assert_eq!(cfg.master_timeout, Duration::from_secs(30));
        assert_eq!(cfg.master_max_age, Duration::from_secs(7200));
        assert_eq!(cfg.max_profile_age, Duration::from_secs(720 * 3600));
        assert!(reason(&[(ENABLED_KEY, "yes")]).contains("reason=invalid"));
        assert!(reason(&[(ENABLED_KEY, " ")]).contains("reason=empty"));
        assert!(reason(&[(INTERVAL_KEY, "59")]).contains("below_min"));
        assert!(reason(&[(INTERVAL_KEY, "86401")]).contains("above_max"));
        assert!(reason(&[(TIMEOUT_KEY, "499")]).contains("below_min"));
        assert!(reason(&[(MASTER_AGE_KEY, "0")]).contains("below_min"));
        assert!(reason(&[(PROFILE_AGE_KEY, "721")]).contains("above_max"));
        assert!(reason(&[(PROFILE_AGE_KEY, "x")]).contains("reason=invalid"));
    }

    #[test]
    fn the_sync_needs_the_portal_registry_and_its_ca() {
        let on = load(&[(ENABLED_KEY, "true")]).expect("on");
        let missing = on.check_registry(None, false).expect_err("no registry");
        assert!(missing.to_string().contains("needs_portal_registry_url"));
        let no_ca = on
            .check_registry(Some("https://registry.example.test"), false)
            .expect_err("no ca");
        assert!(no_ca.to_string().contains("portal.ca_file"));
        on.check_registry(Some("https://registry.example.test"), true)
            .expect("https with ca");
        on.check_registry(Some("http://127.0.0.1:4100"), false)
            .expect("loopback http");
    }
}
