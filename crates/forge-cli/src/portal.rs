//! The PFS portal's wiring (ADR-0036) for `forge serve`, `forge worker` and
//! `forge doctor`:
//!
//! - [`news_policy`]: the database stages portal news only with
//!   `portal.enabled` and `portal.news_enabled` (`Db::with_portal_news`);
//!   off, nothing is written to the news outbox.
//! - [`widgets`]: the widget routes' authenticator (forge-oidc), with
//!   `portal.enabled` and `auth.oidc`; without either the routes answer
//!   `FG-AUTH-0026 reason=portal_disabled`.
//! - [`news_sender`]: the `portal.news_send` handler of `forge worker`
//!   (the client-credentials token of `auth.oidc.client_id`, the registry
//!   and its CA); news on without `auth.oidc` stops the start
//!   (`FG-SYS-0002 config_key=portal.news_enabled reason=needs_auth_oidc`).

use std::sync::Arc;

use forge_core::config::Config;
use forge_core::portal::PortalAuthenticator;
use forge_db::portal::NewsPolicy;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::portal_news::{HttpNewsTransport, NewsSender};
use forge_oidc::{Oidc, OidcSettings, PortalWidgets, ServiceTokens};

/// The news policy of the database, or `None` when news is off.
pub(crate) fn news_policy(cfg: &Config) -> Result<Option<NewsPolicy>, ForgeError> {
    if !cfg.portal.news_on() {
        return Ok(None);
    }
    NewsPolicy::new(cfg.portal.max_attempts, cfg.portal.timeout).map(Some)
}

/// The widget routes' authenticator, or `None` (`portal.enabled` off, or
/// no sign-in to verify with).
pub(crate) fn widgets(
    cfg: &Config,
    oidc: Option<&Arc<Oidc>>,
) -> Option<Arc<dyn PortalAuthenticator>> {
    if !cfg.portal.enabled {
        return None;
    }
    let oidc = oidc?;
    Some(Arc::new(PortalWidgets::new(
        Arc::clone(oidc),
        &cfg.portal.client_id,
    )))
}

/// The news sender of `forge worker`, or `None` when news is off.
pub(crate) fn news_sender(cfg: &Config, db: &Db) -> Result<Option<NewsSender>, ForgeError> {
    if !cfg.portal.news_on() {
        return Ok(None);
    }
    let needs = |key: &'static str, reason: &'static str| {
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", key)
            .with_context("reason", reason)
    };
    let oidc = cfg
        .auth
        .oidc
        .as_ref()
        .ok_or_else(|| needs("portal.news_enabled", "needs_auth_oidc"))?;
    let registry = cfg
        .portal
        .registry_url
        .as_deref()
        .ok_or_else(|| needs("portal.registry_url", "missing"))?;
    // The base URL and the session lifetimes do not matter to the token.
    let settings =
        OidcSettings::from_config(oidc, "", cfg.web.session_idle, cfg.web.session_absolute)?;
    let tokens = Arc::new(ServiceTokens::new(&settings)?);
    let ca = cfg.portal.ca_pem()?;
    let transport = Arc::new(HttpNewsTransport::new(ca.as_deref(), cfg.portal.timeout)?);
    NewsSender::new(db, registry, tokens, transport).map(Some)
}

#[cfg(test)]
mod tests {
    use forge_core::config::{load, Env};

    use super::*;

    #[test]
    fn the_portal_is_off_without_its_keys() {
        let cfg = load(None, &Env::from_pairs([])).expect("config");
        assert!(news_policy(&cfg).expect("policy").is_none());
        assert!(widgets(&cfg, None).is_none());
    }

    #[test]
    fn news_follows_the_configuration() {
        let cfg = load(
            None,
            &Env::from_pairs([
                ("FORGE_PORTAL_ENABLED", "true"),
                ("FORGE_PORTAL_NEWS_ENABLED", "true"),
                ("FORGE_PORTAL_REGISTRY_URL", "http://127.0.0.1:4100"),
                ("FORGE_PORTAL_MAX_ATTEMPTS", "3"),
            ]),
        )
        .expect("config");
        let policy = news_policy(&cfg).expect("policy").expect("on");
        assert_eq!(policy.max_attempts.get(), 3);
        // Widgets need the sign-in to verify with.
        assert!(widgets(&cfg, None).is_none());
    }
}
