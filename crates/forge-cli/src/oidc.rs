//! The OIDC sign-in's wiring for `forge serve` and `forge doctor` (M2 MR4,
//! ADR-0021 §3, §11): `auth.oidc.*` together with `web.*`.
//!
//! When `auth.oidc` is configured, `web.session_key_file` is required (the
//! login state cookie and the sealed IdP tokens use its keys): without it
//! `serve` does not start, `FG-SYS-0002 config_key=web.session_key_file
//! reason=missing` (runbook config-invalid). Without `auth.oidc` the login
//! routes answer `FG-AUTH-0018` (not configured) and `forge doctor` warns.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_core::config::Config;
use forge_core::web_keys::WebKeys;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_oidc::{Oidc, OidcSettings};

/// The clock skew `forge doctor` warns about: the ID token's `iat` may be
/// at most 60 s in the future (ADR-0021 §3), so a larger skew refuses
/// sign-ins.
pub(crate) const MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);

/// The settings and the session keys, or `None` when `auth.oidc` is not
/// configured.
pub(crate) fn settings(cfg: &Config) -> Result<Option<(OidcSettings, WebKeys)>, ForgeError> {
    let Some(oidc) = cfg.auth.oidc.as_ref() else {
        return Ok(None);
    };
    let keys = cfg.web.session_keys()?.ok_or_else(|| {
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", "web.session_key_file")
            .with_context("reason", "missing")
            .with_context("required_by", "auth.oidc")
    })?;
    let origin = cfg.web.require_base_url()?.origin().to_owned();
    let settings = OidcSettings::from_config(
        oidc,
        &origin,
        cfg.web.session_idle,
        cfg.web.session_absolute,
    )?;
    Ok(Some((settings, keys)))
}

/// The sign-in service of `forge serve`, or `None` without `auth.oidc`.
pub(crate) fn service(cfg: &Config, db: &Db) -> Result<Option<Arc<Oidc>>, ForgeError> {
    let Some((settings, keys)) = settings(cfg)? else {
        return Ok(None);
    };
    // clone: the pool handle is shared
    Ok(Some(Arc::new(Oidc::new(settings, &keys, db.clone())?)))
}

/// How far the provider's clock (its HTTP `Date`) is from ours, or `None`
/// when it sent no readable `Date`.
pub(crate) fn clock_skew(date: Option<&str>, now: SystemTime) -> Option<Duration> {
    let theirs = httpdate::parse_http_date(date?).ok()?;
    Some(match now.duration_since(theirs) {
        Ok(behind) => behind,
        Err(ahead) => ahead.duration(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use forge_core::config::{load, Env};

    use super::*;

    #[test]
    fn without_auth_oidc_there_is_nothing_to_wire() {
        let cfg = load(None, &Env::from_pairs([])).expect("config");
        assert!(settings(&cfg).expect("no oidc").is_none());
    }

    #[test]
    fn auth_oidc_requires_the_session_key_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = dir.path().join("client-secret");
        std::fs::write(&secret, "synthetic-secret").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
                .expect("chmod");
        }
        let cfg = load(
            None,
            &Env::from_pairs([
                ("FORGE_WEB_BASE_URL", "http://127.0.0.1:8080"),
                ("FORGE_AUTH_OIDC_ISSUER", "http://127.0.0.1:58080/realms/r"),
                ("FORGE_AUTH_OIDC_CLIENT_ID", "gitstation"),
                (
                    "FORGE_AUTH_OIDC_CLIENT_SECRET_FILE",
                    secret.to_str().expect("utf-8"),
                ),
                ("FORGE_AUTH_OIDC_EMPLOYEE_NUMBER_CLAIM", "employee_number"),
                ("FORGE_AUTH_OIDC_ACCESS_ROLE", "access"),
            ]),
        );
        let cfg = cfg.expect("config");
        let err = settings(&cfg).expect_err("no session keys");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        let text = err.to_string();
        assert!(text.contains("config_key=web.session_key_file"), "{text}");
        assert!(text.contains("reason=missing"), "{text}");
    }

    #[test]
    fn the_clock_skew_is_measured_both_ways() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let date = httpdate::fmt_http_date(now - Duration::from_secs(90));
        assert_eq!(clock_skew(Some(&date), now), Some(Duration::from_secs(90)));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(5));
        assert_eq!(clock_skew(Some(&date), now), Some(Duration::from_secs(5)));
        assert_eq!(clock_skew(Some("yesterday"), now), None);
        assert_eq!(clock_skew(None, now), None);
    }
}
