//! `auth.oidc` (M2 MR4, ADR-0021 §11): the sign-in's provider. Not
//! configured is a warning (no browser sign-in). Configured: the settings
//! and `web.session_key_file` must load (`FG-SYS-0002`), then one fresh
//! fetch of the discovery document and the JWKS (`FG-AUTH-0015`, never
//! cached), and the provider's clock (its HTTP `Date`) within 60 s of ours
//! (beyond: warn — ID tokens would be refused as issued in the future).
//! A plain-HTTP `web.base_url` (`web.allow_insecure_http`, trial servers
//! only) turns a passing check into a warning: sign-ins and session cookies
//! cross the network in clear text.

use std::time::SystemTime;

use forge_core::config::{Config, OidcClaimsSource};

use super::{fail, ok, warn, Check};
use crate::oidc::{clock_skew, settings, MAX_CLOCK_SKEW};

const NAME: &str = "auth.oidc";

/// With `claims_source = access_token` the employee number links accounts
/// by itself (ADR-0021 amendment 2026-11-17): a reminder that only the
/// realm's administrators may edit it.
const STAFF_ID_REMINDER: &str = concat!(
    "; claims_source=access_token: the employee number claim must be admin-only ",
    "in the Keycloak user profile (Realm settings > User profile > staff_id > ",
    "permissions: edit = admin only; runbook oidc-login, PFS Keycloak)"
);

/// The warning of a plain-HTTP `web.base_url` (`web.allow_insecure_http`).
const PLAIN_HTTP_NOTE: &str = concat!(
    "; web.allow_insecure_http: web.base_url is plain http, so sign-ins and session ",
    "cookies cross the network in clear text (trial servers only; never in production)"
);

/// Whether `web.base_url` is plain `http` off the loopback.
fn plain_http(cfg: &Config) -> bool {
    cfg.web.base_url().is_some_and(|u| u.is_plain_http())
}

