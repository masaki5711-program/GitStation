//! `auth.oidc` (M2 MR4, ADR-0021 §11): the sign-in's provider. Not
//! configured is a warning (no browser sign-in). Configured: the settings
//! and `web.session_key_file` must load (`FG-SYS-0002`), then one fresh
//! fetch of the discovery document and the JWKS (`FG-AUTH-0015`, never
//! cached), and the provider's clock (its HTTP `Date`) within 60 s of ours
//! (beyond: warn — ID tokens would be refused as issued in the future).

use std::time::SystemTime;

use forge_core::config::Config;

use super::{fail, ok, warn, Check};
use crate::oidc::{clock_skew, settings, MAX_CLOCK_SKEW};

const NAME: &str = "auth.oidc";

pub(super) async fn oidc_check(cfg: &Config) -> Check {
    let settings = match settings(cfg) {
        Ok(Some((settings, _keys))) => settings,
        Ok(None) => {
            return warn(
                NAME,
                "not configured (no browser sign-in: the login routes answer FG-AUTH-0018)",
            )
        }
        Err(e) => return fail(NAME, e),
    };
    match forge_oidc::check_provider(&settings).await {
        Err(e) => fail(NAME, e),
        Ok((doc, keys)) => {
            let reached = format!("{} reachable, {keys} signing key(s)", doc.issuer);
            match clock_skew(doc.date.as_deref(), SystemTime::now()) {
                Some(skew) if skew > MAX_CLOCK_SKEW => warn(
                    NAME,
                    &format!(
                        "{reached}; clock skew {} s exceeds {} s (sign-ins may be refused)",
                        skew.as_secs(),
                        MAX_CLOCK_SKEW.as_secs()
                    ),
                ),
                Some(skew) => ok(NAME, &format!("{reached}; clock skew {} s", skew.as_secs())),
                None => warn(
                    NAME,
                    &format!("{reached}; no Date header (clock skew unknown)"),
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use forge_core::config::{load, Env};
    use forge_oidc::test_support::MockOp;

    use super::super::Outcome;
    use super::*;

    #[tokio::test]
    async fn not_configured_is_a_warning() {
        let cfg = load(None, &Env::from_pairs([])).expect("config");
        let check = oidc_check(&cfg).await;
        assert!(
            matches!(check.outcome, Outcome::Warn(ref m) if m.contains("not configured")),
            "auth.oidc without configuration warns"
        );
    }

    /// The provider check itself, against the mock provider: reachable,
    /// keys counted, the skew of its `Date` within bounds.
    #[tokio::test]
    async fn the_provider_check_reaches_discovery_and_jwks() {
        let op = MockOp::start().await;
        let settings = op.settings("http://127.0.0.1:8080");
        let (doc, keys) = forge_oidc::check_provider(&settings)
            .await
            .expect("the mock provider answers");
        assert_eq!(doc.issuer, settings.issuer);
        assert!(keys >= 1, "{keys}");
        if let Some(skew) = clock_skew(doc.date.as_deref(), SystemTime::now()) {
            assert!(skew <= MAX_CLOCK_SKEW, "{skew:?}");
        }
    }
}