pub(super) async fn oidc_check(cfg: &Config) -> Check {
    let plain = plain_http(cfg);
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
            let mut reached = format!("{} reachable, {keys} signing key(s)", doc.issuer);
            if settings.claims_source == OidcClaimsSource::AccessToken {
                reached.push_str(STAFF_ID_REMINDER);
            }
            if plain {
                reached.push_str(PLAIN_HTTP_NOTE);
            }
            match clock_skew(doc.date.as_deref(), SystemTime::now()) {
                Some(skew) if skew > MAX_CLOCK_SKEW => warn(
                    NAME,
                    &format!(
                        "{reached}; clock skew {} s exceeds {} s (sign-ins may be refused)",
                        skew.as_secs(),
                        MAX_CLOCK_SKEW.as_secs()
                    ),
                ),
                Some(skew) if plain => {
                    warn(NAME, &format!("{reached}; clock skew {} s", skew.as_secs()))
                }
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

    #[test]
    fn only_a_plain_http_base_url_is_flagged() {
        let with = |pairs: &[(&str, &str)]| {
            load(None, &Env::from_pairs(pairs.iter().copied())).expect("config")
        };
        assert!(!plain_http(&with(&[])));
        assert!(!plain_http(&with(&[(
            "FORGE_WEB_BASE_URL",
            "https://git.example.test"
        )])));
        assert!(plain_http(&with(&[
            ("FORGE_WEB_BASE_URL", "http://192.0.2.10:3306"),
            ("FORGE_WEB_ALLOW_INSECURE_HTTP", "true"),
        ])));
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

const ORG_CLAIMS: &str = "auth.oidc.org_claims";
/// A snapshot not confirmed for this long: its person has not signed in
/// (or no refresh check ran) for a week — informational only.
const ORG_STALE_SECS: i64 = 7 * 24 * 3600;

/// `auth.oidc.org_claims` (ADR-0021 amendment 2026-11-20): the snapshots of
/// the PFS org claims. Skipped unless `claims_source = access_token`; warn
/// while any snapshot is marked invalid (`FG-AUTH-0028`, runbook
/// oidc-login 「PFS の組織の claim」); otherwise the count and the oldest
/// confirmation.
pub(super) async fn org_claims_check(db: &forge_db::Db, cfg: &Config) -> Check {
    let access = cfg
        .auth
        .oidc
        .as_ref()
        .is_some_and(|o| o.claims_source == OidcClaimsSource::AccessToken);
    if !access {
        return super::skip(
            ORG_CLAIMS,
            "auth.oidc.claims_source is not access_token: no org claims are read",
        );
    }
    match forge_db::org_profiles::health(db).await {
        Ok(h) => org_claims_line(&h),
        Err(e) => fail(ORG_CLAIMS, e),
    }
}

fn org_claims_line(h: &forge_db::org_profiles::OrgHealth) -> Check {
    let oldest = h.oldest_confirmed_at.as_deref().unwrap_or("none");
    let detail = format!(
        "{} snapshot(s), {} invalid; oldest confirmed_at {oldest}",
        h.profiles, h.invalid
    );
    if h.invalid > 0 {
        return warn(
            ORG_CLAIMS,
            &format!("{detail} (FG-AUTH-0028: runbook oidc-login, PFS の組織の claim)"),
        );
    }
    if h.oldest_age_secs.is_some_and(|s| s > ORG_STALE_SECS) {
        return ok(
            ORG_CLAIMS,
            &format!("{detail} (older than 7 days: that person has not signed in since)"),
        );
    }
    ok(ORG_CLAIMS, &detail)
}

#[cfg(test)]
mod org_claims_tests {
    use forge_db::org_profiles::OrgHealth;

    use super::super::Outcome;
    use super::*;

    #[test]
    fn invalid_snapshots_warn_and_the_rest_is_ok() {
        let quiet = org_claims_line(&OrgHealth {
            profiles: 3,
            invalid: 0,
            oldest_confirmed_at: Some("2026-11-20T00:00:00Z".to_owned()),
            oldest_age_secs: Some(60),
        });
        assert!(matches!(quiet.outcome, Outcome::Ok(_)));
        let marked = org_claims_line(&OrgHealth {
            profiles: 3,
            invalid: 1,
            oldest_confirmed_at: None,
            oldest_age_secs: None,
        });
        let Outcome::Warn(text) = marked.outcome else {
            panic!("an invalid snapshot warns");
        };
        assert!(text.contains("FG-AUTH-0028"), "{text}");
    }
}

const ADMIN_ROLE: &str = "auth.oidc.admin_role";

/// `auth.oidc.admin_role` (ADR-0021 amendment 2026-11-24): whether the
/// client role grants and revokes the administrator flag, and how many
/// active human administrators each source manages. Off with
/// administrators the role once granted warns: nothing revokes their flag
/// any more (`forge admin user admin-source`, runbook oidc-login).
pub(super) async fn admin_role_check(db: &forge_db::Db, cfg: &Config) -> Check {
    let Some(oidc) = cfg.auth.oidc.as_ref() else {
        return super::skip(ADMIN_ROLE, "auth.oidc is not configured");
    };
    match forge_db::users::admin_sources(db).await {
        Ok(counts) => admin_role_line(oidc.admin_role.as_deref(), &oidc.client_id, counts),
        Err(e) => fail(ADMIN_ROLE, e),
    }
}

fn admin_role_line(
    role: Option<&str>,
    client: &str,
    counts: forge_db::users::AdminSources,
) -> Check {
    let held = format!(
        "{} administrator(s) from the role, {} from the CLI (break-glass)",
        counts.oidc, counts.cli
    );
    match role {
        Some(role) if counts.cli == 0 => warn(
            ADMIN_ROLE,
            &format!(
                "on: client role {role} of {client}; {held}: no break-glass administrator \
                 (forge admin user admin-source <user> --source cli; runbook oidc-login)"
            ),
        ),
        Some(role) => ok(
            ADMIN_ROLE,
            &format!(
                "on: client role {role} of {client} grants and revokes the administrator flag \
                 at each sign-in and refresh check; {held}"
            ),
        ),
        None if counts.oidc > 0 => warn(
            ADMIN_ROLE,
            &format!(
                "off, but {held}: nothing revokes the role's grants any more (set \
                 auth.oidc.admin_role again, or `forge admin user admin-source <user> --source \
                 cli`; runbook oidc-login)"
            ),
        ),
        None => ok(
            ADMIN_ROLE,
            &format!("off: the CLI alone sets administrators; {held}"),
        ),
    }
}

#[cfg(test)]
mod admin_role_tests {
    use forge_db::users::AdminSources;

    use super::super::Outcome;
    use super::*;

    #[test]
    fn the_admin_role_line_says_on_off_and_warns_on_orphaned_grants() {
        let on = admin_role_line(
            Some("admin"),
            "gitstation",
            AdminSources { cli: 1, oidc: 2 },
        );
        let Outcome::Ok(text) = on.outcome else {
            panic!("on is ok");
        };
        assert!(
            text.starts_with("on: client role admin of gitstation"),
            "{text}"
        );
        assert!(
            text.contains("2 administrator(s) from the role, 1 from the CLI"),
            "{text}"
        );
        let glassless = admin_role_line(
            Some("admin"),
            "gitstation",
            AdminSources { cli: 0, oidc: 2 },
        );
        assert!(matches!(glassless.outcome, Outcome::Warn(ref t) if t.contains("no break-glass")));
        let off = admin_role_line(None, "gitstation", AdminSources { cli: 1, oidc: 0 });
        assert!(matches!(off.outcome, Outcome::Ok(ref t) if t.starts_with("off")));
        let orphaned = admin_role_line(None, "gitstation", AdminSources { cli: 1, oidc: 1 });
        assert!(matches!(orphaned.outcome, Outcome::Warn(ref t) if t.contains("admin-source")));
    }
}
